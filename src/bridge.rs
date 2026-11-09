//! A provider-independent bridge that lets a parent host execute tool calls.
//!
//! The bridge reuses [`run`] unchanged: Rust still admits calls, enforces the
//! deadline, call allowance, concurrency, and byte budget, assigns call identities,
//! and writes the trace. Only provider execution moves to the parent host, which
//! keeps its own registry, permissions, approvals, and credentials.
//!
//! Protocol version 2 uses LF-terminated JSON frames. Host frames are at most
//! [`BRIDGE_FRAME_BYTES`]. The report frame is bounded by the run limits instead:
//! source, per-call arguments, and the output budget. Host to bridge:
//!
//! - `{"type":"start","version":2,"code":"...","tools":["read"]}` must be first.
//!   `tools` is the host's authorization list; the guest never sees this channel.
//! - `{"type":"result","id":1,"ok":true,"value":...}`,
//!   `{"type":"result","id":1,"ok":false,"error":{"code":"...","message":"..."}}`, or
//!   `{"type":"result","id":1,"ok":true,"too_large":true}` (or `"ok":false`) when
//!   the tool's value or error was observed but cannot fit in a frame. Rust records that as `result_too_large`
//!   and ends the run with `output_limit`, as for any oversized observed value. An optional
//!   `"bytes":n` gives the host-measured serialized size; without it the size is unknown.
//! - `{"type":"fact","id":1,"kind":"executor_started","host_ms":12.5}` reports a
//!   milestone for a running call: `received`, `permission_requested`,
//!   `permission_granted`, `permission_denied`, or `executor_started`. `host_ms`
//!   is optional and uses the host's own clock. Rust records facts as host claims,
//!   stamped with their arrival time; facts for finished calls are ignored.
//! - `{"type":"cancel"}` requests run cancellation. Closing the input does too.
//!
//! Bridge to host:
//!
//! - `{"type":"call","id":1,"tool":"read","arguments":{...}}` for each dispatched call.
//! - `{"type":"report","report":{...}}` once, after which the bridge exits.
//! - `{"type":"error","message":"..."}` instead of a report when no run started.
//!
//! The bridge starts its guest immediately and waits for the start frame until its
//! input closes, so a host can start a bridge ahead of a run to take guest startup
//! off the critical path. The run's deadline starts when the start frame arrives.
//!
//! The host should abort every outstanding call when it reads the report. A
//! result for an unknown or already finished call is ignored, because a deadline
//! or cancellation may legitimately race with a late host reply.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::runtime::{REPLY_FRAME_BYTES, frame};
use crate::{
    BridgeTotals, CallContext, CallOutcome, FactSink, ProviderError, ProviderFactKind,
    RunCancellation, RunConfig, RunError, RunReport, RunStatus, ToolCall, ToolProvider, run,
};

