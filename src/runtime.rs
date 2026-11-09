use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

/// Largest frame from the guest (source and call frames), including its LF.
pub(crate) const FRAME_BYTES: usize = 64 * 1024;
/// Largest reply frame to the guest, including its LF; the launcher accepts up to 1 MiB.
pub(crate) const REPLY_FRAME_BYTES: usize = 1024 * 1024;
/// Version of the serialized [`RunReport`] format written by this build.
pub const REPORT_SCHEMA_VERSION: u32 = 3;
/// Provider facts kept per call; later facts for that call are counted as dropped.
const MAX_FACTS_PER_CALL: usize = 8;
/// Provider facts buffered between the provider and the run loop.
const FACT_CHANNEL_CAPACITY: usize = 1024;
const MAX_GUEST_ID: u64 = 9_007_199_254_740_991;
static LAUNCHER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Passive limits and executable selection for one run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Absolute wall-clock budget, including process startup and provider calls.
    pub timeout_ms: u64,
    /// Maximum total admitted calls, including calls rejected before dispatch. The
    /// first request beyond it is recorded as rejected with code `call_limit` and
    /// ends the run, so a report holds at most `max_calls + 1` calls.
    pub max_calls: usize,
    /// Maximum provider futures executing simultaneously.
    pub max_concurrency: usize,
    /// Cumulative serialized response and final-output bytes, not a process memory limit.
    /// Guest responses include their protocol envelopes; native results do not.
    pub max_output_bytes: usize,
    /// Deno executable; use an absolute path when executable selection must be fixed.
    pub deno: PathBuf,
    /// Program engine and, for embedding, the dedicated worker executable.
    /// Native [`run_calls`] does not use this setting.
    pub backend: GuestBackend,
    /// Tool names authorized by this host, independently of guest requests.
    pub allowed_tools: Vec<String>,
    /// How queued calls start when provider slots are free.
    pub dispatch: DispatchPolicy,
}

/// Engine used by an isolated arbitrary-program guest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuestBackend {
    /// External Deno executable selected by [`RunConfig::deno`].
    #[default]
    Deno,
    /// The `codemode` executable built with the `embedded-v8` feature.
    ///
    /// Library consumers must supply the worker path explicitly; their own
    /// executable need not implement codemode's internal worker entry point.
    EmbeddedV8 {
        /// Dedicated worker executable, preferably an absolute path.
        executable: PathBuf,
    },
}

/// When an admitted call may start, given the concurrency cap.
///
/// A sequential baseline is either policy with `max_concurrency` set to 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPolicy {
    /// Start a queued call as soon as any active call finishes.
    #[default]
    Rolling,
    /// Start up to the cap only when no call is active, then wait for that whole
    /// batch. A batch includes every complete call frame the host has received
    /// when it forms. Calls submitted in one synchronous guest turn arrive in one
    /// write and share a batch, up to the cap; calls from later turns may not.
    Batch,
}

/// The host-chosen limits a run executed under, recorded with its trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    /// Absolute wall-clock budget in milliseconds.
    pub timeout_ms: u64,
    /// Maximum admitted calls.
    pub max_calls: usize,
    /// Maximum simultaneously executing provider calls.
    pub max_concurrency: usize,
    /// Shared serialized response and output byte allowance.
    pub max_output_bytes: usize,
    /// Dispatch policy under the concurrency cap.
    pub dispatch: DispatchPolicy,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_calls: 32,
            max_concurrency: 4,
            max_output_bytes: 256 * 1024,
            deno: PathBuf::from("deno"),
            backend: GuestBackend::Deno,
            allowed_tools: ["mock.echo", "mock.wait", "mock.fail"]
                .map(String::from)
                .to_vec(),
            dispatch: DispatchPolicy::Rolling,
        }
    }
}

/// A clonable cancellation signal shared with a run and its providers.
#[derive(Debug, Clone)]
pub struct RunCancellation(watch::Sender<bool>);

impl Default for RunCancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl RunCancellation {
    /// Request cancellation; later calls remain cancelled as well.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until cancellation is requested, including an earlier request.
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A finite host-owned request, without guest correlation or transport metadata.
///
/// [`run_calls`] borrows requests and applies the run's allowance before copying
/// them. Each serialized request must fit in 64 KiB, including its tool name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeCall {
    /// Tool name to check against the host's allowance.
    pub tool: String,
    /// JSON arguments, validated before provider dispatch.
    pub arguments: Value,
}

/// The execution source whose work and lifecycle the report records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionBackend {
    /// Host-owned finite calls, with no JavaScript guest.
    Native,
    /// A restricted Deno subprocess; also the default for historical reports.
    #[default]
    Deno,
    /// A restricted subprocess embedding V8.
    EmbeddedV8,
}

/// Host identity and validated input passed to a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Sequential host-assigned identity, independent of guest correlation tokens.
    pub id: u64,
    /// Authorized tool name.
    pub tool: String,
    /// JSON arguments; mock-tool argument contracts are checked before dispatch.
    pub arguments: Value,
}

/// The absolute budget and cancellation signal inherited by a provider.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// Absolute monotonic deadline shared by the entire run.
    pub deadline: Instant,
    /// Cooperative cancellation signal; provider side effects cannot be rolled back.
    pub cancellation: RunCancellation,
    /// Records facts the provider reports about its calls, such as a permission decision.
    pub facts: FactSink,
}

/// A provider-reported milestone for one call. Rust records it as the provider's
/// claim, separately from what Rust observed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFactKind {
    /// The provider received the call and took responsibility for it.
    Received,
    /// The provider began waiting for a permission or approval decision.
    PermissionRequested,
    /// The provider's permission check allowed the call.
    PermissionGranted,
    /// The provider's permission check refused the call.
    PermissionDenied,
    /// The provider invoked the tool implementation; effects may occur from here.
    ExecutorStarted,
}

