//! Execute and recover an approved plan through the existing runtime.
//!
//! A session checks the latest approval against the current provider declaration
//! and policy, asks the provider whether each scope is still authorized,
//! reconciles operations whose outcome is unknown, and then dispatches every
//! operation whose next step is `dispatch`. One deadline bounds all of it.
//! Dispatch reuses [`run_calls`]: native requests name only operation
//! identities, Rust admits and schedules those calls under the approved
//! concurrency, and a journaling provider looks up each operation's frozen
//! arguments, records the intent durably, calls the trusted provider, and
//! records the observation durably before replying.
//!
//! Recovery never reruns an agent's program: it works only from the approved list.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

use crate::journal::{History, Journal, JournalError, SessionKind};
use crate::plan::{FrozenPlan, Operation};
use crate::recovery::{
    Approval, Assessment, Attempt, Fact, NextStep, Observed, ProbeContext, ReconcileMethod,
    RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety, Retention,
    approval_binding, assess, machine_code,
};
use crate::runtime::{
    CallContext, NativeCall, ProviderError, RunCancellation, RunConfig, RunError, RunReport,
    RunStatus, ToolCall, ToolProvider, millis, run_calls,
};

/// Process settings for a session that are not part of the approval.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// `run` for the first session, `resume` afterwards.
    pub kind: SessionKind,
    /// Total time for the session from its start: authorization, reconciliation
    /// probes, and dispatch, including any host permission wait inside a provider
    /// call. Not part of the approval, because stopping earlier can only reduce
    /// effects. Defaults to 300 seconds.
    pub deadline: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            kind: SessionKind::Run,
            deadline: Duration::from_secs(300),
        }
    }
}

/// An operation's identity, tool, and assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationStatus {
    /// Stable operation identity.
    pub operation: String,
    /// Provider tool.
    pub tool: String,
    /// What is known and what may happen next.
    #[serde(flatten)]
    pub assessment: Assessment,
}

/// Wall-clock phases of a session, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SessionTiming {
    /// Opening checks: approval, declaration, and authorization.
    pub checks_ms: u64,
    /// Reconciliation probes.
    pub reconcile_ms: u64,
    /// The native dispatch run; see the trace for its phases.
    pub dispatch_ms: u64,
    /// The whole session.
    pub total_ms: u64,
}

/// What a session did and where every operation stands afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct SessionOutcome {
    /// Session number in the journal.
    pub session: u64,
    /// `completed`, `needs_review`, `incomplete`, `interrupted`, or `refused`.
    pub status: String,
    /// Why a session was refused or interrupted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What stopped an `interrupted` session: [`RunStatus::Cancelled`],
    /// [`RunStatus::Deadline`], or another status that ended its dispatch run.
    #[serde(skip)]
    pub interruption: Option<RunStatus>,
    /// Operations charged for a probe in this session, including unanswered probes.
    pub probed: usize,
    /// Attempts this session recorded an intent for: dispatched, or withdrawn
    /// after the intent and before the provider saw them.
    pub dispatched: usize,
    /// Every operation, in plan order.
    pub operations: Vec<OperationStatus>,
    /// Phase timings.
    pub timing: SessionTiming,
    /// The dispatch run's execution record, also stored in the journal.
    #[serde(skip)]
    pub trace: Option<RunReport>,
}

