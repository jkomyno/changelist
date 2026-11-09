//! Plan mode of the host bridge: a parent host is the trusted provider of an
//! approved plan's session, while Rust keeps the journal.
//!
//! [`serve_plan_bridge`] runs one [`run_session`] whose [`RecoverableProvider`]
//! forwards authorization checks, attempts, and reconciliation probes to the host
//! over stdio. The host keeps its registry, permission gates, and credentials; Rust
//! checks the approval, records each attempt's intent durably before its `call`
//! frame is written, records what the host observed, and decides what may happen next.
//!
//! Protocol version 3 uses LF-terminated JSON frames; host frames are at most
//! [`BRIDGE_FRAME_BYTES`]. Host to bridge:
//!
//! - `{"type":"start","version":3,"declaration":{"provider":..,"account":..,"contracts":{..}}}`
//!   must be first. The declaration is the trusted host's identity, recovery
//!   contracts, and optional `"retention"` policy (`"metadata/1"` when omitted;
//!   see [`Retention`](crate::Retention)); the session is refused unless the
//!   approval binds exactly it.
//! - `{"type":"authorized","id":1,"ok":true}`, or `"ok":false` with
//!   `"error":{"code","message"}`, answers an `authorize` frame.
//! - `{"type":"result","id":2,"ok":true,"value":...}`, or `"ok":false` with
//!   `"error":{..}` and an optional `"effect":"refused"|"not_applied"|"unknown"`,
//!   answers a `call` frame. Without `effect` a failure is `unknown`: a host error
//!   is never proof that nothing happened.
//! - `{"type":"reconciled","id":3,"verdict":"applied","evidence":"..."}`, or
//!   `{"type":"reconciled","id":3,"error":{..}}`, answers a `reconcile` frame.
//! - `{"type":"cancel"}` cancels the session. Closing the input does too.
//!
//! Bridge to host:
//!
//! - `{"type":"authorize","id":1,"scope":{..}}` for each scope the plan uses,
//!   before anything is reconciled or dispatched.
//! - `{"type":"call","id":2,"tool":..,"arguments":{..},"operation":"..","attempt":"..","preconditions":[..],"scope":{..}}`
//!   for each attempt, after its intent is durable. `arguments` are the frozen,
//!   approved arguments; `attempt` is the attempt key the host should hand to a
//!   provider that keeps receipts.
//! - `{"type":"reconcile","id":3,"operation":"..","tool":..,"arguments":{..},"keys":[..],"scope":{..}}`
//!   for each operation whose outcome is unknown and whose contract can reconcile it.
//! - `{"type":"outcome","outcome":{..}}` once, last. A refused session has
//!   `{"session":n,"status":"refused","detail":".."}` as its outcome. When host
//!   input ended the session early (closed input or a malformed frame), the frame
//!   also carries `"diagnostic"`.
//! - `{"type":"error","message":".."}` instead, when no session could run.
//!
//! A reply that never comes leaves an attempt pending until the session deadline
//! (`--deadline-ms`) or cancellation records it as unknown; an unanswered
//! `authorize` or `reconcile` frame waits at most until that deadline too.
//! Replies for unknown or finished requests are ignored, because they can race
//! with a deadline.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::bridge::{BRIDGE_FRAME_BYTES, Outbound, drain_writer, encode, present, write_frames};
use crate::executor::{SessionError, SessionOptions, SessionOutcome, run_session};
use crate::journal::Journal;
use crate::plan::{Operation, Scope};
use crate::recovery::{
    Attempt, AttemptError, FailureEffect, ProbeContext, ProviderDeclaration, Reconciliation,
    RecoverableProvider, Verdict,
};
use crate::runtime::{CallContext, ProviderError, RunCancellation, frame};

/// The bridge protocol version that serves plan sessions.
pub const PLAN_BRIDGE_PROTOCOL_VERSION: u32 = 3;

/// Failure of a plan bridge session or delivery of its final frame.
#[derive(Debug)]
pub enum PlanBridgeError {
    /// The host sent an invalid or missing start frame; no session began.
    Protocol(String),
    /// The session could not run, or was refused (its outcome frame says so).
    Session(SessionError),
    /// Reading or writing the bridge streams failed.
    Io(io::Error),
}

impl fmt::Display for PlanBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => write!(formatter, "bridge protocol error: {message}"),
            Self::Session(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "bridge I/O failed: {error}"),
        }
    }
}