/// A provider-reported fact, stamped with the time Rust received it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderFact {
    /// Host-assigned call identity the provider named.
    pub call_id: u64,
    /// What the provider reported.
    pub kind: ProviderFactKind,
    /// Rust monotonic milliseconds since run start when the fact arrived.
    pub received_ms: u64,
    /// The provider's own timestamp, in its own clock; not comparable with Rust times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_ms: Option<f64>,
}

/// A fact on its way to the run loop, stamped when the provider reported it.
#[derive(Debug)]
struct PendingFact {
    call_id: u64,
    kind: ProviderFactKind,
    provider_ms: Option<f64>,
    at: Instant,
}

/// Where a provider sends [`ProviderFact`]s; reporting never blocks.
///
/// Facts for calls that are not running, beyond a small per-call limit, or that
/// arrive while the buffer is full are dropped and counted in the report.
#[derive(Debug, Clone)]
pub struct FactSink {
    sender: mpsc::Sender<PendingFact>,
    dropped: Arc<AtomicU64>,
}

impl FactSink {
    /// A sink whose facts go nowhere, for calling a provider outside a run.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::new().0
    }

    fn new() -> (Self, mpsc::Receiver<PendingFact>) {
        let (sender, receiver) = mpsc::channel(FACT_CHANNEL_CAPACITY);
        let sink = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        (sink, receiver)
    }

    /// Report `kind` for call `call_id`, with an optional timestamp from the provider's clock.
    pub fn report(&self, call_id: u64, kind: ProviderFactKind, provider_ms: Option<f64>) {
        let provider_ms = provider_ms.filter(|value| value.is_finite());
        // Stamped now, so a busy run loop does not delay the recorded arrival time.
        let fact = PendingFact {
            call_id,
            kind,
            provider_ms,
            at: Instant::now(),
        };
        if self.sender.try_send(fact).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Executes host-authorized calls; implementations must yield instead of blocking the executor.
pub trait ToolProvider: Send + Sync + 'static {
    /// Execute once, without automatic retry.
    ///
    /// # Errors
    /// Return a [`ProviderError`] when the provider reports a failure. Dropping the
    /// future does not prove that an external side effect did not occur.
    fn execute(
        &self,
        call: ToolCall,
        context: CallContext,
    ) -> impl Future<Output = Result<Value, ProviderError>> + Send;
}

/// A provider-reported error, retaining its origin in the host trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderError {
    /// Machine-readable provider error code.
    pub code: String,
    /// Human-readable provider explanation.
    pub message: String,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ProviderError {}

/// Local deterministic tools for exercising execution without external credentials.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockProvider;

impl ToolProvider for MockProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        match call.tool.as_str() {
            "mock.echo" => Ok(call.arguments),
            "mock.wait" => {
                let ms = call.arguments["ms"]
                    .as_u64()
                    .ok_or_else(|| invalid_arguments("ms must be a nonnegative integer"))?;
                tokio::select! {
                    () = time::sleep(Duration::from_millis(ms)) => Ok(call.arguments.get("value").cloned().unwrap_or(Value::Null)),
                    () = time::sleep_until(context.deadline) => Err(ProviderError { code: "deadline".into(), message: "run deadline reached".into() }),
                    () = context.cancellation.cancelled() => Err(ProviderError { code: "cancelled".into(), message: "run cancelled".into() }),
                }
            }
            "mock.fail" => Err(ProviderError {
                code: "mock_failure".into(),
                message: call
                    .arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("mock provider failed")
                    .into(),
            }),
            _ => Err(ProviderError {
                code: "unknown_tool".into(),
                message: "provider does not implement this tool".into(),
            }),
        }
    }
}

/// The terminal reason recorded by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The input completed and all dispatched calls were observed.
    Success,
    /// Guest execution failed.
    GuestError,
    /// A dispatched provider task terminated without returning a result.
    ProviderTaskError,
    /// Input attempted more calls than allowed; the first excess call is recorded
    /// as rejected with code `call_limit`.
    CallLimit,
    /// Serialized responses or final output exceeded the byte budget.
    OutputLimit,
    /// The absolute deadline elapsed.
    Deadline,
    /// The caller requested cancellation.
    Cancelled,
    /// Child stdout violated the untrusted input protocol.
    ProtocolError,
}

impl RunStatus {
    /// Whether the run completed successfully.
    #[must_use]
    pub fn is_success(self) -> bool {
        self == Self::Success
    }

    /// The status name as serialized in a report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::GuestError => "guest_error",
            Self::ProviderTaskError => "provider_task_error",
            Self::CallLimit => "call_limit",
            Self::OutputLimit => "output_limit",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::ProtocolError => "protocol_error",
        }
    }

    /// CLI exit code: success 0, cancellation 130, deadline 124, other failure 1.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Cancelled => 130,
            Self::Deadline => 124,
            _ => 1,
        }
    }
}

/// Observed outcome, distinguishing rejection from uncertain effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CallOutcome {
    /// Queued; only appears while the host is executing.
    Queued,
    /// Dispatched; only appears while the host is executing.
    Running,
    /// The provider returned a value observed by the host.
    Succeeded {
        /// Returned JSON value.
        value: Value,
    },
    /// The provider returned an error observed by the host.
    ProviderFailed {
        /// Provider error, separate from host authorization errors.
        error: ProviderError,
    },
    /// Host policy or argument validation prevented dispatch.
    Rejected {
        /// Host validation failure.
        error: ProviderError,
    },
    /// The run ended before this admitted call was dispatched.
    CancelledBeforeDispatch,
    /// A provider result was observed but omitted because serialization exceeded a limit.
    ResultTooLarge,
    /// A dispatched call had no observed result; external effects remain unknown.
    Unknown,
}