/// Why a session could not run.
#[derive(Debug)]
pub enum SessionError {
    /// The journal could not be read or written.
    Journal(JournalError),
    /// The session was refused before any dispatch; the refusal is recorded.
    Refused {
        /// The recorded session.
        session: u64,
        /// Why.
        reason: String,
    },
    /// `run` was asked of a journal that already has sessions.
    AlreadyStarted,
    /// The dispatch run could not start; nothing was dispatched.
    Run(RunError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => error.fmt(formatter),
            Self::Refused { session, reason } => {
                write!(formatter, "session {session} refused: {reason}")
            }
            Self::AlreadyStarted => formatter
                .write_str("this plan already ran; inspect the journal and use resume to continue"),
            Self::Run(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<JournalError> for SessionError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// The contracts to judge operations by: the approved ones, or the most
/// conservative contract when nothing is approved.
fn contract_for(approval: Option<&Approval>, tool: &str) -> RecoveryContract {
    approval
        .and_then(|approval| approval.declaration.contracts.get(tool).copied())
        .unwrap_or(RecoveryContract {
            repeat: RepeatSafety::Unsafe,
            reconcile: ReconcileMethod::None,
        })
}

/// Assess every operation from a journal's history, in plan order.
///
/// Contracts and budgets come from the latest approval; without one, every
/// operation is judged as unrepeatable and unreconcilable under the default policy.
#[must_use]
pub fn assess_history(
    plan: &FrozenPlan,
    history: &History,
    probed_this_session: &HashSet<String>,
) -> Vec<OperationStatus> {
    let approval = history.approvals.last();
    let policy = approval.map(|approval| approval.policy).unwrap_or_default();
    let mut facts: HashMap<&str, Vec<(u64, Fact<'_>)>> = HashMap::new();
    for attempt in &history.attempts {
        let list = facts.entry(attempt.operation.as_str()).or_default();
        list.push((
            attempt.ordinal,
            Fact::Intent {
                number: attempt.number,
            },
        ));
        if let Some(observation) = &attempt.observation {
            list.push((
                observation.ordinal,
                Fact::Observation {
                    number: attempt.number,
                    observed: &observation.observed,
                },
            ));
        }
    }
    for probe in &history.probes {
        let verdict = probe.outcome.as_ref().ok().map(|outcome| outcome.verdict);
        facts
            .entry(probe.operation.as_str())
            .or_default()
            .push((probe.ordinal, Fact::Probe(verdict)));
    }
    for review in &history.reviews {
        facts
            .entry(review.operation.as_str())
            .or_default()
            .push((review.ordinal, Fact::Review(review.resolution)));
    }
    plan.plan()
        .operations
        .iter()
        .map(|operation| {
            let mut list = facts.remove(operation.id.as_str()).unwrap_or_default();
            list.sort_by_key(|(ordinal, _)| *ordinal);
            let list: Vec<Fact<'_>> = list.into_iter().map(|(_, fact)| fact).collect();
            OperationStatus {
                operation: operation.id.clone(),
                tool: operation.tool.clone(),
                assessment: assess(
                    &list,
                    contract_for(approval, &operation.tool),
                    &policy,
                    probed_this_session.contains(&operation.id),
                ),
            }
        })
        .collect()
}

/// The session's journal, and a count of journal writes queued on blocking threads.
///
/// A write keeps running after the runtime drops the call that queued it, so the
/// count lets the session wait for every such write before it records which
/// attempts were never observed.
#[derive(Clone)]
struct Shared {
    journal: Arc<Mutex<Journal>>,
    writes: Arc<(Mutex<usize>, Condvar)>,
}

/// Counts one queued write until it is dropped, after the write ran or was discarded.
struct QueuedWrite(Arc<(Mutex<usize>, Condvar)>);

impl Drop for QueuedWrite {
    fn drop(&mut self) {
        let (count, finished) = &*self.0;
        *count.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        finished.notify_all();
    }
}

impl Shared {
    fn new(journal: Journal) -> Self {
        Self {
            journal: Arc::new(Mutex::new(journal)),
            writes: Arc::default(),
        }
    }

    fn id(&self) -> String {
        self.journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .id()
            .to_string()
    }
}

/// Run `body` against the journal on a blocking thread.
async fn journaled<T: Send + 'static>(
    shared: &Shared,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    *shared
        .writes
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner) += 1;
    let queued = QueuedWrite(Arc::clone(&shared.writes));
    let journal = Arc::clone(&shared.journal);
    tokio::task::spawn_blocking(move || {
        let _queued = queued;
        let mut guard = journal.lock().unwrap_or_else(PoisonError::into_inner);
        body(&mut guard)
    })
    .await
    .map_err(|error| JournalError::Storage(error.to_string()))?
}

/// Like [`journaled`], but first waits until every queued write has finished.
async fn after_queued_writes<T: Send + 'static>(
    shared: &Shared,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let (count, finished) = &*shared.writes;
        let mut queued = count.lock().unwrap_or_else(PoisonError::into_inner);
        while *queued > 0 {
            queued = finished
                .wait(queued)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(queued);
        let mut guard = shared
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        body(&mut guard)
    })
    .await
    .map_err(|error| JournalError::Storage(error.to_string()))?
}