impl std::error::Error for PlanBridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for PlanBridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum HostFrame {
    Start {
        version: u32,
        declaration: ProviderDeclaration,
    },
    Authorized {
        id: u64,
        ok: bool,
        #[serde(default)]
        error: Option<ProviderError>,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(default, deserialize_with = "present")]
        value: Option<Value>,
        #[serde(default)]
        error: Option<ProviderError>,
        #[serde(default)]
        effect: Option<FailureEffect>,
    },
    Reconciled {
        id: u64,
        #[serde(default)]
        verdict: Option<Verdict>,
        #[serde(default)]
        evidence: Option<String>,
        #[serde(default)]
        error: Option<ProviderError>,
    },
    Cancel,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BridgeFrame<'a> {
    Authorize {
        id: u64,
        scope: &'a Scope,
    },
    Call {
        id: u64,
        tool: &'a str,
        arguments: &'a Value,
        operation: &'a str,
        attempt: &'a str,
        preconditions: &'a [Value],
        scope: &'a Scope,
    },
    Reconcile {
        id: u64,
        operation: &'a str,
        tool: &'a str,
        arguments: &'a Value,
        keys: &'a [String],
        scope: &'a Scope,
    },
    Outcome {
        outcome: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostic: Option<&'a str>,
    },
    Error {
        message: &'a str,
    },
}

/// Where to deliver the host's answer to one request.
enum Waiter {
    Authorize(oneshot::Sender<Result<(), ProviderError>>),
    Call(oneshot::Sender<Result<Value, AttemptError>>),
    Reconcile(oneshot::Sender<Result<Reconciliation, ProviderError>>),
}

type Pending = Arc<Mutex<HashMap<u64, Waiter>>>;

/// Forget a request whose future was dropped, so a late reply is ignored.
struct Forget<'a>(&'a Pending, u64);
impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.1);
    }
}

/// The host as a [`RecoverableProvider`]: every request becomes a frame, and its
/// future resolves with the correlated reply.
struct HostPlanProvider {
    declaration: ProviderDeclaration,
    outbox: mpsc::UnboundedSender<Outbound>,
    pending: Pending,
    next_id: AtomicU64,
}

fn bridge_error(code: &str, message: impl Into<String>) -> ProviderError {
    ProviderError {
        code: code.into(),
        message: message.into(),
    }
}

impl HostPlanProvider {
    /// Register `waiter` under a fresh id, then queue the frame `build` encodes for
    /// that id. The guard forgets the id when the request's future ends.
    fn send_with(
        &self,
        waiter: Waiter,
        build: impl FnOnce(u64) -> io::Result<Vec<u8>>,
    ) -> Result<Forget<'_>, ProviderError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Registered first, so a fast reply finds its waiter.
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, waiter);
        let guard = Forget(&self.pending, id);
        let frame =
            build(id).map_err(|error| bridge_error("bridge_unavailable", error.to_string()))?;
        self.outbox
            .send(Outbound::Frame(frame))
            .map_err(|_| bridge_error("bridge_unavailable", "bridge output is closed"))?;
        Ok(guard)
    }

    /// Wait for a reply, or for cancellation: authorization and probes are not
    /// bounded by a dispatch run, so they must not outlive the session's cancellation.
    /// The executor bounds them by the session deadline.
    async fn answer<T>(
        receiver: oneshot::Receiver<Result<T, ProviderError>>,
        cancellation: &RunCancellation,
    ) -> Result<T, ProviderError> {
        tokio::select! {
            reply = receiver => reply.unwrap_or_else(|_| {
                Err(bridge_error("bridge_unavailable", "the host's reply was rejected"))
            }),
            () = cancellation.cancelled() => {
                Err(bridge_error("cancelled", "the session was cancelled"))
            }
        }
    }
}

impl RecoverableProvider for HostPlanProvider {
    fn declaration(&self) -> ProviderDeclaration {
        self.declaration.clone()
    }

    async fn authorize(&self, scope: &Scope, context: ProbeContext) -> Result<(), ProviderError> {
        let (sender, receiver) = oneshot::channel();
        let _guard = self.send_with(Waiter::Authorize(sender), |id| {
            encode(&BridgeFrame::Authorize { id, scope })
        })?;
        Self::answer(receiver, &context.cancellation).await
    }