/// One host-assigned call and its observed lifecycle.
///
/// Times are Rust monotonic milliseconds since run start, floored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// Validated request and trusted identity.
    pub call: ToolCall,
    /// Untrusted correlation token used only for guest replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_id: Option<u64>,
    /// Observed result or precise reason no result is known.
    pub outcome: CallOutcome,
    /// Admission time; absent in schema version 1 reports.
    #[serde(default)]
    pub admitted_ms: Option<u64>,
    /// Dispatch time since run start; absent for calls never dispatched.
    pub started_ms: Option<u64>,
    /// Completion or termination time since run start.
    pub finished_ms: Option<u64>,
    /// When the reply frame was completely written to the guest's input. The guest
    /// may not have read it; absent when no reply was delivered.
    #[serde(default)]
    pub delivered_ms: Option<u64>,
    /// Serialized size of the submitted arguments.
    #[serde(default)]
    pub argument_bytes: u64,
    /// Serialized size of the observed value or error, including omitted ones.
    #[serde(default)]
    pub result_bytes: Option<u64>,
    /// Bytes of the reply frame queued for the guest.
    #[serde(default)]
    pub reply_bytes: Option<u64>,
}

/// A host-created lifecycle event, never accepted from child stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    /// Sequential host-created event identity.
    pub id: u64,
    /// Related trusted call identity, if any.
    pub call_id: Option<u64>,
    /// Event name such as admitted, dispatched, succeeded, or run_finished.
    pub kind: String,
    /// Monotonic time elapsed since run start.
    pub at_ms: u64,
    /// Why the event happened, for example the run status that left a call unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Which clock stamped the report's Rust times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockInfo {
    /// What zero means: `run_start`, before submission or guest process startup.
    pub origin: String,
    /// The clock: `rust_monotonic`, the host's monotonic clock read by Rust.
    pub source: String,
    /// Unit and rounding: `ms_floor`, whole milliseconds rounded down.
    pub unit: String,
}

impl Default for ClockInfo {
    fn default() -> Self {
        Self {
            origin: "run_start".into(),
            source: "rust_monotonic".into(),
            unit: "ms_floor".into(),
        }
    }
}

/// Serialized byte accounting observed by Rust.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteTotals {
    /// Bytes of complete frames read from the guest.
    pub from_guest: u64,
    /// Bytes written to the guest's input, including the initial source frame.
    pub to_guest: u64,
    /// Serialized arguments, observed values and errors, and output kept in this report.
    pub retained: u64,
    /// Serialized observed values or errors that were not kept, such as oversized results.
    pub omitted: u64,
}

/// Serializable source, output, and host-observed trace for one execution.
///
/// `events`, call times, and `bytes` are Rust observations. `provider_facts` are
/// claims a provider reported; a missing fact does not show that a call did not run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Version of this serialized report format.
    pub schema_version: u32,
    /// Execution source; absent historical fields deserialize as Deno.
    #[serde(default)]
    pub backend: ExecutionBackend,
    /// Exact executed TypeScript source; empty for native execution.
    pub code: String,
    /// Host-selected terminal status.
    pub status: RunStatus,
    /// Limits and dispatch policy chosen by the host for this run.
    pub limits: RunLimits,
    /// Guest-returned JSON value, when observed within limits; omitted from JSON when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Calls in admission order, using host-assigned identities.
    pub calls: Vec<CallRecord>,
    /// Host-generated events in observation order.
    pub events: Vec<RunEvent>,
    /// Facts the provider reported about dispatched calls, in arrival order.
    #[serde(default)]
    pub provider_facts: Vec<ProviderFact>,
    /// Provider facts discarded: over the per-call limit, for calls not running, or unbuffered.
    #[serde(default)]
    pub provider_facts_dropped: u64,
    /// Monotonic elapsed wall-clock milliseconds.
    pub elapsed_ms: u64,
    /// The clock behind every Rust time in this report.
    #[serde(default)]
    pub clock: ClockInfo,
    /// Bytes moved, retained, and omitted, as Rust observed them.
    #[serde(default)]
    pub bytes: ByteTotals,
    /// How long the guest had been running before the run started, when it was
    /// started ahead of time with [`PreparedGuest`]; absent when the run started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prewarmed_ms: Option<u64>,
    /// Bridge stdio accounting, present when the run was served over the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<BridgeTotals>,
    /// A bounded explanation of the terminal status; child stderr is discarded.
    pub diagnostic: Option<String>,
}

/// Bytes the bridge exchanged with its host, excluding the final report frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeTotals {
    /// Bytes of host frames read, including the start frame.
    pub from_host: u64,
    /// Bytes of call frames written to the host.
    pub to_host: u64,
}

/// Failure to configure execution, launch a guest, or communicate with it.
#[derive(Debug)]
pub enum RunError {
    /// Invalid host limits, oversized source, or an oversized native request.
    Config(String),
    /// Launcher creation, process spawning, or pipe I/O failed.
    Io(io::Error),
}
impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(formatter, "invalid run configuration: {message}"),
            Self::Io(error) => write!(formatter, "runtime I/O failed: {error}"),
        }
    }
}
impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Config(_) => None,
        }
    }
}
impl From<io::Error> for RunError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum GuestFrame {
    Call {
        id: u64,
        tool: String,
        arguments: Value,
    },
    Output {
        value: Value,
    },
    Error {
        message: String,
    },
}