/// Run one session against `journal` with `provider`.
///
/// `options.deadline`, counted from the call, bounds authorization, probes, and
/// dispatch. Reached before an attempt's intent commits, it withdraws the
/// attempt; after, it drops the provider call and the attempt stays unknown
/// (`unobserved`). A probe it cuts off stays charged and is recorded as
/// `probe_timeout`; an authorization it cuts off ends the session before dispatch.
///
/// # Errors
/// Returns [`SessionError::Refused`] (already recorded in the journal) when no
/// approval matches the current plan, provider declaration, and policy, or a
/// scope is not authorized now; [`SessionError::AlreadyStarted`] for `run` on a
/// journal with sessions; [`SessionError::Run`] for a deadline too large to
/// represent; and journal or runtime failures. A dispatch run that ends early,
/// or cancellation or the deadline during authorization or reconciliation,
/// returns an `interrupted` outcome.
pub async fn run_session<P: RecoverableProvider>(
    journal: Journal,
    provider: P,
    options: SessionOptions,
    cancellation: RunCancellation,
) -> Result<SessionOutcome, SessionError> {
    let started = Instant::now();
    let deadline = started.checked_add(options.deadline).ok_or_else(|| {
        SessionError::Run(RunError::Config("the session deadline is too large".into()))
    })?;
    let history = journal.history()?;
    if options.kind == SessionKind::Run && !history.sessions.is_empty() {
        return Err(SessionError::AlreadyStarted);
    }
    let plan = journal.plan().clone();
    let journal = Shared::new(journal);
    let kind = options.kind;
    let session = journaled(&journal, move |journal| journal.begin_session(kind)).await?;
    let provider = Arc::new(provider);

    let approval = match check_approval(&plan, &history, provider.as_ref()) {
        Ok(approval) => approval,
        Err(reason) => return refuse(&journal, session, reason).await,
    };
    let policy = approval.policy;
    let mut interrupted = match authorize(&plan, provider.as_ref(), deadline, &cancellation).await {
        Err(AuthorizationError::Refused(reason)) => return refuse(&journal, session, reason).await,
        Err(AuthorizationError::Stopped(status)) => Some(status),
        Ok(()) => None,
    };
    let checks_ms = millis(started.elapsed());

    let reconcile_started = Instant::now();
    let statuses = assess_history(&plan, &history, &HashSet::new());
    let to_probe: Vec<&OperationStatus> = statuses
        .iter()
        .filter(|status| status.assessment.next == NextStep::Reconcile)
        .collect();
    let retention = approval.declaration.retention;
    let probed = if interrupted.is_some() {
        HashSet::new()
    } else {
        probe(
            &journal,
            &plan,
            &history,
            &to_probe,
            &provider,
            &policy,
            retention,
            session,
            deadline,
            &cancellation,
        )
        .await?
    };
    let reconcile_ms = millis(reconcile_started.elapsed());

    // Include unanswered probe starts: cancellation does not refund their budget.
    let history = if probed.is_empty() {
        history
    } else {
        journaled(&journal, |journal| journal.history()).await?
    };
    let statuses = assess_history(&plan, &history, &probed);
    let dispatch: Vec<(Operation, u32)> = statuses
        .iter()
        .filter(|status| status.assessment.next == NextStep::Dispatch)
        .filter_map(|status| {
            plan.operation(&status.operation)
                .map(|operation| (operation.clone(), status.assessment.attempts + 1))
        })
        .collect();

    let dispatch_started = Instant::now();
    let mut trace = None;
    interrupted = interrupted.or_else(|| stopped(&cancellation, deadline));
    if interrupted.is_none() && !dispatch.is_empty() {
        let report = dispatch_run(
            &journal,
            &provider,
            dispatch,
            &policy,
            retention,
            session,
            deadline,
            &cancellation,
        )
        .await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                let detail = error.to_string();
                journaled(&journal, move |journal| {
                    journal.end_session(session, "error", Some(&detail), None)
                })
                .await?;
                return Err(SessionError::Run(error));
            }
        };
        if !report.status.is_success() {
            interrupted = Some(report.status);
        }
        trace = Some(report);
    }
    let dispatch_ms = millis(dispatch_started.elapsed());

    // An attempt the run stopped waiting for keeps an unknown outcome, with the reason.
    let reason = interrupted.map_or("run_finished", RunStatus::as_str);
    // Writes the run abandoned may still commit; wait for them so a recorded result
    // is never replaced by "unobserved", and a late intent is still marked.
    let history = after_queued_writes(&journal, move |journal| {
        journal.record_unobserved(session, reason)?;
        journal.history()
    })
    .await?;
    let operations = assess_history(&plan, &history, &probed);
    let dispatched = history
        .attempts
        .iter()
        .filter(|attempt| attempt.session == session)
        .count();
    let status = match &interrupted {
        Some(_) => "interrupted",
        None if operations
            .iter()
            .all(|status| status.assessment.next == NextStep::Done) =>
        {
            "completed"
        }
        None if operations
            .iter()
            .all(|status| matches!(status.assessment.next, NextStep::Done | NextStep::Review)) =>
        {
            "needs_review"
        }
        None => "incomplete",
    }
    .to_string();
    let detail = interrupted.map(|status| {
        let status = status.as_str();
        if trace.is_some() {
            format!("the dispatch run ended with {status}")
        } else {
            format!("the session ended with {status} before dispatch")
        }
    });
    let stored = (status.clone(), detail.clone(), trace.clone());
    journaled(&journal, move |journal| {
        let (status, detail, trace) = stored;
        journal.end_session(session, &status, detail.as_deref(), trace.as_ref())
    })
    .await?;
    Ok(SessionOutcome {
        session,
        status,
        detail,
        interruption: interrupted,
        probed: probed.len(),
        dispatched,
        operations,
        timing: SessionTiming {
            checks_ms,
            reconcile_ms,
            dispatch_ms,
            total_ms: millis(started.elapsed()),
        },
        trace,
    })
}

