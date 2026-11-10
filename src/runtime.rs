use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

use crate::embedded::MEMORY_LIMIT_EXIT_CODE;

/// Largest frame from the guest (source and call frames), including its LF.
pub(crate) const FRAME_BYTES: usize = 64 * 1024;
/// Largest reply frame to the guest, including its LF; the worker accepts up to 1 MiB.
pub(crate) const REPLY_FRAME_BYTES: usize = 1024 * 1024;
/// Version of the serialized [`RunReport`] format written by this build.
pub const REPORT_SCHEMA_VERSION: u32 = 5;
/// Provider facts kept per call; later facts for that call are counted as dropped.
const MAX_FACTS_PER_CALL: usize = 8;
/// Provider facts buffered between the provider and the run loop.
const FACT_CHANNEL_CAPACITY: usize = 1024;
pub(crate) const MAX_GUEST_ID: u64 = 9_007_199_254_740_991;
const MIB: u64 = 1024 * 1024;
/// Smallest heap budget; V8 aborts below about 8 MiB while the worker starts,
/// before exhaustion can be reported.
const MIN_HEAP_MB: u64 = 16;
/// Default Linux data limit above the heap budget, for V8's other reservations,
/// thread stacks, and native allocations; measured, see `run`.
#[cfg(target_os = "linux")]
const DATA_MARGIN_MB: u64 = 256;
/// How long a stopped worker's remaining output is read for a `memory_limit` frame.
const STOPPED_OUTPUT_GRACE: Duration = Duration::from_millis(200);

/// Lock `mutex`, taking the data even if a panicking holder poisoned it.
pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    /// Worker executable that runs each program in a fresh process embedding V8,
    /// preferably an absolute path. It must call [`crate::run_embedded_worker`] when
    /// started with the single argument `--internal-embedded-guest`, as the `codemode`
    /// binary does; a library caller's own executable need not. Required by [`run`]
    /// and [`crate::serve_bridge`]; native [`run_calls`] starts no worker.
    pub worker: Option<PathBuf>,
    /// Tool names authorized by this host, independently of guest requests.
    /// Empty by default: every call is denied until the host authorizes its tool.
    pub allowed_tools: Vec<String>,
    /// V8 heap budget for the program's worker, in MiB (default 256, minimum 16). Reaching it
    /// ends the run with [`RunStatus::MemoryLimit`]. It bounds the JavaScript heap
    /// only: `ArrayBuffer` backing stores and native allocations are outside it.
    pub heap_mb: u64,
    /// Linux only: the worker's data-segment limit (`RLIMIT_DATA`) in MiB, which
    /// also bounds `ArrayBuffer` backing stores. `None` applies the heap budget plus
    /// 256 MiB on Linux and no process limit elsewhere. Requesting it on another
    /// platform fails with [`RunError::Config`] before a worker starts. A limit too
    /// small for V8's own reservations (measured: about 112 MiB with small heap
    /// budgets) stops the worker at startup, and the run ends with
    /// [`RunStatus::WorkerCrashed`].
    pub process_memory_mb: Option<u64>,
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
    /// V8 heap budget in bytes applied to the worker; `null` for native runs.
    pub heap_bytes: Option<u64>,
    /// Data-segment limit (`RLIMIT_DATA`) in bytes the worker applied to itself
    /// before starting V8; `null` when no process limit was applied, as on every
    /// platform other than Linux and for native runs.
    pub process_data_bytes: Option<u64>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            max_calls: 32,
            max_concurrency: 4,
            max_output_bytes: 256 * 1024,
            worker: None,
            allowed_tools: Vec::new(),
            heap_mb: 256,
            process_memory_mb: None,
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
    #[serde(deserialize_with = "crate::json::value")]
    pub arguments: Value,
}

/// The execution source whose work and lifecycle the report records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionBackend {
    /// Host-owned finite calls, with no JavaScript guest.
    Native,
    /// A program in a restricted worker subprocess embedding V8.
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
    #[serde(deserialize_with = "crate::json::value")]
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

/// Local deterministic tools for exercising execution without external credentials:
/// `mock.echo` returns its arguments, `mock.wait` `{ms, value?}` sleeps, and
/// `mock.fail` `{message?}` fails with `mock_failure`. Invalid arguments fail with
/// `invalid_arguments`. Requires the `testkit` feature.
#[cfg(feature = "testkit")]
#[derive(Debug, Clone, Copy, Default)]
pub struct MockProvider;