#[derive(Serialize)]
struct Reply<'a> {
    #[serde(rename = "type")]
    frame_type: &'static str,
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ProviderError>,
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("serialized value exceeds byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Serialized JSON length without allocating the output.
fn json_len(value: &impl Serialize) -> u64 {
    struct Count(u64);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Option<Vec<u8>> {
    let mut writer = BoundedJson {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.bytes)
}

fn resolve_executable(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    let current = std::env::current_dir()?;
    if path.components().count() > 1 {
        return Ok(current.join(path));
    }
    if let Some(search_path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&search_path) {
            let candidate = directory.join(path);
            if candidate.is_file() {
                return Ok(if candidate.is_absolute() {
                    candidate
                } else {
                    current.join(candidate)
                });
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Deno executable was not found on the host PATH",
    ))
}

struct Launcher(PathBuf);
impl Launcher {
    fn create() -> io::Result<Self> {
        loop {
            let sequence = LAUNCHER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("codemode-{}-{sequence}.ts", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let launcher = Self(path);
                    file.write_all(include_str!("guest.ts").as_bytes())?;
                    return Ok(launcher);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
impl Drop for Launcher {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whole milliseconds, saturating.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
fn elapsed(start: Instant) -> u64 {
    millis(start.elapsed())
}
fn event(report: &mut RunReport, start: Instant, call_id: Option<u64>, kind: &str) {
    event_because(report, start, call_id, kind, None);
}
fn event_because(
    report: &mut RunReport,
    start: Instant,
    call_id: Option<u64>,
    kind: &str,
    reason: Option<&str>,
) {
    report.events.push(RunEvent {
        id: report.events.len() as u64 + 1,
        call_id,
        kind: kind.into(),
        at_ms: elapsed(start),
        reason: reason.map(String::from),
    });
}
/// Record a cancellation or deadline that stops the run, as Rust observed it.
fn stop(report: &mut RunReport, start: Instant, status: RunStatus) {
    report.status = status;
    let kind = if status == RunStatus::Deadline {
        "deadline_reached"
    } else {
        "cancellation_observed"
    };
    event(report, start, None, kind);
}

/// Keep a provider fact only for a running call and within the per-call limit.
fn record_fact(
    report: &mut RunReport,
    start: Instant,
    counts: &mut HashMap<u64, usize>,
    fact: PendingFact,
) {
    let running = usize::try_from(fact.call_id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .and_then(|index| report.calls.get(index))
        .is_some_and(|record| record.outcome == CallOutcome::Running);
    // Only running calls get a counter, so the map is bounded by the call allowance.
    let count = running.then(|| counts.entry(fact.call_id).or_default());
    let Some(count) = count.filter(|count| **count < MAX_FACTS_PER_CALL) else {
        report.provider_facts_dropped += 1;
        return;
    };
    *count += 1;
    let received = fact.at.saturating_duration_since(start).as_millis();
    report.provider_facts.push(ProviderFact {
        call_id: fact.call_id,
        kind: fact.kind,
        received_ms: u64::try_from(received).unwrap_or(u64::MAX),
        provider_ms: fact.provider_ms,
    });
}
fn invalid_arguments(message: &str) -> ProviderError {
    ProviderError {
        code: "invalid_arguments".into(),
        message: message.into(),
    }
}
fn validate_call(call: &ToolCall, config: &RunConfig) -> Result<(), ProviderError> {
    if !config.allowed_tools.contains(&call.tool) {
        return Err(ProviderError {
            code: "tool_denied".into(),
            message: "tool is not authorized by the host".into(),
        });
    }
    match call.tool.as_str() {
        "mock.wait" if call.arguments.get("ms").and_then(Value::as_u64).is_none() => Err(
            invalid_arguments("mock.wait requires an object with nonnegative integer ms"),
        ),
        "mock.fail"
            if !call.arguments.is_object()
                || call
                    .arguments
                    .get("message")
                    .is_some_and(|value| !value.is_string()) =>
        {
            Err(invalid_arguments(
                "mock.fail requires an object with optional string message",
            ))
        }
        _ => Ok(()),
    }
}

/// Whether [`frame`] can finish now without waiting: a complete frame, end of
/// input, an error, or an oversized frame is available. Bytes of an incomplete
/// frame move into `partial`, where [`frame`] resumes, so a guest that stops
/// mid-frame cannot hold back dispatch.
async fn frame_ready(
    reader: &mut (impl AsyncBufRead + Unpin),
    partial: &mut Vec<u8>,
    limit: usize,
) -> bool {
    std::future::poll_fn(|context| {
        loop {
            let count = match Pin::new(&mut *reader).poll_fill_buf(context) {
                Poll::Pending => return Poll::Ready(false),
                Poll::Ready(Ok(bytes))
                    if !bytes.is_empty()
                        && !bytes.contains(&b'\n')
                        && partial.len() + bytes.len() <= limit =>
                {
                    partial.extend_from_slice(bytes);
                    bytes.len()
                }
                Poll::Ready(_) => return Poll::Ready(true),
            };
            Pin::new(&mut *reader).consume(count);
        }
    })
    .await
}

/// Read one LF-terminated frame of at most `limit` bytes, keeping partial input
/// in `bytes` so a cancelled read can resume.
pub(crate) async fn frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    bytes: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unterminated frame",
                ))
            };
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if bytes.len() + count > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds {limit} bytes"),
            ));
        }
        let finished = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if finished {
            return Ok(Some(std::mem::take(bytes)));
        }
    }
}

/// A restricted guest process started ahead of a run, waiting for its program.
///
/// It serves exactly one run and is killed when dropped. Starting it early takes
/// Deno and worker startup off a run's critical path; the run's deadline then
/// starts when [`run_prepared`] is called, and the report records how long the
/// guest had been waiting.
pub struct PreparedGuest {
    launcher: Option<Launcher>,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    spawned: Instant,
    backend: ExecutionBackend,
}