async fn refuse(
    journal: &Shared,
    session: u64,
    reason: String,
) -> Result<SessionOutcome, SessionError> {
    let detail = reason.clone();
    journaled(journal, move |journal| {
        journal.end_session(session, "refused", Some(&detail), None)
    })
    .await?;
    Err(SessionError::Refused { session, reason })
}

/// The latest approval, if it binds the journal's plan to the provider's current
/// declaration and a valid policy.
fn check_approval(
    plan: &FrozenPlan,
    history: &History,
    provider: &impl RecoverableProvider,
) -> Result<Approval, String> {
    let Some(approval) = history.approvals.last() else {
        return Err("the plan has not been approved".into());
    };
    approval
        .policy
        .validate()
        .map_err(|error| error.to_string())?;
    let current = provider.declaration();
    current.admits(plan).map_err(|error| error.to_string())?;
    let current = current.for_plan(plan);
    let approved = &approval.declaration;
    if current.provider != approved.provider || current.account != approved.account {
        return Err(format!(
            "approved for {}/{}, but the provider now acts as {}/{}",
            approved.provider, approved.account, current.provider, current.account
        ));
    }
    if current.retention != approved.retention {
        return Err(format!(
            "the provider's retention policy changed since approval (approved {}, now {}); \
             review and approve again",
            approved.retention, current.retention
        ));
    }
    if current.contracts != approved.contracts {
        return Err(
            "the provider's recovery contracts changed since approval; review and approve again"
                .into(),
        );
    }
    let binding = approval_binding(plan.digest(), &current, &approval.policy);
    if binding != approval.binding || &approval.plan != plan.digest() {
        return Err("the approval does not bind this plan, provider, and policy".into());
    }
    Ok(approval.clone())
}

/// Why the session must stop now, if it must: cancellation, then the deadline.
fn stopped(cancellation: &RunCancellation, deadline: Instant) -> Option<RunStatus> {
    if cancellation.is_cancelled() {
        Some(RunStatus::Cancelled)
    } else if Instant::now() >= deadline {
        Some(RunStatus::Deadline)
    } else {
        None
    }
}

enum AuthorizationError {
    /// Cancellation or the deadline ended the session before every scope answered.
    Stopped(RunStatus),
    Refused(String),
}

/// Ask the provider whether every scope the plan uses is authorized now.
async fn authorize(
    plan: &FrozenPlan,
    provider: &impl RecoverableProvider,
    deadline: Instant,
    cancellation: &RunCancellation,
) -> Result<(), AuthorizationError> {
    let scopes: BTreeSet<_> = plan
        .plan()
        .operations
        .iter()
        .map(|operation| &operation.scope)
        .collect();
    for scope in scopes {
        if let Some(status) = stopped(cancellation, deadline) {
            return Err(AuthorizationError::Stopped(status));
        }
        let context = ProbeContext {
            deadline,
            cancellation: cancellation.clone(),
        };
        let outcome = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(AuthorizationError::Stopped(RunStatus::Cancelled));
            }
            outcome = time::timeout_at(deadline, provider.authorize(scope, context)) => outcome,
        };
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(AuthorizationError::Refused(format!(
                    "the provider refused authorization for {scope} ({})",
                    machine_code(&error.code)
                )));
            }
            // Not a refusal: no answer is not evidence that the scope is unauthorized.
            Err(_) => return Err(AuthorizationError::Stopped(RunStatus::Deadline)),
        }
    }
    Ok(())
}