/// The bridge protocol version this build speaks; start frames for any other version are refused.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 2;
/// Maximum bytes in one bridge frame, including its LF terminator.
pub const BRIDGE_FRAME_BYTES: usize = 1024 * 1024;
const MAX_TOOLS: usize = 256;
const MAX_TOOL_NAME_BYTES: usize = 128;
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Failure before a run report could be produced.
#[derive(Debug)]
pub enum BridgeError {
    /// The host sent an invalid or missing start frame.
    Protocol(String),
    /// The runtime rejected its configuration or could not start the guest.
    Run(RunError),
    /// Reading or writing the bridge streams failed.
    Io(io::Error),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => write!(formatter, "bridge protocol error: {message}"),
            Self::Run(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "bridge I/O failed: {error}"),
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Run(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for BridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Distinguish an explicit JSON `null` from a missing field.
pub(crate) fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    crate::json::value(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum HostFrame {
    Start {
        version: u32,
        code: String,
        tools: Vec<String>,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(default, deserialize_with = "present")]
        value: Option<Value>,
        #[serde(default)]
        error: Option<ProviderError>,
        #[serde(default)]
        too_large: bool,
        #[serde(default)]
        bytes: Option<u64>,
    },
    Fact {
        id: u64,
        kind: ProviderFactKind,
        #[serde(default)]
        host_ms: Option<f64>,
    },
    Cancel,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BridgeFrame<'a> {
    Call {
        id: u64,
        tool: &'a str,
        arguments: &'a Value,
    },
    Report {
        report: &'a RunReport,
    },
    Error {
        message: &'a str,
    },
}

type Reply = Result<Value, ProviderError>;
/// Running calls: where to send the host's reply and its facts.
type Pending = Arc<Mutex<HashMap<u64, (oneshot::Sender<Reply>, FactSink)>>>;

/// Bytes exchanged with the host, counted as frames are read or queued, and
/// what routing could not attach to a running call.
#[derive(Default)]
struct Counters {
    from_host: AtomicU64,
    to_host: AtomicU64,
    facts_ignored: AtomicU64,
    /// Calls answered with `too_large`, and the host-reported size when given.
    too_large: Mutex<HashMap<u64, Option<u64>>>,
}

pub(crate) enum Outbound {
    Frame(Vec<u8>),
    /// Write this frame, then stop.
    Last(Vec<u8>),
}

/// One LF-terminated JSON frame.
pub(crate) fn encode(frame: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The only writer of bridge output. Frames are written whole and in order, so a
/// dropped provider future can never leave a partial call frame before the report.
pub(crate) async fn write_frames<W: AsyncWrite + Unpin>(
    mut output: W,
    mut frames: mpsc::UnboundedReceiver<Outbound>,
    cancellation: RunCancellation,
) -> io::Result<()> {
    while let Some(frame) = frames.recv().await {
        let (bytes, last) = match frame {
            Outbound::Frame(bytes) => (bytes, false),
            Outbound::Last(bytes) => (bytes, true),
        };
        let written = async {
            output.write_all(&bytes).await?;
            output.flush().await
        };
        if let Err(error) = written.await {
            // The host can no longer receive calls or the report.
            cancellation.cancel();
            return Err(error);
        }
        if last {
            break;
        }
    }
    Ok(())
}

/// Stop a stalled transport without detaching its writer. Once execution ends,
/// a peer has a finite grace period to receive the remaining complete frames.
pub(crate) async fn drain_writer(
    mut writer: tokio::task::JoinHandle<io::Result<()>>,
) -> io::Result<()> {
    match tokio::time::timeout(DRAIN_GRACE, &mut writer).await {
        Ok(result) => result.map_err(io::Error::other)?,
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "host did not drain bridge output within the 1 second shutdown grace period",
            ))
        }
    }
}

/// Forwards each dispatched call to the host and waits for its correlated reply.
struct HostProvider {
    outbox: mpsc::UnboundedSender<Outbound>,
    pending: Pending,
    counters: Arc<Counters>,
}

/// Forget a call whose provider future was dropped, so a late reply is ignored.
struct Forget<'a>(&'a Pending, u64);
impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.1);
    }
}

impl ToolProvider for HostProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call.id, (sender, context.facts));
        let _forget = Forget(&self.pending, call.id);
        let frame = encode(&BridgeFrame::Call {
            id: call.id,
            tool: &call.tool,
            arguments: &call.arguments,
        });
        let undelivered = |reason: String| ProviderError {
            code: "bridge_unavailable".into(),
            message: format!("call was not delivered to the host: {reason}"),
        };
        let frame = frame.map_err(|error| undelivered(error.to_string()))?;
        let length = frame.len() as u64;
        // Failure here means the writer stopped before this frame was queued.
        self.outbox
            .send(Outbound::Frame(frame))
            .map_err(|_| undelivered("bridge output is closed".into()))?;
        self.counters.to_host.fetch_add(length, Ordering::Relaxed);
        match receiver.await {
            Ok(reply) => reply,
            // No reply can arrive; the run's deadline or cancellation records it as unknown.
            Err(_) => std::future::pending().await,
        }
    }
}

fn validate_tools(tools: &[String]) -> Result<(), BridgeError> {
    if tools.len() > MAX_TOOLS {
        return Err(BridgeError::Protocol(format!(
            "start frame lists more than {MAX_TOOLS} tools"
        )));
    }
    let mut seen = HashSet::new();
    for tool in tools {
        if tool.is_empty() || tool.len() > MAX_TOOL_NAME_BYTES {
            return Err(BridgeError::Protocol(format!(
                "tool names must contain 1 to {MAX_TOOL_NAME_BYTES} bytes"
            )));
        }
        if !seen.insert(tool) {
            return Err(BridgeError::Protocol(format!(
                "duplicate tool name '{tool}'"
            )));
        }
    }
    Ok(())
}