impl fmt::Debug for PreparedGuest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedGuest")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl PreparedGuest {
    /// Wait until the guest process exits, for example while it waits for a program.
    pub(crate) async fn exited(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Start the guest with `deno`, before its program is known.
    ///
    /// Requires a Tokio runtime with I/O enabled.
    ///
    /// # Errors
    /// Returns [`RunError::Io`] if the launcher cannot be written or Deno cannot start.
    pub fn start(deno: &Path) -> Result<Self, RunError> {
        let launcher = Launcher::create()?;
        let deno = resolve_executable(deno)?;
        let spawned = Instant::now();
        let mut command = Command::new(deno);
        command
            .args([
                "run",
                "--no-config",
                "--no-lock",
                "--no-npm",
                "--no-remote",
                "--cached-only",
                "--no-prompt",
                "--deny-read",
                "--deny-write",
                "--deny-net",
                "--deny-env",
                "--deny-run",
                "--deny-sys",
                "--deny-ffi",
                "--deny-import",
                "--unstable-worker-options",
            ])
            .arg(&launcher.0);
        Self::spawn(command, Some(launcher), spawned, ExecutionBackend::Deno)
    }

    /// Start a fresh embedded worker before its program is known.
    ///
    /// `executable` must implement codemode's internal worker entry point and be
    /// built with `embedded-v8`. Requires a Tokio runtime with I/O enabled.
    ///
    /// # Errors
    /// Returns [`RunError::Config`] when this library lacks the build feature,
    /// or [`RunError::Io`] when the executable cannot start.
    pub fn start_embedded(executable: &Path) -> Result<Self, RunError> {
        if !cfg!(feature = "embedded-v8") {
            return Err(RunError::Config(
                "embedded backend requires the embedded-v8 build feature".into(),
            ));
        }
        let mut command = Command::new(executable);
        command.arg("--internal-embedded-guest");
        Self::spawn(command, None, Instant::now(), ExecutionBackend::EmbeddedV8)
    }

    pub(crate) fn from_config(config: &RunConfig) -> Result<Self, RunError> {
        match &config.backend {
            GuestBackend::Deno => Self::start(&config.deno),
            GuestBackend::EmbeddedV8 { executable } => Self::start_embedded(executable),
        }
    }

    fn spawn(
        mut command: Command,
        launcher: Option<Launcher>,
        spawned: Instant,
        backend: ExecutionBackend,
    ) -> Result<Self, RunError> {
        let mut child = command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin missing"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("child stdout missing"))?,
        );
        Ok(Self {
            launcher,
            child,
            input,
            output,
            spawned,
            backend,
        })
    }
}

/// Execute TypeScript with the selected isolated guest and host-side scheduler.
///
/// Provider failures can be caught by guest code and remain visible in the trace.
/// A deadline or cancellation drops pending provider futures; an already dispatched
/// external effect may still have occurred. This is not an OS sandbox or RSS limit.
/// Requires a Tokio runtime with I/O and time enabled. The default engine uses
/// Deno 2.9.8-compatible flags; embedded execution requires `embedded-v8`.
///
/// # Examples
///
/// ```no_run
/// use codemode::{run, MockProvider, RunCancellation, RunConfig};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let report = run("return await tools.call(\"mock.echo\", {answer: 42});", RunConfig::default(),
///                  MockProvider, RunCancellation::default()).await?;
/// assert!(report.status.is_success());
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// Returns [`RunError`] for invalid configuration, startup failure, or pipe I/O.
/// Guest errors, protocol violations, cancellation, and exceeded budgets return a report.
pub async fn run(
    code: &str,
    config: RunConfig,
    provider: impl ToolProvider,
    cancellation: RunCancellation,
) -> Result<RunReport, RunError> {
    let initial = validate(code, &config)?;
    // The deadline includes guest startup.
    let start = Instant::now();
    let guest = PreparedGuest::from_config(&config)?;
    execute(
        ExecutionInput::Guest {
            guest: Box::new(guest),
            initial,
            code,
        },
        start,
        config,
        provider,
        cancellation,
    )
    .await
}

/// Execute TypeScript in a guest started earlier with [`PreparedGuest::start`].
///
/// Behaves like [`run`], except that the deadline starts now rather than when the
/// guest was started, and the report's `prewarmed_ms` says how long the guest had
/// been waiting. The guest's `deno` is the one it was started with.
///
/// # Errors
/// Returns [`RunError`] for invalid configuration or pipe I/O, as [`run`] does.
pub async fn run_prepared(
    guest: PreparedGuest,
    code: &str,
    config: RunConfig,
    provider: impl ToolProvider,
    cancellation: RunCancellation,
) -> Result<RunReport, RunError> {
    let initial = validate(code, &config)?;
    execute(
        ExecutionInput::Guest {
            guest: Box::new(guest),
            initial,
            code,
        },
        Instant::now(),
        config,
        provider,
        cancellation,
    )
    .await
}

fn validate_limits(config: &RunConfig) -> Result<(), RunError> {
    if config.timeout_ms == 0 || config.max_concurrency == 0 || config.max_output_bytes == 0 {
        return Err(RunError::Config(
            "timeout, concurrency, and output budget must be positive".into(),
        ));
    }
    Ok(())
}

/// Check limits and source size, returning the initial guest frame.
fn validate(code: &str, config: &RunConfig) -> Result<Vec<u8>, RunError> {
    validate_limits(config)?;
    if code.len() > FRAME_BYTES {
        return Err(RunError::Config(
            "source exceeds the 64 KiB frame limit".into(),
        ));
    }
    let initial =
        serde_json::to_vec(&json!({"type":"run", "code":code})).map_err(io::Error::other)?;
    if initial.len() + 1 > FRAME_BYTES {
        return Err(RunError::Config(
            "serialized source exceeds the 64 KiB frame limit".into(),
        ));
    }
    Ok(initial)
}

/// Execute finite host-owned requests through the program runtime's controls.
///
/// Deno is never resolved or started. Input exhaustion waits for queued and
/// running calls; successful runs have no final output value. Authorization,
/// dispatch policy, deadlines, cancellation, provider facts, and cumulative
/// serialized-result limits match program execution. Results are budgeted as
/// JSON values or errors, without a guest reply envelope.
///
/// Requests are borrowed, with at most `max_calls + 1` copied into the report.
/// Each request must serialize within 64 KiB. Requests after the first excess
/// call are neither inspected nor admitted.
///
/// # Examples
///
/// ```
/// use codemode::{NativeCall, MockProvider, RunCancellation, RunConfig, run_calls};
/// # async fn example() -> Result<(), codemode::RunError> {
/// let requests = [NativeCall { tool: "mock.echo".into(), arguments: serde_json::json!(42) }];
/// let report = run_calls(&requests, RunConfig::default(), MockProvider,
///                        RunCancellation::default()).await?;
/// assert!(report.status.is_success());
/// assert!(report.output.is_none());
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// Returns [`RunError::Config`] for invalid limits or oversized admitted input.
/// Provider failures, cancellation, and exceeded execution budgets return a report.
pub async fn run_calls(
    calls: &[NativeCall],
    config: RunConfig,
    provider: impl ToolProvider,
    cancellation: RunCancellation,
) -> Result<RunReport, RunError> {
    validate_limits(&config)?;
    for call in calls.iter().take(config.max_calls.saturating_add(1)) {
        if bounded_json(call, FRAME_BYTES).is_none() {
            return Err(RunError::Config(
                "native request exceeds the 64 KiB limit".into(),
            ));
        }
    }
    execute(
        ExecutionInput::Native(calls),
        Instant::now(),
        config,
        provider,
        cancellation,
    )
    .await
}