/// Probe each operation once, at most `policy.concurrency` at a time, and record
/// every answer. A probe still running at `deadline` is recorded as
/// `probe_timeout`; none starts after it. Returns the operations charged for a
/// probe, including those cancelled before an answer was recorded.
#[expect(clippy::too_many_arguments, reason = "one internal call site")]
async fn probe<P: RecoverableProvider>(
    journal: &Shared,
    plan: &FrozenPlan,
    history: &History,
    statuses: &[&OperationStatus],
    provider: &Arc<P>,
    policy: &RecoveryPolicy,
    retention: Retention,
    session: u64,
    deadline: Instant,
    cancellation: &RunCancellation,
) -> Result<HashSet<String>, JournalError> {
    let mut probed = HashSet::new();
    let mut pending = statuses.iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < policy.concurrency && stopped(cancellation, deadline).is_none() {
            let Some(status) = pending.next() else { break };
            let Some(operation) = plan.operation(&status.operation).cloned() else {
                continue;
            };
            let keys: Vec<String> = history
                .attempts
                .iter()
                .filter(|attempt| attempt.operation == operation.id)
                .map(|attempt| attempt.key.clone())
                .collect();

            // Charged before the provider is asked, so a crash loop cannot probe forever.
            let start = (operation.id.clone(), keys.clone());
            let probe = journaled(journal, move |journal| {
                let (operation, keys) = start;
                journal.record_probe_start(&operation, session, &keys)
            })
            .await?;
            probed.insert(operation.id.clone());
            if cancellation.is_cancelled() {
                break;
            }
            // Charged, so the deadline answers it if the provider does not.
            let provider = Arc::clone(provider);
            let context = ProbeContext {
                deadline,
                cancellation: cancellation.clone(),
            };
            running.spawn(async move {
                let started = Instant::now();
                let cancellation = context.cancellation.clone();
                let outcome = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return None,
                    outcome = time::timeout_at(deadline, provider.reconcile(operation, keys, context)) => {
                        outcome.unwrap_or_else(|_| {
                            Err(ProviderError {
                                code: "probe_timeout".into(),
                                message: "the reconciliation probe did not answer by the session deadline".into(),
                            })
                        })
                    }
                };
                Some((probe, millis(started.elapsed()), outcome))
            });
        }
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                running.abort_all();
                while running.join_next().await.is_some() {}
                break;
            }
            result = running.join_next() => result,
        };
        let Some(result) = result else { break };
        let Some((probe, elapsed, outcome)) =
            result.map_err(|error| JournalError::Storage(error.to_string()))?
        else {
            continue;
        };
        journaled(journal, move |journal| {
            journal.record_probe(probe, elapsed, &outcome, retention)
        })
        .await?;
    }
    Ok(probed)
}

/// Dispatch `operations` in one native run that ends by the session's `deadline`.
#[expect(clippy::too_many_arguments, reason = "one internal call site")]
async fn dispatch_run<P: RecoverableProvider>(
    journal: &Shared,
    provider: &Arc<P>,
    operations: Vec<(Operation, u32)>,
    policy: &RecoveryPolicy,
    retention: Retention,
    session: u64,
    deadline: Instant,
    cancellation: &RunCancellation,
) -> Result<RunReport, RunError> {
    let calls: Vec<NativeCall> = operations
        .iter()
        .map(|(operation, _)| NativeCall {
            tool: operation.tool.clone(),
            arguments: json!({"operation": operation.id}),
        })
        .collect();
    let tools: BTreeSet<String> = operations
        .iter()
        .map(|(operation, _)| operation.tool.clone())
        .collect();
    // The time left, rounded down to whole milliseconds; at least 1 ms, because
    // the caller found time left just before.
    let remaining = millis(deadline.saturating_duration_since(Instant::now())).max(1);
    let config = RunConfig {
        timeout_ms: remaining,
        max_calls: operations.len(),
        max_concurrency: policy.concurrency,
        allowed_tools: tools.into_iter().collect(),
        ..RunConfig::default()
    };
    let journal_id = journal.id();
    let entries = operations
        .into_iter()
        .map(|(operation, number)| {
            let key = format!("{journal_id}-{}-{number}", operation.id);
            (operation.id.clone(), (operation, number, key))
        })
        .collect();
    let journaling = JournalingProvider {
        journal: journal.clone(),
        provider: Arc::clone(provider),
        entries,
        dispatched: Mutex::new(HashSet::new()),
        session,
        retention,
        cancellation: cancellation.clone(),
    };
    run_calls(&calls, config, journaling, cancellation.clone()).await
}

/// The runtime's provider for a dispatch run: it journals each attempt around the
/// trusted provider's call.
struct JournalingProvider<P> {
    journal: Shared,
    provider: Arc<P>,
    /// Operation identity -> (operation, attempt number, attempt key).
    entries: HashMap<String, (Operation, u32, String)>,
    dispatched: Mutex<HashSet<String>>,
    session: u64,
    /// What the journal keeps of the provider's replies, from the approval.
    retention: Retention,
    cancellation: RunCancellation,
}