#[cfg(feature = "testkit")]
impl MockProvider {
    /// The tools this provider implements.
    pub const TOOLS: [&'static str; 3] = ["mock.echo", "mock.wait", "mock.fail"];

    /// [`RunConfig::default`] with every mock tool authorized.
    #[must_use]
    pub fn config() -> RunConfig {
        RunConfig {
            allowed_tools: Self::TOOLS.map(String::from).to_vec(),
            ..RunConfig::default()
        }
    }
}

#[cfg(feature = "testkit")]
impl ToolProvider for MockProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let invalid = |message: &str| ProviderError {
            code: "invalid_arguments".into(),
            message: message.into(),
        };
        match call.tool.as_str() {
            "mock.echo" => Ok(call.arguments),
            "mock.wait" => {
                let ms = call.arguments["ms"].as_u64().ok_or_else(|| {
                    invalid("mock.wait requires an object with nonnegative integer ms")
                })?;
                tokio::select! {
                    () = time::sleep(Duration::from_millis(ms)) => Ok(call.arguments.get("value").cloned().unwrap_or(Value::Null)),
                    () = time::sleep_until(context.deadline) => Err(ProviderError { code: "deadline".into(), message: "run deadline reached".into() }),
                    () = context.cancellation.cancelled() => Err(ProviderError { code: "cancelled".into(), message: "run cancelled".into() }),
                }
            }
            "mock.fail" => match call.arguments.get("message") {
                _ if !call.arguments.is_object() => Err(invalid(
                    "mock.fail requires an object with optional string message",
                )),
                Some(Value::String(message)) => Err(ProviderError {
                    code: "mock_failure".into(),
                    message: message.clone(),
                }),
                None => Err(ProviderError {
                    code: "mock_failure".into(),
                    message: "mock provider failed".into(),
                }),
                Some(_) => Err(invalid(
                    "mock.fail requires an object with optional string message",
                )),
            },
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
    /// The worker exhausted its memory: V8 reached the heap budget, or on Linux an
    /// allocation failed under the process data limit. The worker reported it with
    /// a `memory_limit` frame or exited with the reserved code 75.
    MemoryLimit,
    /// The worker died without output for another reason, such as a signal it
    /// did not receive from this controller; [`RunReport::worker_exit`] records how.
    WorkerCrashed,
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
            Self::MemoryLimit => "memory_limit",
            Self::WorkerCrashed => "worker_crashed",
        }
    }

    /// CLI exit code: success 0, cancellation 130, deadline 124, other failure,
    /// including `memory_limit` and `worker_crashed`, 1.
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
        #[serde(deserialize_with = "crate::json::value")]
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
    /// Admission time.
    pub admitted_ms: Option<u64>,
    /// Dispatch time since run start; absent for calls never dispatched.
    pub started_ms: Option<u64>,
    /// Completion or termination time since run start.
    pub finished_ms: Option<u64>,
    /// When the reply frame was completely written to the guest's input. The guest
    /// may not have read it; absent when no reply was delivered.
    pub delivered_ms: Option<u64>,
    /// Serialized size of the submitted arguments.
    pub argument_bytes: u64,
    /// Serialized size of the observed value or error, including omitted ones.
    pub result_bytes: Option<u64>,
    /// Bytes of the reply frame queued for the guest.
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
    /// Execution source.
    pub backend: ExecutionBackend,
    /// Exact executed TypeScript source; empty for native execution.
    pub code: String,
    /// Host-selected terminal status.
    pub status: RunStatus,
    /// Limits chosen by the host for this run.
    pub limits: RunLimits,
    /// Guest-returned JSON value, when observed within limits; omitted from JSON when absent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::json::optional"
    )]
    pub output: Option<Value>,
    /// Calls in admission order, using host-assigned identities.
    pub calls: Vec<CallRecord>,
    /// Host-generated events in observation order.
    pub events: Vec<RunEvent>,
    /// Facts the provider reported about dispatched calls, in arrival order.
    pub provider_facts: Vec<ProviderFact>,
    /// Provider facts discarded: over the per-call limit, for calls not running, or unbuffered.
    pub provider_facts_dropped: u64,
    /// Monotonic elapsed wall-clock milliseconds.
    pub elapsed_ms: u64,
    /// The clock behind every Rust time in this report.
    pub clock: ClockInfo,
    /// Bytes moved, retained, and omitted, as Rust observed them.
    pub bytes: ByteTotals,
    /// Bridge stdio accounting, present when the run was served over the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<BridgeTotals>,
    /// A bounded explanation of the terminal status; child stderr is discarded.
    pub diagnostic: Option<String>,
    /// How the worker exited when it ended without output and without being
    /// stopped by this controller; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_exit: Option<WorkerExit>,
}