enum ExecutionInput<'a> {
    Guest {
        guest: Box<PreparedGuest>,
        initial: Vec<u8>,
        code: &'a str,
    },
    Native(&'a [NativeCall]),
}

enum Submission {
    Call {
        guest_id: Option<u64>,
        tool: String,
        arguments: Value,
    },
    Output(Value),
    Error(String),
    InvalidFrame,
    Exhausted,
}

async fn execute(
    source: ExecutionInput<'_>,
    start: Instant,
    config: RunConfig,
    provider: impl ToolProvider,
    cancellation: RunCancellation,
) -> Result<RunReport, RunError> {
    let (guest, initial, code, native_calls, backend) = match source {
        ExecutionInput::Guest {
            guest,
            initial,
            code,
        } => {
            let backend = guest.backend;
            (Some(guest), Some(initial), code, &[][..], backend)
        }
        ExecutionInput::Native(calls) => (None, None, "", calls, ExecutionBackend::Native),
    };
    let native = backend == ExecutionBackend::Native;
    let mut native_calls = native_calls.iter();
    let deadline = start
        .checked_add(Duration::from_millis(config.timeout_ms))
        .ok_or_else(|| RunError::Config("timeout is too large".into()))?;
    let mut report = RunReport {
        schema_version: REPORT_SCHEMA_VERSION,
        backend,
        code: code.into(),
        status: RunStatus::Success,
        limits: RunLimits {
            timeout_ms: config.timeout_ms,
            max_calls: config.max_calls,
            max_concurrency: config.max_concurrency,
            max_output_bytes: config.max_output_bytes,
            dispatch: config.dispatch,
        },
        output: None,
        calls: Vec::new(),
        events: Vec::new(),
        provider_facts: Vec::new(),
        provider_facts_dropped: 0,
        elapsed_ms: 0,
        clock: ClockInfo::default(),
        bytes: ByteTotals::default(),
        prewarmed_ms: None,
        bridge: None,
        diagnostic: None,
    };
    let (mut child, mut input, mut output, _launcher) = if let Some(guest) = guest {
        let PreparedGuest {
            launcher,
            child,
            input,
            output,
            spawned,
            backend: _,
        } = *guest;
        if spawned < start {
            let waited = start.duration_since(spawned).as_millis();
            report.prewarmed_ms = Some(u64::try_from(waited).unwrap_or(u64::MAX));
        } else {
            event(&mut report, start, None, "guest_spawned");
        }
        (Some(child), Some(input), Some(output), launcher)
    } else {
        (None, None, None, None)
    };
    let provider = Arc::new(provider);
    let (facts, mut fact_receiver) = FactSink::new();
    let mut fact_counts = HashMap::new();
    let context = CallContext {
        deadline,
        cancellation: cancellation.clone(),
        facts: facts.clone(),
    };
    let mut running: JoinSet<(usize, Result<Value, ProviderError>)> = JoinSet::new();
    let mut task_calls = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut guest_ids = HashSet::new();
    let mut responses: VecDeque<(Option<usize>, Vec<u8>)> =
        initial.into_iter().map(|bytes| (None, bytes)).collect();
    let mut write_offset = 0;
    let mut response_bytes = 0usize;
    let mut guest_done = false;
    let mut source_exhausted = false;
    let mut input_closed = false;
    let mut partial_frame = Vec::new();
    let mut io_failure = None;
    loop {
        if cancellation.is_cancelled() {
            stop(&mut report, start, RunStatus::Cancelled);
            break;
        }
        if Instant::now() >= deadline {
            stop(&mut report, start, RunStatus::Deadline);
            break;
        }
        if guest_done {
            while let Some(index) = queue.pop_front() {
                let record: &mut CallRecord = &mut report.calls[index];
                record.outcome = CallOutcome::CancelledBeforeDispatch;
                record.finished_ms = Some(elapsed(start));
                let call_id = record.call.id;
                event_because(
                    &mut report,
                    start,
                    Some(call_id),
                    "cancelled_before_dispatch",
                    Some("guest_finished"),
                );
            }
        }
        // A batch forms only while nothing runs, after every complete call frame
        // already received is admitted.
        let slots_open = match config.dispatch {
            DispatchPolicy::Rolling => true,
            DispatchPolicy::Batch => {
                running.is_empty()
                    && (source_exhausted
                        || guest_done
                        || if let Some(output) = &mut output {
                            !frame_ready(output, &mut partial_frame, FRAME_BYTES).await
                        } else {
                            false
                        })
            }
        };
        while slots_open && !guest_done && running.len() < config.max_concurrency {
            // Dispatch is linearized at this check; future effects may outlive cancellation.
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                break;
            }
            let Some(index) = queue.pop_front() else {
                break;
            };
            let record: &mut CallRecord = &mut report.calls[index];
            record.started_ms = Some(elapsed(start));
            record.outcome = CallOutcome::Running;
            let call = record.call.clone();
            event(&mut report, start, Some(call.id), "dispatched");
            let provider = Arc::clone(&provider);
            let context = context.clone();
            let task = running.spawn(async move { (index, provider.execute(call, context).await) });
            task_calls.insert(task.id(), index);
        }
        if (guest_done || source_exhausted) && running.is_empty() && queue.is_empty() {
            break;
        }
        if let Some((_, bytes)) = responses.front_mut() {
            if bytes.last() != Some(&b'\n') {
                bytes.push(b'\n');
            }
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => { stop(&mut report, start, RunStatus::Cancelled); break; }
            () = time::sleep_until(deadline) => { stop(&mut report, start, RunStatus::Deadline); break; }
            result = async {
                match &mut input {
                    Some(input) => input.write(responses.front().map_or(&[][..], |(_, bytes)| &bytes[write_offset..])).await,
                    None => std::future::pending().await,
                }
            }, if !responses.is_empty() && !guest_done && !input_closed => {
                match result {
                    Ok(0) => { input_closed = true; responses.clear(); write_offset = 0; }
                    Ok(count) => {
                        report.bytes.to_guest += count as u64;
                        write_offset += count;
                        if write_offset == responses.front().map_or(0, |(_, bytes)| bytes.len()) {
                            if let Some((Some(index), _)) = responses.pop_front() {
                                report.calls[index].delivered_ms = Some(elapsed(start));
                                let call_id = report.calls[index].call.id;
                                event(&mut report, start, Some(call_id), "reply_written");
                            }
                            write_offset = 0;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => { input_closed = true; responses.clear(); write_offset = 0; }
                    Err(error) => { io_failure = Some(error); break; }
                }
            }
            result = running.join_next_with_id(), if !running.is_empty() => {
                let Some(result) = result else { continue; };
                // Facts a provider reported before its result belong before it.
                for _ in 0..FACT_CHANNEL_CAPACITY {
                    let Ok(fact) = fact_receiver.try_recv() else { break };
                    record_fact(&mut report, start, &mut fact_counts, fact);
                }
                let (index, result) = match result {
                    Ok((task_id, value)) => { task_calls.remove(&task_id); value },
                    Err(error) => {
                        report.status = RunStatus::ProviderTaskError;
                        report.diagnostic = Some("provider task terminated without an observed result".into());
                        if let Some(index) = task_calls.remove(&error.id()) {
                            let call_id = report.calls[index].call.id;
                            event(&mut report, start, Some(call_id), "provider_task_failed");
                        }
                        break;
                    }
                };
                let record = &mut report.calls[index];
                record.finished_ms = Some(elapsed(start));
                let kind = if result.is_ok() { "succeeded" } else { "provider_failed" };
                let remaining = config.max_output_bytes.saturating_sub(response_bytes);
                let result_bytes = match &result { Ok(value) => json_len(value), Err(error) => json_len(error) };
                let bytes = if let Some(id) = record.guest_id {
                    let response = Reply {
                        frame_type: "result", id, ok: result.is_ok(),
                        value: result.as_ref().ok(), error: result.as_ref().err(),
                    };
                    bounded_json(&response, (REPLY_FRAME_BYTES - 1).min(remaining))
                } else {
                    None
                };
                let counted = if native {
                    usize::try_from(result_bytes).ok().filter(|count| *count <= remaining)
                } else { bytes.as_ref().map(Vec::len) };
                let call_id = record.call.id;
                record.result_bytes = Some(result_bytes);
                if counted.is_some() {
                    record.outcome = match result {
                        Ok(value) => CallOutcome::Succeeded { value },
                        Err(error) => CallOutcome::ProviderFailed { error },
                    };
                } else {
                    record.outcome = CallOutcome::ResultTooLarge;
                }
                event(&mut report, start, Some(call_id), kind);
                let Some(counted) = counted else { report.status = RunStatus::OutputLimit; break; };
                response_bytes += counted;
                if let Some(bytes) = bytes.filter(|_| !guest_done) {
                    report.calls[index].reply_bytes = Some(bytes.len() as u64 + 1);
                    responses.push_back((Some(index), bytes));
                }
            }
            result = async {
                if let Some(output) = &mut output {
                    let bytes = match frame(output, &mut partial_frame, FRAME_BYTES).await? {
                        Some(bytes) => bytes,
                        None => return Ok::<_, io::Error>((Submission::Exhausted, 0)),
                    };
                    let message = match serde_json::from_slice::<GuestFrame>(&bytes) {
                        Ok(GuestFrame::Call { id, tool, arguments }) => Submission::Call { guest_id: Some(id), tool, arguments },
                        Ok(GuestFrame::Output { value }) => Submission::Output(value),
                        Ok(GuestFrame::Error { message }) => Submission::Error(message),
                        Err(_) => Submission::InvalidFrame,
                    };
                    Ok((message, bytes.len() as u64))
                } else {
                    // Ready native input must still let provider and cancellation tasks run.
                    tokio::task::consume_budget().await;
                    match native_calls.next() {
                        Some(call) => Ok((Submission::Call { guest_id: None, tool: call.tool.clone(), arguments: call.arguments.clone() }, 0)),
                        None => Ok((Submission::Exhausted, 0)),
                    }
                }
            }, if !guest_done && !source_exhausted => {
                let message = match result {
                    Ok((message, bytes)) => { report.bytes.from_guest += bytes; message }
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => { report.status = RunStatus::ProtocolError; report.diagnostic = Some(error.to_string()); break; }
                    Err(error) => { io_failure = Some(error); break; }
                };
                match message {
                    Submission::Call { guest_id, tool, arguments } => {
                        if let Some(id) = guest_id {
                            if id > MAX_GUEST_ID { report.status = RunStatus::ProtocolError; report.diagnostic = Some("guest correlation token exceeds the JavaScript safe integer range".into()); break; }
                            if !guest_ids.insert(id) { report.status = RunStatus::ProtocolError; report.diagnostic = Some("duplicate guest correlation token".into()); break; }
                        }
                        let call = ToolCall { id: report.calls.len() as u64 + 1, tool, arguments };
                        let over_limit = report.calls.len() >= config.max_calls;
                        let rejection = if over_limit {
                            Some(ProviderError { code: "call_limit".into(), message: "call allowance exhausted".into() })
                        } else {
                            validate_call(&call, &config).err()
                        };
                        let index = report.calls.len();
                        let call_id = call.id;
                        let argument_bytes = json_len(&call.arguments);
                        report.calls.push(CallRecord {
                            call, guest_id, outcome: CallOutcome::Queued, admitted_ms: Some(elapsed(start)), started_ms: None,
                            finished_ms: None, delivered_ms: None, argument_bytes, result_bytes: None, reply_bytes: None,
                        });
                        event(&mut report, start, Some(call_id), "admitted");
                        if let Some(error) = rejection {
                            let error_bytes = json_len(&error);
                            report.calls[index].result_bytes = Some(error_bytes);
                            let reply = guest_id.filter(|_| !over_limit).map(|id| {
                                serde_json::to_vec(&json!({"type":"result", "id":id, "ok":false, "error":error})).map_err(io::Error::other)
                            }).transpose()?;
                            report.calls[index].outcome = CallOutcome::Rejected { error };
                            report.calls[index].finished_ms = Some(elapsed(start));
                            event(&mut report, start, Some(call_id), "rejected");
                            if over_limit { report.status = RunStatus::CallLimit; break; }
                            let counted = reply.as_ref().map_or(error_bytes as usize, Vec::len);
                            if counted > config.max_output_bytes.saturating_sub(response_bytes) { report.status = RunStatus::OutputLimit; break; }
                            response_bytes += counted;
                            if let Some(bytes) = reply {
                                report.calls[index].reply_bytes = Some(bytes.len() as u64 + 1);
                                responses.push_back((Some(index), bytes));
                            }
                        } else { queue.push_back(index); }
                    }
                    Submission::Output(value) => {
                        event_because(&mut report, start, None, "guest_finished", Some("output"));
                        let Some(bytes) = bounded_json(&value, config.max_output_bytes.saturating_sub(response_bytes)) else {
                            report.bytes.omitted += json_len(&value);
                            report.status = RunStatus::OutputLimit;
                            break;
                        };
                        response_bytes += bytes.len();
                        report.output = Some(value);
                        report.bytes.retained += bytes.len() as u64;
                        guest_done = true;
                        responses.clear();
                        if let Some(child) = &mut child {
                            if let Err(error) = child.start_kill() { io_failure = Some(error); break; }
                        }
                    }
                    Submission::Error(message) => { report.status = RunStatus::GuestError; report.diagnostic = Some(message); event_because(&mut report, start, None, "guest_finished", Some("error")); guest_done = true; responses.clear(); if let Some(child) = &mut child {
                            if let Err(error) = child.start_kill() { io_failure = Some(error); break; }
                        } }
                    Submission::InvalidFrame => {
                        report.status = RunStatus::ProtocolError;
                        report.diagnostic = Some("invalid child stdout frame".into());
                        break;
                    }
                    Submission::Exhausted => {
                        if native {
                            source_exhausted = true;
                            event(&mut report, start, None, "input_exhausted");
                        } else {
                            report.status = RunStatus::GuestError;
                            report.diagnostic = Some("child stdout closed without returning output; stderr discarded".into());
                            event_because(&mut report, start, None, "guest_finished", Some("exited"));
                            guest_done = true;
                            responses.clear();
                            if let Some(child) = &mut child {
                                if let Err(error) = child.start_kill() { io_failure = Some(error); break; }
                            }
                        }
                    }
                }
            }
            // Last, so a provider flooding facts cannot starve guest I/O or results.
            Some(fact) = fact_receiver.recv() => record_fact(&mut report, start, &mut fact_counts, fact),
        }
    }
    if native {
        event(&mut report, start, None, "execution_finished");
    }
    // Stop producers first, so this drain ends after at most the buffered facts.
    fact_receiver.close();
    while let Ok(fact) = fact_receiver.try_recv() {
        record_fact(&mut report, start, &mut fact_counts, fact);
    }
    let stop_reason = report.status.as_str();
    for index in 0..report.calls.len() {
        let record = &mut report.calls[index];
        let kind = match record.outcome {
            CallOutcome::Queued => {
                record.outcome = CallOutcome::CancelledBeforeDispatch;
                Some("cancelled_before_dispatch")
            }
            CallOutcome::Running => {
                record.outcome = CallOutcome::Unknown;
                Some("unknown")
            }
            _ => None,
        };
        if let Some(kind) = kind {
            record.finished_ms = Some(elapsed(start));
            let call_id = record.call.id;
            event_because(&mut report, start, Some(call_id), kind, Some(stop_reason));
        }
    }
    for record in &report.calls {
        let observed = record.result_bytes.unwrap_or(0);
        match record.outcome {
            CallOutcome::ResultTooLarge => report.bytes.omitted += observed,
            _ => report.bytes.retained += observed,
        }
        report.bytes.retained += record.argument_bytes;
    }
    if !report.status.is_success() {
        cancellation.cancel();
    }
    running.abort_all();
    // Wait until every provider task has stopped, so none still runs after the report.
    while running.join_next().await.is_some() {}
    let wait_result = if let Some(child) = &mut child {
        let _ = child.start_kill();
        let result = child.wait().await;
        event(&mut report, start, None, "guest_terminated");
        result.map(|_| ())
    } else {
        Ok(())
    };
    if let Some(error) = io_failure {
        return Err(RunError::Io(error));
    }
    wait_result?;
    report.provider_facts_dropped += facts.dropped.load(Ordering::Relaxed);
    report.elapsed_ms = elapsed(start);
    event(&mut report, start, None, "run_finished");
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::frame;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn partial_stdout_frame_survives_a_cancelled_read() {
        let (mut sender, receiver) = tokio::io::duplex(128);
        let mut reader = BufReader::new(receiver);
        let mut partial = Vec::new();
        sender
            .write_all(b"{\"type\":")
            .await
            .expect("first fragment is writable");
        tokio::select! {
            biased;
            result = frame(&mut reader, &mut partial, 128) => panic!("partial frame completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        sender
            .write_all(b"\"output\",\"value\":1}\n")
            .await
            .expect("second fragment is writable");
        let bytes = frame(&mut reader, &mut partial, 128)
            .await
            .expect("complete frame is readable")
            .expect("frame exists");
        assert_eq!(bytes, b"{\"type\":\"output\",\"value\":1}\n");
    }
}
