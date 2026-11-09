//! Restricted V8 worker process that runs one program.

use std::cell::RefCell;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use deno_ast::{EmitOptions, MediaType, ParseParams, TranspileModuleOptions, TranspileOptions};
use deno_core::{JsRuntime, ModuleSpecifier, OpState, RuntimeOptions, op2, v8};
use deno_error::JsErrorBox;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};

const MAX_HOST_FRAME: usize = 1024 * 1024;
const MAX_GUEST_FRAME: usize = 64 * 1024;
const MAX_CODE: usize = 64 * 1024;
const MIB: usize = 1024 * 1024;
/// Exit code of a worker that exhausted its memory, after its `memory_limit` frame.
pub(crate) const MEMORY_LIMIT_EXIT_CODE: i32 = 75;

/// Set once memory is exhausted; the isolate thread then emits nothing further.
static MEMORY_EXHAUSTED: AtomicBool = AtomicBool::new(false);

type Input = Rc<Mutex<mpsc::UnboundedReceiver<String>>>;

/// Failure to initialize or execute the embedded worker.
#[derive(Debug)]
pub struct EmbeddedWorkerError(String);

impl fmt::Display for EmbeddedWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EmbeddedWorkerError {}

impl From<io::Error> for EmbeddedWorkerError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

#[op2(fast)]
fn op_guest_write(#[string] text: &str) -> Result<(), JsErrorBox> {
    halt_if_exhausted();
    write_frames(text)
}

/// After memory exhaustion only the watchdog writes and exits; a guest still
/// running in the granted headroom must not report calls or output.
fn halt_if_exhausted() {
    if MEMORY_EXHAUSTED.load(Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
}

fn write_frames(text: &str) -> Result<(), JsErrorBox> {
    for line in text.split_terminator('\n') {
        if line.len() > MAX_GUEST_FRAME {
            return Err(JsErrorBox::generic("Guest frame exceeds 64 KiB"));
        }
    }
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(JsErrorBox::from_err)
}

#[op2(fast)]
fn op_guest_log(#[string] text: &str) -> Result<(), JsErrorBox> {
    if text.len() > MAX_GUEST_FRAME {
        return Err(JsErrorBox::generic("Log message exceeds 64 KiB"));
    }
    io::stderr()
        .lock()
        .write_all(text.as_bytes())
        .map_err(JsErrorBox::from_err)
}

#[op2]
#[string]
async fn op_guest_receive(state: Rc<RefCell<OpState>>) -> Result<String, JsErrorBox> {
    let input = state.borrow().borrow::<Input>().clone();
    input
        .lock()
        .await
        .recv()
        .await
        .ok_or_else(|| JsErrorBox::generic("Host input closed"))
}

#[op2(fast)]
fn op_guest_finish(#[string] frame: &str, failed: bool) -> Result<(), JsErrorBox> {
    halt_if_exhausted();
    write_frames(&format!("{frame}\n"))?;
    std::process::exit(i32::from(failed));
}

deno_core::extension!(
    guest_protocol,
    ops = [
        op_guest_write,
        op_guest_log,
        op_guest_receive,
        op_guest_finish
    ]
);

fn report_error(message: &str) {
    let message: String = message.chars().take(4096).collect();
    let frame = serde_json::json!({ "type": "error", "message": message });
    let _ = writeln!(io::stdout().lock(), "{frame}");
}

fn read_input(send: mpsc::UnboundedSender<String>) {
    let mut input = io::stdin().lock();
    let mut frame = Vec::new();
    let result: io::Result<()> = (|| loop {
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            // EOF must stop even a JavaScript infinite loop. Never wait for the
            // isolate thread or a blocked input queue to observe parent death.
            std::process::exit(1);
        }
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(bytes.len());
        if frame.len() + count > MAX_HOST_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Input frame exceeds 1 MiB",
            ));
        }
        frame.extend_from_slice(&bytes[..count]);
        input.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            if frame.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Empty input frame",
                ));
            }
            let text = String::from_utf8(std::mem::take(&mut frame))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            // The parent bounds admitted calls and aggregate reply bytes. A
            // nonblocking queue preserves bursts and independent EOF detection.
            send.send(text)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
    })();
    if let Err(error) = result {
        report_error(&error.to_string());
    }
    std::process::exit(1);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFrame {
    #[serde(rename = "type")]
    kind: String,
    code: String,
    /// V8 heap budget.
    heap_bytes: u64,
    /// Process data limit to apply before V8 starts, or none.
    process_data_bytes: Option<u64>,
}