/// A worker's own exit, as the controller observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerExit {
    /// Exit code, when the worker exited normally.
    pub code: Option<i32>,
    /// Terminating signal number, when a signal ended the worker.
    pub signal: Option<i32>,
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
    /// Invalid host limits, oversized source, a missing worker, or an oversized native request.
    Config(String),
    /// Worker process spawning or pipe I/O failed.
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
        #[serde(deserialize_with = "crate::json::value")]
        arguments: Value,
    },
    Output {
        #[serde(deserialize_with = "crate::json::value")]
        value: Value,
    },
    Error {
        message: String,
    },
    MemoryLimit {
        heap_limit_bytes: u64,
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
fn validate_call(call: &ToolCall, config: &RunConfig) -> Result<(), ProviderError> {
    if !config.allowed_tools.contains(&call.tool) {
        return Err(ProviderError {
            code: "tool_denied".into(),
            message: "tool is not authorized by the host".into(),
        });
    }
    Ok(())
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

/// A fresh worker process for one program, killed when dropped.
struct Guest {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Guest {
    /// Start `executable` as a worker with an empty environment and discarded stderr.
    fn start(executable: &Path) -> Result<Self, RunError> {
        let mut child = Command::new(executable)
            .arg("--internal-embedded-guest")
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
            child,
            input,
            output,
        })
    }
}

/// Execute TypeScript in a fresh V8 worker process with the host-side scheduler.
///
/// The worker named by [`RunConfig::worker`] starts with an empty environment,
/// strips TypeScript types without checking them, refuses every import, and exposes
/// only `tools.call`, `console`, and standard JavaScript built-ins other than
/// `WebAssembly`, `SharedArrayBuffer`, and `Atomics`.
/// Provider failures can be caught by guest code and remain visible in the trace.
/// A deadline or cancellation kills the worker and drops pending provider futures;
/// an already dispatched external effect may still have occurred. This is not an
/// OS sandbox. Requires a Tokio runtime with I/O and time enabled.
///
/// # Memory
///
/// The worker enforces [`RunConfig::heap_mb`] as its V8 heap budget on every
/// platform. On Linux it also applies a data-segment limit (`RLIMIT_DATA`) to
/// itself before V8 starts, which bounds `ArrayBuffer` backing stores and native
/// allocations; by default the heap budget plus 256 MiB, a margin measured to
/// leave V8's startup reservations and the heap budget's exhaustion path working.
/// Elsewhere no process limit is applied, so `ArrayBuffer` memory is unbounded,
/// and [`RunConfig::process_memory_mb`] is refused. [`RunLimits`] records what was
/// applied. Exhaustion ends the run with [`RunStatus::MemoryLimit`]; another worker
/// death without output, such as a signal from elsewhere, with
/// [`RunStatus::WorkerCrashed`]. Either way calls already dispatched keep their
/// observed results, collected until the deadline, and are otherwise unknown.
///
/// # Examples
///
/// ```no_run
/// use codemode::{CallContext, ProviderError, RunCancellation, RunConfig, ToolCall, ToolProvider, run};
/// use serde_json::Value;
///
/// struct Echo;
/// impl ToolProvider for Echo {
///     async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
///         Ok(call.arguments)
///     }
/// }
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // The host authorizes each tool; the default configuration authorizes none.
/// let config = RunConfig {
///     worker: Some("/usr/local/bin/codemode".into()),
///     allowed_tools: vec!["host.echo".into()],
///     ..RunConfig::default()
/// };
/// let report = run("return await tools.call(\"host.echo\", {answer: 42});", config,
///                  Echo, RunCancellation::default()).await?;
/// assert!(report.status.is_success());
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// Returns [`RunError::Config`] for invalid limits, including a heap budget under
/// 16 MiB or a process memory limit this platform cannot enforce, oversized source,
/// or a missing [`RunConfig::worker`], and [`RunError::Io`] for worker startup
/// failure or pipe I/O.
/// Guest errors, protocol violations, cancellation, and exceeded budgets return a report.
pub async fn run(
    code: &str,
    config: RunConfig,
    provider: impl ToolProvider,
    cancellation: RunCancellation,
) -> Result<RunReport, RunError> {
    let (initial, memory) = validate(code, &config)?;
    let worker = config
        .worker
        .as_deref()
        .ok_or_else(|| RunError::Config("no worker executable is configured".into()))?;
    // The deadline includes worker startup.
    let start = Instant::now();
    let guest = Guest::start(worker)?;
    execute(
        ExecutionInput::Guest {
            guest: Box::new(guest),
            initial,
            code,
            memory,
        },
        start,
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

/// The worker's memory limits in bytes.
#[derive(Debug, Clone, Copy)]
struct WorkerMemory {
    /// V8 heap budget.
    heap: u64,
    /// The process data limit the worker applies to itself, if this platform can enforce one.
    data: Option<u64>,
}

fn memory_limits(config: &RunConfig) -> Result<WorkerMemory, RunError> {
    let bytes = |mb: u64, name: &str| {
        mb.checked_mul(MIB)
            .filter(|_| mb > 0)
            .ok_or_else(|| RunError::Config(format!("{name} must be a positive number of MiB")))
    };
    if config.heap_mb < MIN_HEAP_MB {
        return Err(RunError::Config(format!(
            "the heap budget must be at least {MIN_HEAP_MB} MiB"
        )));
    }
    let heap = bytes(config.heap_mb, "the heap budget")?;
    #[cfg(target_os = "linux")]
    let data = {
        let data_mb = config
            .process_memory_mb
            .unwrap_or_else(|| config.heap_mb.saturating_add(DATA_MARGIN_MB));
        Some(bytes(data_mb, "the process memory limit")?)
    };
    #[cfg(not(target_os = "linux"))]
    let data = match config.process_memory_mb {
        Some(_) => {
            return Err(RunError::Config(
                "a process memory limit is enforced only on Linux; this platform \
                 enforces only the V8 heap budget"
                    .into(),
            ));
        }
        None => None,
    };
    Ok(WorkerMemory { heap, data })
}

/// Check limits and source size, returning the initial guest frame and the
/// worker's memory limits.
fn validate(code: &str, config: &RunConfig) -> Result<(Vec<u8>, WorkerMemory), RunError> {
    validate_limits(config)?;
    let memory = memory_limits(config)?;
    if code.len() > FRAME_BYTES {
        return Err(RunError::Config(
            "source exceeds the 64 KiB frame limit".into(),
        ));
    }
    let initial = serde_json::to_vec(&json!({
        "type": "run", "code": code, "heap_bytes": memory.heap, "process_data_bytes": memory.data,
    }))
    .map_err(io::Error::other)?;
    if initial.len() + 1 > FRAME_BYTES {
        return Err(RunError::Config(
            "serialized source exceeds the 64 KiB frame limit".into(),
        ));
    }
    Ok((initial, memory))
}

/// Execute finite host-owned requests through the program runtime's controls.
///
/// No worker is started. Input exhaustion waits for queued and
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
/// use codemode::{
///     CallContext, NativeCall, ProviderError, RunCancellation, RunConfig, ToolCall, ToolProvider,
///     run_calls,
/// };
/// use serde_json::Value;
///
/// struct Echo;
/// impl ToolProvider for Echo {
///     async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
///         Ok(call.arguments)
///     }
/// }
///
/// # async fn example() -> Result<(), codemode::RunError> {
/// let requests = [NativeCall { tool: "host.echo".into(), arguments: serde_json::json!(42) }];
/// let config = RunConfig { allowed_tools: vec!["host.echo".into()], ..RunConfig::default() };
/// let report = run_calls(&requests, config, Echo, RunCancellation::default()).await?;
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
        guest: Box<Guest>,
        initial: Vec<u8>,
        code: &'a str,
        memory: WorkerMemory,
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
    MemoryLimit(u64),
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
    let (guest, initial, code, native_calls, backend, memory) = match source {
        ExecutionInput::Guest {
            guest,
            initial,
            code,
            memory,
        } => (
            Some(guest),
            Some(initial),
            code,
            &[][..],
            ExecutionBackend::EmbeddedV8,
            Some(memory),
        ),
        ExecutionInput::Native(calls) => (None, None, "", calls, ExecutionBackend::Native, None),
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
            heap_bytes: memory.map(|memory| memory.heap),
            process_data_bytes: memory.and_then(|memory| memory.data),
        },
        output: None,
        calls: Vec::new(),
        events: Vec::new(),
        provider_facts: Vec::new(),
        provider_facts_dropped: 0,
        elapsed_ms: 0,
        clock: ClockInfo::default(),
        bytes: ByteTotals::default(),
        bridge: None,
        diagnostic: None,
        worker_exit: None,
    };
    let (mut child, mut input, mut output) = if let Some(guest) = guest {
        let Guest {
            child,
            input,
            output,
        } = *guest;
        event(&mut report, start, None, "guest_spawned");
        (Some(child), Some(input), Some(output))
    } else {
        (None, None, None)
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
    // How the worker itself ended, once known; later stops cannot replace it.
    let mut worker_end: Option<(RunStatus, String)> = None;
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
        // A queued call starts as soon as any provider slot is free.
        while !guest_done && running.len() < config.max_concurrency {
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
        if let Some((_, bytes)) = responses.front_mut()
            && bytes.last() != Some(&b'\n')
        {
            bytes.push(b'\n');
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
                        Ok(GuestFrame::MemoryLimit { heap_limit_bytes }) => Submission::MemoryLimit(heap_limit_bytes),
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
                        if let Some(child) = &mut child
                            && let Err(error) = child.start_kill() { io_failure = Some(error); break; }
                    }
                    Submission::Error(message) => { report.status = RunStatus::GuestError; report.diagnostic = Some(message); event_because(&mut report, start, None, "guest_finished", Some("error")); guest_done = true; responses.clear(); if let Some(child) = &mut child
                            && let Err(error) = child.start_kill() { io_failure = Some(error); break; } }
                    Submission::MemoryLimit(limit) => {
                        // Recorded before the worker is killed, so no kill can reclassify it.
                        worker_end = Some((RunStatus::MemoryLimit, memory_diagnostic(limit)));
                        event_because(&mut report, start, None, "guest_finished", Some(RunStatus::MemoryLimit.as_str()));
                        guest_done = true;
                        responses.clear();
                        if let Some(child) = &mut child
                            && let Err(error) = child.start_kill() { io_failure = Some(error); break; }
                    }
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
                            guest_done = true;
                            responses.clear();
                            // Wait for the exit without killing: a kill would hide how the worker ended.
                            let exit = match &mut child {
                                Some(child) => tokio::select! {
                                    biased;
                                    () = cancellation.cancelled() => None,
                                    () = time::sleep_until(deadline) => None,
                                    status = child.wait() => Some(status),
                                },
                                None => None,
                            };
                            let exit = match exit.transpose() {
                                Ok(exit) => exit.map(worker_exit),
                                Err(error) => { io_failure = Some(error); break; }
                            };
                            report.worker_exit = exit;
                            let reason = match exit_status(exit) {
                                Some(end) => {
                                    let status = end.0;
                                    worker_end = Some(end);
                                    status.as_str()
                                }
                                None => {
                                    report.status = RunStatus::GuestError;
                                    report.diagnostic = Some("child stdout closed without returning output; stderr discarded".into());
                                    "exited"
                                }
                            };
                            event_because(&mut report, start, None, "guest_finished", Some(reason));
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
    // A worker this controller stops, for example at a deadline, may have exhausted
    // its memory at the same moment: a `memory_limit` frame it wrote before dying,
    // or its reserved exit code, still classifies the run. A protocol violation
    // makes the rest of its output untrustworthy.
    if !guest_done
        && worker_end.is_none()
        && report.status != RunStatus::ProtocolError
        && let (Some(child), Some(output)) = (&mut child, &mut output)
    {
        let _ = child.start_kill();
        let exited = child.wait().await.ok().and_then(|status| status.code());
        let reported = time::timeout(
            STOPPED_OUTPUT_GRACE,
            stopped_memory_limit(output, &mut partial_frame),
        )
        .await
        .ok()
        .flatten();
        if let Some(limit) = reported {
            worker_end = Some((RunStatus::MemoryLimit, memory_diagnostic(limit)));
        } else if exited == Some(MEMORY_LIMIT_EXIT_CODE) {
            worker_end = exit_status(Some(WorkerExit {
                code: exited,
                signal: None,
            }));
        }
    }
    if let Some((status, diagnostic)) = worker_end {
        report.status = status;
        report.diagnostic = Some(diagnostic);
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

fn memory_diagnostic(heap_limit_bytes: u64) -> String {
    format!(
        "the worker exhausted its memory: V8 neared its heap limit of {heap_limit_bytes} bytes, \
         or an allocation failed under the process data limit"
    )
}

fn worker_exit(status: std::process::ExitStatus) -> WorkerExit {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    WorkerExit {
        code: status.code(),
        signal,
    }
}

/// Classify a worker that ended on its own without output: the reserved code is
/// memory exhaustion, a signal or an unexpected code is a crash, and its own
/// failure codes 0 and 1 (or an unobserved exit) are left to the caller.
fn exit_status(exit: Option<WorkerExit>) -> Option<(RunStatus, String)> {
    let exit = exit?;
    match (exit.code, exit.signal) {
        (Some(MEMORY_LIMIT_EXIT_CODE), _) => Some((
            RunStatus::MemoryLimit,
            format!(
                "the worker exited with the reserved memory-limit code {MEMORY_LIMIT_EXIT_CODE}"
            ),
        )),
        (_, Some(signal)) => Some((
            RunStatus::WorkerCrashed,
            format!(
                "the worker was terminated by signal {signal} without returning output; stderr discarded"
            ),
        )),
        (Some(0 | 1), _) | (None, None) => None,
        (Some(code), None) => Some((
            RunStatus::WorkerCrashed,
            format!(
                "the worker exited with code {code} without returning output; stderr discarded"
            ),
        )),
    }
}

/// A `memory_limit` frame among the frames a stopped worker wrote before it died.
async fn stopped_memory_limit(
    output: &mut BufReader<ChildStdout>,
    partial: &mut Vec<u8>,
) -> Option<u64> {
    while let Ok(Some(bytes)) = frame(output, partial, FRAME_BYTES).await {
        if let Ok(GuestFrame::MemoryLimit { heap_limit_bytes }) = serde_json::from_slice(&bytes) {
            return Some(heap_limit_bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        CallContext, CallOutcome, NativeCall, ProviderError, RunCancellation, RunConfig, RunStatus,
        ToolCall, ToolProvider, frame, run_calls,
    };
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncWriteExt, BufReader};

    struct Counting(Arc<AtomicUsize>);
    impl ToolProvider for Counting {
        async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(call.arguments)
        }
    }

    #[tokio::test]
    async fn the_default_configuration_authorizes_no_tool() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let calls =
            ["mock.echo", "mock.wait", "mock.fail", "host.read", ""].map(|tool| NativeCall {
                tool: tool.into(),
                arguments: json!({}),
            });
        let report = run_calls(
            &calls,
            RunConfig::default(),
            Counting(Arc::clone(&dispatches)),
            RunCancellation::default(),
        )
        .await
        .expect("rejections produce a report");
        assert!(RunConfig::default().allowed_tools.is_empty());
        assert_eq!(report.status, RunStatus::Success, "{report:?}");
        assert_eq!(report.calls.len(), calls.len());
        assert!(report.calls.iter().all(|record| matches!(
            &record.outcome,
            CallOutcome::Rejected { error } if error.code == "tool_denied"
        )));
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_program_needs_a_named_worker() {
        let error = super::run(
            "return 1;",
            RunConfig::default(),
            Counting(Arc::new(AtomicUsize::new(0))),
            RunCancellation::default(),
        )
        .await
        .expect_err("no worker is configured by default");
        assert!(
            matches!(&error, super::RunError::Config(message) if message.contains("worker")),
            "{error}"
        );
    }

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