fn reply(
    ok: bool,
    value: Option<Value>,
    error: Option<ProviderError>,
    too_large: bool,
) -> Option<Reply> {
    match (ok, value, error, too_large) {
        (true, Some(value), None, false) => Some(Ok(value)),
        (false, None, Some(error), false) => Some(Err(error)),
        // Larger than any guest reply frame, so the runtime's own limit path records
        // `result_too_large` and stops the run, instead of a catchable failure.
        (true, None, None, true) => Some(Ok(Value::String(" ".repeat(REPLY_FRAME_BYTES)))),
        (false, None, None, true) => Some(Err(ProviderError {
            code: "result_too_large".into(),
            message: " ".repeat(REPLY_FRAME_BYTES),
        })),
        _ => None,
    }
}

/// Route host replies and cancellation until the input closes or becomes invalid.
async fn route<R: AsyncRead + Unpin>(
    mut input: BufReader<R>,
    pending: Pending,
    counters: Arc<Counters>,
    cancellation: RunCancellation,
    failure: Arc<Mutex<Option<(RunStatus, String)>>>,
) {
    let mut partial = Vec::new();
    let reason = loop {
        let bytes = match frame(&mut input, &mut partial, BRIDGE_FRAME_BYTES).await {
            Ok(Some(bytes)) => {
                counters
                    .from_host
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                bytes
            }
            Ok(None) => break (RunStatus::Cancelled, "host closed the bridge input".into()),
            Err(error) => break (RunStatus::ProtocolError, format!("host frame: {error}")),
        };
        match serde_json::from_slice::<HostFrame>(&bytes) {
            Ok(HostFrame::Cancel) => {
                cancellation.cancel();
                return;
            }
            Ok(HostFrame::Result {
                id,
                ok,
                value,
                error,
                too_large,
                bytes,
            }) => {
                let reply =
                    reply(ok, value, error, too_large).filter(|_| too_large || bytes.is_none());
                let Some(reply) = reply else {
                    break (
                        RunStatus::ProtocolError,
                        "host result needs ok with value or too_large, or not ok with error".into(),
                    );
                };
                let entry = pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if let Some((sender, _)) = entry {
                    if too_large {
                        counters
                            .too_large
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(id, bytes);
                    }
                    let _ = sender.send(reply);
                }
            }
            Ok(HostFrame::Fact { id, kind, host_ms }) => {
                // Sent before the result is, so the run loop sees the fact first.
                match pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&id)
                {
                    Some((_, facts)) => facts.report(id, kind, host_ms),
                    None => {
                        counters.facts_ignored.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(HostFrame::Start { .. }) => {
                break (RunStatus::ProtocolError, "duplicate start frame".into());
            }
            Err(error) => {
                break (
                    RunStatus::ProtocolError,
                    format!("invalid host frame: {error}"),
                );
            }
        }
    };
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    cancellation.cancel();
}

/// Serve one run whose tools are executed by the host on the other end of `input` and `output`.
///
/// `config` supplies limits and the worker executable; its `allowed_tools` are replaced
/// by the start frame's list. The worker starts after the start frame, and the run's
/// deadline, which includes worker startup, starts with it. Cancelling `cancellation` behaves like a host `cancel`
/// frame. Dispatched calls without an observed host reply are recorded as unknown.
///
/// # Examples
///
/// ```no_run
/// use codemode::{RunCancellation, RunConfig, serve_bridge};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = RunConfig { worker: Some("/usr/local/bin/codemode".into()), ..RunConfig::default() };
/// let report = serve_bridge(tokio::io::stdin(), tokio::io::stdout(),
///                           config, RunCancellation::default()).await?;
/// println!("{:?}", report.status);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// Returns [`BridgeError`] when the start frame is missing or invalid, the run cannot
/// start, or the output stream fails. An `error` frame is written first when possible.
/// Host protocol violations after start end the run with a report instead.
/// After execution ends, output has a one-second drain grace period. A stalled
/// reader returns [`BridgeError::Io`] with [`io::ErrorKind::TimedOut`]; the final
/// report cannot be guaranteed when the peer stops reading.
pub async fn serve_bridge<R, W>(
    input: R,
    output: W,
    mut config: RunConfig,
    cancellation: RunCancellation,
) -> Result<RunReport, BridgeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outbox, frames) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_frames(output, frames, cancellation.clone()));
    let result = start_and_run(input, outbox.clone(), &mut config, cancellation).await;
    let last = match &result {
        Ok(report) => Some(encode(&BridgeFrame::Report { report })?),
        Err(BridgeError::Io(_)) => None,
        Err(error) => Some(encode(&BridgeFrame::Error {
            message: &error.to_string(),
        })?),
    };
    if let Some(bytes) = last {
        let _ = outbox.send(Outbound::Last(bytes));
    }
    drop(outbox);
    drain_writer(writer).await?;
    result
}