fn refusal(code: &str, message: &str) -> ProviderError {
    ProviderError {
        code: code.into(),
        message: message.into(),
    }
}

impl<P: RecoverableProvider> JournalingProvider<P> {
    /// The approved entry the guest named, dispatched at most once per session.
    fn claim(&self, call: &ToolCall) -> Result<(Operation, u32, String), ProviderError> {
        let id = call
            .arguments
            .as_object()
            .filter(|arguments| arguments.len() == 1)
            .and_then(|arguments| arguments.get("operation"))
            .and_then(Value::as_str)
            .ok_or_else(|| refusal("plan_violation", "a call must name one operation"))?;
        let entry = self
            .entries
            .get(id)
            .filter(|(operation, _, _)| operation.tool == call.tool)
            .ok_or_else(|| {
                refusal(
                    "plan_violation",
                    "the operation is not approved for dispatch now",
                )
            })?;
        let fresh = self
            .dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_string());
        if !fresh {
            return Err(refusal(
                "plan_violation",
                "the operation was already dispatched in this session",
            ));
        }
        Ok(entry.clone())
    }

    /// Stop the run: the journal can no longer record what happens.
    fn journal_failed(&self, error: &JournalError, when: &str) -> ProviderError {
        self.cancellation.cancel();
        refusal(
            "journal_unavailable",
            &format!("{when}; the run was stopped: {error}"),
        )
    }
}

impl<P: RecoverableProvider> ToolProvider for JournalingProvider<P> {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let (operation, number, key) = self.claim(&call)?;
        let id = operation.id.clone();
        let session = self.session;
        let call_id = call.id;
        let intent = (key.clone(), id.clone());
        let recorded = journaled(&self.journal, move |journal| {
            let (key, id) = intent;
            journal.record_intent(&key, &id, number, session, Some(call_id))
        })
        .await;
        if let Err(error) = recorded {
            return Err(self.journal_failed(&error, "the dispatch intent was not recorded"));
        }
        if context.cancellation.is_cancelled() || Instant::now() >= context.deadline {
            let withdrawn = key.clone();
            let _ = journaled(&self.journal, move |journal| {
                journal.record_observation(&withdrawn, Observed::Withdrawn, Retention::Metadata)
            })
            .await;
            return Err(refusal("cancelled", "the run stopped before dispatch"));
        }
        let attempt = Attempt {
            operation,
            key: key.clone(),
            number,
        };
        // The run's deadline drops this future; the session then records the
        // attempt as unobserved, which is unknown.
        let (observed, reply) = match self.provider.execute(attempt, context).await {
            Ok(value) => (
                Observed::Succeeded {
                    bytes: serde_json::to_vec(&value).map_or(0, |bytes| bytes.len() as u64),
                },
                Ok(json!({"operation": id, "attempt": number})),
            ),
            // Only a machine code reaches the trace, never the provider's message.
            Err(error) => {
                let reply = Err(refusal(
                    machine_code(&error.error.code),
                    "the provider failed",
                ));
                let observed = Observed::Failed {
                    error: error.error,
                    effect: error.effect,
                };
                (observed, reply)
            }
        };
        let retention = self.retention;
        let recorded = journaled(&self.journal, move |journal| {
            journal.record_observation(&key, observed, retention)
        })
        .await;
        match recorded {
            Ok(()) => reply,
            Err(error) => Err(self.journal_failed(&error, "the observed result was not recorded")),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Commit-before-dispatch, proved on [`JournalingProvider::execute`] with a
    //! journal hook that holds or fails the intent's transaction before it commits.
    //! The execute future is polled by hand, so "the provider has not been entered"
    //! is checked after every step that could reach it, never after a sleep.

    use std::path::PathBuf;
    use std::pin::pin;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::journal::test_hook::{CommitHook, Point};
    use crate::journal::{Durability, WriteKind};
    use crate::plan::{Plan, Scope};
    use crate::recovery::{AttemptError, Reconciliation};
    use crate::runtime::FactSink;

    /// Fails a test that would otherwise wait forever.
    const BOUND: Duration = Duration::from_secs(10);

    /// What happened, in order, across the journal and the provider.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        IntentHeld(String),
        IntentCommitted(String),
        ProviderEntered(String),
    }

    type Events = Arc<Mutex<Vec<Event>>>;