/// Apply the process data limit (Linux `RLIMIT_DATA`) to this process.
#[cfg(target_os = "linux")]
fn limit_process_data(bytes: Option<u64>) -> Result<(), EmbeddedWorkerError> {
    // Make the kernel's OOM killer prefer this worker over its controller. Raising
    // one's own score needs no privilege; without /proc it cannot be done at all.
    if let Err(error) = std::fs::write("/proc/self/oom_score_adj", "1000") {
        let _ = writeln!(
            io::stderr(),
            "codemode worker: oom_score_adj not raised: {error}"
        );
    }
    if let Some(bytes) = bytes {
        rlimit::setrlimit(rlimit::Resource::DATA, bytes, bytes).map_err(|error| {
            EmbeddedWorkerError(format!("Cannot apply the process data limit: {error}"))
        })?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn limit_process_data(bytes: Option<u64>) -> Result<(), EmbeddedWorkerError> {
    match bytes {
        Some(_) => Err(EmbeddedWorkerError(
            "A process data limit is enforced only on Linux".into(),
        )),
        None => Ok(()),
    }
}

/// Write the `memory_limit` frame and exit with the reserved code once the
/// near-heap-limit callback reports exhaustion. Exiting from this thread does
/// not depend on JavaScript reaching a safe point.
fn start_memory_watchdog() -> Result<std::sync::mpsc::Sender<usize>, EmbeddedWorkerError> {
    let (send, receive) = std::sync::mpsc::channel::<usize>();
    std::thread::Builder::new()
        .name("memory-watchdog".into())
        .spawn(move || {
            let Ok(limit) = receive.recv() else { return };
            let frame = serde_json::json!({ "type": "memory_limit", "heap_limit_bytes": limit });
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{frame}");
            let _ = stdout.flush();
            std::process::exit(MEMORY_LIMIT_EXIT_CODE);
        })?;
    Ok(send)
}

/// Create the isolate under `heap_bytes`, with the exhaustion path installed.
fn create_runtime(heap_bytes: u64) -> Result<JsRuntime, EmbeddedWorkerError> {
    let heap = usize::try_from(heap_bytes)
        .map_err(|_| EmbeddedWorkerError("Heap budget exceeds the address space".into()))?;
    let watchdog = start_memory_watchdog()?;
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![guest_protocol::init()],
        create_params: Some(v8::CreateParams::default().heap_limits(0, heap)),
        // The default loader rejects every import; no filesystem, network,
        // environment, process, or FFI extensions are installed.
        ..Default::default()
    });
    // V8 also calls this when an external allocation, such as an ArrayBuffer
    // backing store, fails under the process data limit; that is exhaustion too.
    runtime.add_near_heap_limit_callback(move |current, _initial| {
        if MEMORY_EXHAUSTED.swap(true, Ordering::SeqCst) {
            // The watchdog is already exiting; never let V8 abort the process first.
            loop {
                std::thread::park();
            }
        }
        let _ = watchdog.send(current);
        // One bounded grant lets V8 finish the allocation in progress.
        current.saturating_add((current / 4).clamp(16 * MIB, 64 * MIB))
    });
    Ok(runtime)
}

async fn execute_worker() -> Result<(), EmbeddedWorkerError> {
    let (send, receive) = mpsc::unbounded_channel();
    let input = Rc::new(Mutex::new(receive));
    std::thread::Builder::new()
        .name("guest-input".into())
        .spawn(move || read_input(send))?;
    // Limits arrive in the run frame and apply before V8 creates the isolate.
    let frame = input
        .lock()
        .await
        .recv()
        .await
        .ok_or_else(|| EmbeddedWorkerError("Missing run frame".into()))?;
    let frame: RunFrame = serde_json::from_str(&frame).map_err(|error| {
        EmbeddedWorkerError(format!(
            "Expected run frame with string code and limits: {error}"
        ))
    })?;
    if frame.kind != "run" {
        return Err(EmbeddedWorkerError(
            "Expected run frame with string code and limits".into(),
        ));
    }
    if frame.code.len() > MAX_CODE {
        return Err(EmbeddedWorkerError("Source exceeds 64 KiB".into()));
    }
    limit_process_data(frame.process_data_bytes)?;
    let mut runtime = create_runtime(frame.heap_bytes)?;
    runtime.op_state().borrow_mut().put(input.clone());
    runtime
        .execute_script("codemode:bootstrap", include_str!("embedded_guest.js"))
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    let specifier = ModuleSpecifier::parse("codemode:program")
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    let source = format!(
        "globalThis.__codemode_run(async () => {{\n{}\n}});",
        frame.code
    );
    let parsed = deno_ast::parse_module(ParseParams {
        specifier: specifier.clone(),
        text: source.into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    let code = parsed
        .transpile(
            &TranspileOptions::default(),
            &TranspileModuleOptions::default(),
            &EmitOptions::default(),
        )
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?
        .into_source()
        .text;
    let module = runtime
        .load_main_es_module_from_code(&specifier, code)
        .await
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    let evaluation = runtime.mod_evaluate(module);
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    evaluation
        .await
        .map_err(|error| EmbeddedWorkerError(error.to_string()))?;
    Err(EmbeddedWorkerError("Guest ended without output".into()))
}

/// Run one restricted embedded guest using bounded newline-delimited stdio.
///
/// Call this on the process's main thread before starting another async runtime.
/// The process must be dedicated to the worker; its stdin and stdout carry the
/// guest protocol and stdin closure terminates it independently of JavaScript.
/// Normal completion writes a final frame and exits this dedicated process.
///
/// The first input frame names the V8 heap budget and, on Linux, a data-segment
/// limit (`RLIMIT_DATA`) that the worker applies to itself before V8 starts. On
/// Linux the worker also raises its own `oom_score_adj` so the kernel's OOM killer
/// prefers it over its controller. When V8 nears the heap budget, or an allocation
/// fails under the data limit, the worker writes a `memory_limit` frame and exits
/// with the reserved code 75.
///
/// # Errors
/// Returns an error when initialization, protocol input, or JavaScript fails.
pub fn run_embedded_worker() -> Result<(), EmbeddedWorkerError> {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(execute_worker());
    if let Err(error) = &result {
        report_error(&error.to_string());
    }
    result
}