async fn start_and_run<R>(
    input: R,
    outbox: mpsc::UnboundedSender<Outbound>,
    config: &mut RunConfig,
    cancellation: RunCancellation,
) -> Result<RunReport, BridgeError>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // A waiting bridge still stops on cancellation (SIGTERM), so an unused one cannot linger.
    let mut input = BufReader::new(input);
    let mut partial = Vec::new();
    let first = tokio::select! {
        first = frame(&mut input, &mut partial, BRIDGE_FRAME_BYTES) => first,
        () = cancellation.cancelled() => {
            return Err(BridgeError::Protocol("cancelled before the start frame".into()));
        }
    };
    let bytes = match first {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(BridgeError::Protocol("input closed before start".into())),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return Err(BridgeError::Protocol(error.to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    let (code, tools) = match serde_json::from_slice::<HostFrame>(&bytes) {
        Ok(HostFrame::Start {
            version: BRIDGE_PROTOCOL_VERSION,
            code,
            tools,
        }) => (code, tools),
        Ok(HostFrame::Start { version, .. }) => {
            return Err(BridgeError::Protocol(format!(
                "unsupported protocol version {version}; expected {BRIDGE_PROTOCOL_VERSION}"
            )));
        }
        Ok(_) => return Err(BridgeError::Protocol("first frame must be start".into())),
        Err(error) => {
            return Err(BridgeError::Protocol(format!(
                "invalid start frame: {error}"
            )));
        }
    };
    validate_tools(&tools)?;
    config.allowed_tools = tools;

    let pending = Pending::default();
    let counters = Arc::new(Counters::default());
    counters
        .from_host
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    let failure = Arc::new(Mutex::new(None));
    let router = tokio::spawn(route(
        input,
        Arc::clone(&pending),
        Arc::clone(&counters),
        cancellation.clone(),
        Arc::clone(&failure),
    ));
    let provider = HostProvider {
        outbox,
        pending,
        counters: Arc::clone(&counters),
    };
    let result = run(&code, config.clone(), provider, cancellation).await;
    router.abort();
    let mut report = result.map_err(BridgeError::Run)?;
    report.bridge = Some(BridgeTotals {
        from_host: counters.from_host.load(Ordering::Relaxed),
        to_host: counters.to_host.load(Ordering::Relaxed),
    });
    report.provider_facts_dropped += counters.facts_ignored.load(Ordering::Relaxed);
    // A `too_large` reply reached the runtime as a placeholder; report the host's
    // size instead, or none when the host did not give one.
    for (id, bytes) in counters
        .too_large
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
    {
        let record = usize::try_from(id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| report.calls.get_mut(index))
            .filter(|record| record.outcome == CallOutcome::ResultTooLarge);
        if let Some(record) = record {
            let placeholder = record.result_bytes.unwrap_or(0);
            report.bytes.omitted =
                report.bytes.omitted.saturating_sub(placeholder) + bytes.unwrap_or(0);
            record.result_bytes = bytes;
        }
    }
    // A cancellation caused by the bridge input explains itself in the report.
    if report.status == RunStatus::Cancelled
        && let Some((status, message)) = failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    {
        report.status = status;
        report.diagnostic = Some(message);
        // Calls this ended carry the bridge's cause, not the cancellation it used.
        for event in &mut report.events {
            if event.reason.as_deref() == Some(RunStatus::Cancelled.as_str()) {
                event.reason = Some(status.as_str().into());
            }
        }
    }
    Ok(report)
}