    async fn execute(
        &self,
        attempt: Attempt,
        _context: CallContext,
    ) -> Result<Value, AttemptError> {
        let (sender, receiver) = oneshot::channel();
        let operation = &attempt.operation;
        let sent = self.send_with(Waiter::Call(sender), |id| {
            encode(&BridgeFrame::Call {
                id,
                tool: &operation.tool,
                arguments: &operation.arguments,
                operation: &operation.id,
                attempt: &attempt.key,
                preconditions: &operation.preconditions,
                scope: &operation.scope,
            })
        });
        let _guard = match sent {
            Ok(guard) => guard,
            // Never queued, so the host cannot have seen this attempt.
            Err(error) => {
                return Err(AttemptError {
                    error,
                    effect: FailureEffect::NotApplied,
                });
            }
        };
        match receiver.await {
            Ok(reply) => reply,
            // No reply can arrive; the session deadline or cancellation records it as unknown.
            Err(_) => std::future::pending().await,
        }
    }

    async fn reconcile(
        &self,
        operation: Operation,
        keys: Vec<String>,
        context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        let (sender, receiver) = oneshot::channel();
        let _guard = self.send_with(Waiter::Reconcile(sender), |id| {
            encode(&BridgeFrame::Reconcile {
                id,
                operation: &operation.id,
                tool: &operation.tool,
                arguments: &operation.arguments,
                keys: &keys,
                scope: &operation.scope,
            })
        })?;
        Self::answer(receiver, &context.cancellation).await
    }
}

/// Describe a host frame that did not parse, by position only: serde's messages
/// can quote the frame's values, and the diagnostic is printed by hosts.
fn unparsable(error: &serde_json::Error) -> String {
    let kind = format!("{:?} error", error.classify()).to_lowercase();
    match error.line() {
        // serde reports no position for some errors, such as an unknown enum value.
        0 => kind,
        line => format!("{kind} at line {line} column {}", error.column()),
    }
}

/// What the router should do after one host frame.
enum Flow {
    Continue,
    Cancel,
}

/// Deliver one host frame to its waiter. A reply for an unknown or finished request
/// is ignored; a reply of the wrong shape or kind is a protocol error.
fn deliver(frame: HostFrame, pending: &Pending) -> Result<Flow, String> {
    let take = |id: u64| {
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
    };
    let mismatch = |id: u64| format!("the reply to request {id} has the wrong type");
    match frame {
        HostFrame::Cancel => return Ok(Flow::Cancel),
        HostFrame::Start { .. } => return Err("duplicate start frame".into()),
        HostFrame::Authorized { id, ok, error } => {
            let reply = match (ok, error) {
                (true, None) => Ok(()),
                (false, Some(error)) => Err(error),
                _ => return Err("authorized needs ok, or not ok with error".into()),
            };
            match take(id) {
                None => {}
                Some(Waiter::Authorize(sender)) => {
                    let _ = sender.send(reply);
                }
                Some(_) => return Err(mismatch(id)),
            }
        }
        HostFrame::Result {
            id,
            ok,
            value,
            error,
            effect,
        } => {
            let reply = match (ok, value, error, effect) {
                (true, Some(value), None, None) => Ok(value),
                (false, None, Some(error), effect) => Err(AttemptError {
                    error,
                    // A host error is never proof that nothing happened.
                    effect: effect.unwrap_or(FailureEffect::Unknown),
                }),
                _ => {
                    return Err(
                        "result needs ok with value, or not ok with error and an optional effect"
                            .into(),
                    );
                }
            };
            match take(id) {
                None => {}
                Some(Waiter::Call(sender)) => {
                    let _ = sender.send(reply);
                }
                Some(_) => return Err(mismatch(id)),
            }
        }
        HostFrame::Reconciled {
            id,
            verdict,
            evidence,
            error,
        } => {
            let reply = match (verdict, evidence, error) {
                (Some(verdict), Some(evidence), None) => Ok(Reconciliation { verdict, evidence }),
                (None, None, Some(error)) => Err(error),
                _ => return Err("reconciled needs verdict with evidence, or error".into()),
            };
            match take(id) {
                None => {}
                Some(Waiter::Reconcile(sender)) => {
                    let _ = sender.send(reply);
                }
                Some(_) => return Err(mismatch(id)),
            }
        }
    }
    Ok(Flow::Continue)
}

/// Why host input ended the session, if it did.
type Failure = Arc<Mutex<Option<String>>>;

/// Route host replies and cancellation until the input closes or becomes invalid.
async fn route<R: AsyncRead + Unpin>(
    mut input: BufReader<R>,
    pending: Pending,
    cancellation: RunCancellation,
    failure: Failure,
) {
    let mut partial = Vec::new();
    let reason = loop {
        let bytes = match frame(&mut input, &mut partial, BRIDGE_FRAME_BYTES).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break "host closed the bridge input".to_string(),
            Err(error) => break format!("host frame: {error}"),
        };
        let parsed = match serde_json::from_slice::<HostFrame>(&bytes) {
            Ok(parsed) => parsed,
            Err(error) => break format!("invalid host frame: {}", unparsable(&error)),
        };
        match deliver(parsed, &pending) {
            Ok(Flow::Continue) => {}
            Ok(Flow::Cancel) => {
                cancellation.cancel();
                return;
            }
            Err(message) => break message,
        }
    };
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    cancellation.cancel();
}