    fn events(log: &Events) -> Vec<Event> {
        log.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// A provider that records when it is entered and succeeds.
    struct Recording(Events);

    impl RecoverableProvider for Recording {
        fn declaration(&self) -> crate::recovery::ProviderDeclaration {
            unreachable!("not used by these tests")
        }

        async fn authorize(&self, _: &Scope, _: ProbeContext) -> Result<(), ProviderError> {
            Ok(())
        }

        async fn execute(&self, attempt: Attempt, _: CallContext) -> Result<Value, AttemptError> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Event::ProviderEntered(attempt.key));
            Ok(json!({"ok": true}))
        }

        async fn reconcile(
            &self,
            _: Operation,
            _: Vec<String>,
            _: ProbeContext,
        ) -> Result<Reconciliation, ProviderError> {
            unreachable!("not used by these tests")
        }
    }

    struct Harness {
        provider: JournalingProvider<Recording>,
        events: Events,
        held: mpsc::Receiver<String>,
        release: mpsc::Sender<Result<(), String>>,
        _dir: Scratch,
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn harness(name: &str) -> Harness {
        let dir =
            std::env::temp_dir().join(format!("codemode-executor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let plan = Plan::parse(
            r#"{"version":1,"operations":[{"id":"op","tool":"t",
                "scope":{"provider":"p","account":"x"},"arguments":{}}]}"#,
        )
        .expect("plan");
        let mut journal =
            Journal::create(dir.join("journal.db"), &plan, Durability::Process).expect("journal");
        let session = journal.begin_session(SessionKind::Run).expect("session");

        let events = Events::default();
        let (held_sender, held) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        // The intent's transaction waits here until the test releases it with `Ok` or a failure.
        let release_receiver = Mutex::new(release_receiver);
        let log = Arc::clone(&events);
        let hook: CommitHook = Arc::new(move |point, kind| {
            let WriteKind::Intent(key) = kind else {
                return Ok(());
            };
            let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
            match point {
                Point::BeforeCommit => {
                    log.push(Event::IntentHeld(key.to_string()));
                    drop(log);
                    let _ = held_sender.send(key.to_string());
                    release_receiver
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .recv_timeout(BOUND)
                        .unwrap_or_else(|_| Err("the test never released the write".into()))
                }
                Point::Committed => {
                    log.push(Event::IntentCommitted(key.to_string()));
                    Ok(())
                }
            }
        });
        journal.set_commit_hook(hook);

        let operation = plan.operation("op").expect("op").clone();
        let entries = HashMap::from([("op".to_string(), (operation, 1, "key-op-1".to_string()))]);
        let provider = JournalingProvider {
            journal: Shared::new(journal),
            provider: Arc::new(Recording(Arc::clone(&events))),
            entries,
            dispatched: Mutex::new(HashSet::new()),
            session,
            retention: Retention::Metadata,
            cancellation: RunCancellation::default(),
        };
        Harness {
            provider,
            events,
            held,
            release,
            _dir: Scratch(dir),
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: 1,
            tool: "t".into(),
            arguments: json!({"operation": "op"}),
        }
    }

    fn context(deadline: Instant, cancellation: RunCancellation) -> CallContext {
        CallContext {
            deadline,
            cancellation,
            facts: FactSink::detached(),
        }
    }

    fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Wait until the intent's transaction is held open, polling `future` so any
    /// code path that could reach the provider without the commit runs first.
    fn wait_until_held<F: Future>(harness: &Harness, mut future: std::pin::Pin<&mut F>) {
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            assert!(
                poll_once(future.as_mut()).is_pending(),
                "execute finished while its intent was held"
            );
            match harness.held.try_recv() {
                Ok(key) => {
                    assert_eq!(key, "key-op-1");
                    break;
                }
                Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("the intent write was never held: {error}"),
            }
        }
        // Everything the future can do without the commit, it has now done.
        for _ in 0..8 {
            assert!(poll_once(future.as_mut()).is_pending());
        }
    }

    fn queued_writes(harness: &Harness) -> usize {
        *harness
            .provider
            .journal
            .writes
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_provider_is_entered_only_after_the_intent_commits() {
        let harness = harness("commit");
        let far = Instant::now() + BOUND;
        let mut execute = pin!(
            harness
                .provider
                .execute(call(), context(far, RunCancellation::default()))
        );
        wait_until_held(&harness, execute.as_mut());
        assert_eq!(
            events(&harness.events),
            [Event::IntentHeld("key-op-1".into())]
        );

        harness.release.send(Ok(())).expect("release");
        let reply = time::timeout(BOUND, execute).await.expect("bounded");
        assert_eq!(reply, Ok(json!({"operation": "op", "attempt": 1})));
        assert_eq!(
            events(&harness.events),
            [
                Event::IntentHeld("key-op-1".into()),
                Event::IntentCommitted("key-op-1".into()),
                Event::ProviderEntered("key-op-1".into()),
            ]
        );
        let history = harness
            .provider
            .journal
            .journal
            .lock()
            .expect("journal")
            .history()
            .expect("history");
        assert_eq!(history.attempts.len(), 1);
        assert_eq!(history.attempts[0].key, "key-op-1");
        assert!(matches!(
            history.attempts[0]
                .observation
                .as_ref()
                .map(|o| &o.observed),
            Some(Observed::Succeeded { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_intent_commit_never_reaches_the_provider() {
        let harness = harness("failed");
        let far = Instant::now() + BOUND;
        let mut execute = pin!(
            harness
                .provider
                .execute(call(), context(far, RunCancellation::default()))
        );
        wait_until_held(&harness, execute.as_mut());

        harness
            .release
            .send(Err("disk full".into()))
            .expect("release");
        let reply = time::timeout(BOUND, execute).await.expect("bounded");
        let error = reply.expect_err("the intent was not recorded");
        assert_eq!(error.code, "journal_unavailable");
        assert!(
            harness.provider.cancellation.is_cancelled(),
            "the run is stopped"
        );
        assert_eq!(
            events(&harness.events),
            [Event::IntentHeld("key-op-1".into())]
        );
        let history = harness
            .provider
            .journal
            .journal
            .lock()
            .expect("journal")
            .history()
            .expect("history");
        assert!(
            history.attempts.is_empty(),
            "the rolled-back intent left no attempt"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_or_deadline_during_the_held_write_withdraws_the_attempt() {
        for case in ["cancelled", "deadline"] {
            let harness = harness(case);
            let cancellation = RunCancellation::default();
            let deadline = if case == "deadline" {
                Instant::now() + Duration::from_millis(50)
            } else {
                Instant::now() + BOUND
            };
            let mut execute = pin!(
                harness
                    .provider
                    .execute(call(), context(deadline, cancellation.clone()))
            );
            wait_until_held(&harness, execute.as_mut());
            if case == "cancelled" {
                cancellation.cancel();
            } else {
                // The condition, not a sleep: release only once the deadline has passed.
                while Instant::now() < deadline {
                    std::thread::yield_now();
                }
            }
            harness.release.send(Ok(())).expect("release");
            let reply = time::timeout(BOUND, execute).await.expect("bounded");
            assert_eq!(reply.expect_err(case).code, "cancelled", "{case}");
            let history = harness
                .provider
                .journal
                .journal
                .lock()
                .expect("journal")
                .history()
                .expect("history");
            assert_eq!(
                history.attempts[0]
                    .observation
                    .as_ref()
                    .map(|o| &o.observed),
                Some(&Observed::Withdrawn),
                "{case}"
            );
            assert!(
                !events(&harness.events)
                    .iter()
                    .any(|event| matches!(event, Event::ProviderEntered(_))),
                "{case}: the provider was entered"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_call_never_dispatches_and_its_late_intent_is_still_accounted() {
        let harness = harness("dropped");
        let far = Instant::now() + BOUND;
        {
            let mut execute = pin!(
                harness
                    .provider
                    .execute(call(), context(far, RunCancellation::default()))
            );
            wait_until_held(&harness, execute.as_mut());
            // The runtime drops the call on cancellation or its deadline; the write keeps running.
        }
        assert_eq!(
            queued_writes(&harness),
            1,
            "the abandoned write is still counted"
        );
        let session = harness.provider.session;
        let shared = harness.provider.journal.clone();
        let finish = tokio::spawn(async move {
            after_queued_writes(&shared, move |journal| {
                journal.record_unobserved(session, "cancelled")?;
                journal.history()
            })
            .await
        });
        harness.release.send(Ok(())).expect("release");
        let history = time::timeout(BOUND, finish)
            .await
            .expect("bounded")
            .expect("joined")
            .expect("history");
        assert_eq!(queued_writes(&harness), 0);
        assert_eq!(
            events(&harness.events),
            [
                Event::IntentHeld("key-op-1".into()),
                Event::IntentCommitted("key-op-1".into()),
            ],
            "the provider was never entered"
        );
        assert_eq!(
            history.attempts[0]
                .observation
                .as_ref()
                .map(|o| &o.observed),
            Some(&Observed::Unobserved {
                reason: "cancelled".into()
            }),
            "the late intent is marked as never observed"
        );
    }
}