/// Serve one plan session whose provider is the host on the other end of `input`
/// and `output`; see the module documentation for the protocol.
///
/// `journal` is the journal the session runs against; `options.kind` selects `run`
/// or `resume`. Cancelling `cancellation` behaves like a host `cancel` frame.
///
/// # Examples
///
/// ```no_run
/// use codemode::{Durability, Journal, RunCancellation, SessionOptions, serve_plan_bridge};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let journal = Journal::open("plan.journal", Durability::Storage)?;
/// let outcome = serve_plan_bridge(tokio::io::stdin(), tokio::io::stdout(), journal,
///                                 SessionOptions::default(), RunCancellation::default()).await?;
/// println!("{}", outcome.status);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
/// Returns [`PlanBridgeError::Protocol`] for a missing or invalid start frame,
/// [`PlanBridgeError::Session`] when [`run_session`] fails or refuses the session,
/// and [`PlanBridgeError::Io`] when the output fails. The last frame (`outcome`,
/// including a refusal, or `error`) is written first when possible.
/// After the session ends, output has a one-second drain grace period. A stalled
/// reader returns [`PlanBridgeError::Io`] with [`io::ErrorKind::TimedOut`]. The
/// journal retains the session even when its final frame cannot be delivered.
pub async fn serve_plan_bridge<R, W>(
    input: R,
    output: W,
    journal: Journal,
    options: SessionOptions,
    cancellation: RunCancellation,
) -> Result<SessionOutcome, PlanBridgeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outbox, frames) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_frames(output, frames, cancellation.clone()));
    let failure = Failure::default();
    let result = start_and_run(
        input,
        outbox.clone(),
        journal,
        options,
        cancellation,
        Arc::clone(&failure),
    )
    .await;
    let diagnostic = failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let outcome = |outcome: Value| {
        encode(&BridgeFrame::Outcome {
            outcome,
            diagnostic: diagnostic.as_deref(),
        })
    };
    let last = match &result {
        Ok(session) => Some(outcome(
            serde_json::to_value(session).map_err(io::Error::other)?,
        )?),
        Err(PlanBridgeError::Session(SessionError::Refused { session, reason })) => {
            Some(outcome(serde_json::json!({
                "session": session, "status": "refused", "detail": reason,
            }))?)
        }
        Err(PlanBridgeError::Io(_)) => None,
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
    journal: Journal,
    options: SessionOptions,
    cancellation: RunCancellation,
    failure: Failure,
) -> Result<SessionOutcome, PlanBridgeError>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut input = BufReader::new(input);
    let mut partial = Vec::new();
    let first = tokio::select! {
        first = frame(&mut input, &mut partial, BRIDGE_FRAME_BYTES) => first,
        () = cancellation.cancelled() => {
            return Err(PlanBridgeError::Protocol("cancelled before the start frame".into()));
        }
    };
    let bytes = match first {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(PlanBridgeError::Protocol(
                "input closed before start".into(),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return Err(PlanBridgeError::Protocol(error.to_string()));
        }
        Err(error) => return Err(error.into()),
    };
    let declaration = match serde_json::from_slice::<HostFrame>(&bytes) {
        Ok(HostFrame::Start {
            version: PLAN_BRIDGE_PROTOCOL_VERSION,
            declaration,
        }) => declaration,
        Ok(HostFrame::Start { version, .. }) => {
            return Err(PlanBridgeError::Protocol(format!(
                "unsupported protocol version {version}; plan sessions need {PLAN_BRIDGE_PROTOCOL_VERSION}"
            )));
        }
        Ok(_) => {
            return Err(PlanBridgeError::Protocol(
                "first frame must be start".into(),
            ));
        }
        Err(error) => {
            return Err(PlanBridgeError::Protocol(format!(
                "invalid start frame: {}",
                unparsable(&error)
            )));
        }
    };
    let pending = Pending::default();
    let router = tokio::spawn(route(
        input,
        Arc::clone(&pending),
        cancellation.clone(),
        failure,
    ));
    let provider = HostPlanProvider {
        declaration,
        outbox,
        pending,
        next_id: AtomicU64::new(1),
    };
    let result = run_session(journal, provider, options, cancellation).await;
    router.abort();
    result.map_err(PlanBridgeError::Session)
}
