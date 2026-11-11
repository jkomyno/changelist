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
//! Three rules hold across the calls of one session. The first operation a
//! session dispatches, in plan order, is sent alone, and the others start only
//! after it has an answer.
//! Once [`SessionOptions::max_failures`] operations have ended without being
//! applied, no further operation is sent. After a provider's rate limit
//! refuses an attempt, nothing is sent until the wait is over; then one
//! attempt goes first, and the rest follow once it is answered.
//!
//! Recovery never reruns an agent's program: it works only from the approved list.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

use crate::journal::{History, Journal, JournalError, Record, SessionKind, storage};
use crate::plan::{FrozenPlan, Operation};
use crate::plan_inspect::{AttemptLine, attempt_line};
use crate::recovery::{
    Approval, Assessment, Attempt, Fact, FailureEffect, NextStep, Observed, ProbeContext,
    ReconcileMethod, RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety,
    Retention, approval_binding, assess, machine_code,
};
use crate::runtime::{
    CallContext, NativeCall, ProviderError, RunCancellation, RunConfig, RunError, RunReport,
    RunStatus, ToolCall, ToolProvider, lock, millis, run_calls,
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
    /// Stop sending operations once this many have ended the session without
    /// being applied: the provider failed or refused the attempt, its outcome
    /// is unknown, or a rate limit used up the attempt budget or outlasts the
    /// deadline. Attempts already sent finish and are recorded; operations not
    /// yet sent stay as they were, for a later `resume`. `0` never stops.
    /// Defaults to 1. Not part of the approval, because stopping earlier can
    /// only reduce effects.
    pub max_failures: usize,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            kind: SessionKind::Run,
            deadline: Duration::from_secs(300),
            max_failures: 1,
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
    /// The operation's latest attempt, from any session, with the error
    /// message the journal kept; absent before the first attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<AttemptLine>,
}

/// How a session stopped before it sent every operation it could have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoppedEarly {
    /// The session's [`SessionOptions::max_failures`].
    pub max_failures: usize,
    /// The operations whose end without being applied reached the limit, in
    /// the order they ended.
    pub tripped_by: Vec<String>,
    /// Operations the session then held back: it did not send them, or did
    /// not send them again after a rate limit. A later `resume` sends them.
    pub not_sent: usize,
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
    /// Why a session was refused, was interrupted, or stopped early.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Set when the session held operations back after
    /// [`SessionOptions::max_failures`] of them ended without being applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_early: Option<StoppedEarly>,
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

impl SessionOutcome {
    /// A process exit status for a host that runs one session per process: 0
    /// when `completed`;
    /// for `interrupted`, the [`RunStatus::exit_code`] of [`Self::interruption`]
    /// (124 for a deadline and 130 for cancellation, whether or not dispatch
    /// began), never below 1, and 1 when the interruption is unknown; 3 for
    /// every other status, such as `needs_review` or `incomplete`.
    ///
    /// ```
    /// use codemode::{RunStatus, SessionOutcome, SessionTiming};
    ///
    /// let mut outcome = SessionOutcome {
    ///     session: 1,
    ///     status: "interrupted".into(),
    ///     detail: None,
    ///     stopped_early: None,
    ///     interruption: Some(RunStatus::Deadline),
    ///     probed: 0,
    ///     dispatched: 0,
    ///     operations: Vec::new(),
    ///     timing: SessionTiming::default(),
    ///     trace: None,
    /// };
    /// assert_eq!(outcome.exit_code(), 124);
    /// outcome.interruption = Some(RunStatus::Cancelled);
    /// assert_eq!(outcome.exit_code(), 130);
    /// outcome.status = "needs_review".into();
    /// assert_eq!(outcome.exit_code(), 3);
    /// outcome.status = "completed".into();
    /// assert_eq!(outcome.exit_code(), 0);
    /// ```
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self.status.as_str() {
            "completed" => 0,
            "interrupted" => self
                .interruption
                .map_or(1, |status| status.exit_code().max(1)),
            _ => 3,
        }
    }
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
    // Attempts are in intent order, so the last one seen is the latest.
    let mut latest = HashMap::new();
    for attempt in &history.attempts {
        latest.insert(attempt.operation.as_str(), attempt);
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
                last_attempt: latest
                    .get(operation.id.as_str())
                    .map(|attempt| attempt_line(attempt, true)),
            }
        })
        .collect()
}

/// A record waiting for the next group commit, and who awaits its ordinal.
type Waiting = (Record, oneshot::Sender<Result<u64, JournalError>>);

/// The session's journal, a count of journal writes queued on blocking threads,
/// and the records waiting to be committed.
///
/// A write keeps running after the runtime drops the call that queued it, so the
/// count lets the session wait for every such write before it records which
/// attempts were never observed.
#[derive(Clone)]
struct Shared {
    journal: Arc<Mutex<Journal>>,
    writes: Arc<(Mutex<usize>, Condvar)>,
    waiting: Arc<Mutex<Vec<Waiting>>>,
}

/// Counts one queued write until it is dropped, after the write ran or was discarded.
struct QueuedWrite(Arc<(Mutex<usize>, Condvar)>);

impl Drop for QueuedWrite {
    fn drop(&mut self) {
        let (count, finished) = &*self.0;
        *lock(count) -= 1;
        finished.notify_all();
    }
}

impl Shared {
    fn new(journal: Journal) -> Self {
        Self {
            journal: Arc::new(Mutex::new(journal)),
            writes: Arc::default(),
            waiting: Arc::default(),
        }
    }

    /// Count one write as queued until the returned guard is dropped.
    fn queue_write(&self) -> QueuedWrite {
        *lock(&self.writes.0) += 1;
        QueuedWrite(Arc::clone(&self.writes))
    }

    fn id(&self) -> String {
        lock(&self.journal).id().to_string()
    }
}

/// Run `body` against the journal on a blocking thread.
async fn journaled<T: Send + 'static>(
    shared: &Shared,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    let queued = shared.queue_write();
    journaled_as(shared, queued, body).await
}

/// [`journaled`], counted by `queued`, which the caller took first.
async fn journaled_as<T: Send + 'static>(
    shared: &Shared,
    queued: QueuedWrite,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    on_journal(Arc::clone(&shared.journal), move || queued, body).await
}

/// On a blocking thread, run `before`, then `body` against the locked journal.
/// What `before` returns is dropped once the journal is unlocked again.
async fn on_journal<G, T: Send + 'static>(
    journal: Arc<Mutex<Journal>>,
    before: impl FnOnce() -> G + Send + 'static,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    tokio::task::spawn_blocking(move || {
        let _held = before();
        let mut guard = lock(&journal);
        body(&mut guard)
    })
    .await
    .map_err(storage)?
}

/// Commit `record` and return its fact ordinal once the transaction holding it
/// has committed.
///
/// Group commit: whoever next holds the journal commits every record waiting at
/// that moment, in arrival order, in one transaction, then hands each waiter its
/// own result, so records that arrive while a commit is in flight share the next
/// one. A failed commit fails every record in it and records none of them. The
/// write is counted before the record waits, and its commit is scheduled before
/// the first await, so [`after_queued_writes`] never misses it and it commits
/// even if this future is dropped, as a single write would.
async fn committed(shared: &Shared, record: Record) -> Result<u64, JournalError> {
    let queued = shared.queue_write();
    let (sender, receiver) = oneshot::channel();
    lock(&shared.waiting).push((record, sender));
    let waiting = Arc::clone(&shared.waiting);
    journaled_as(shared, queued, move |journal| {
        // Taken while holding the journal, so a record is either still waiting
        // here or an earlier holder has already answered it.
        let group = std::mem::take(&mut *lock(&waiting));
        if group.is_empty() {
            return Ok(());
        }
        let (records, waiters): (Vec<Record>, Vec<_>) = group.into_iter().unzip();
        let result = journal.commit(&records);
        for (index, waiter) in waiters.into_iter().enumerate() {
            let _ = waiter.send(match &result {
                Ok(ordinals) => Ok(ordinals[index]),
                Err(error) => Err(error.clone()),
            });
        }
        Ok(())
    })
    .await?;
    receiver.await.unwrap_or_else(|_| {
        Err(JournalError::Storage(
            "the journal stopped before the record was committed".into(),
        ))
    })
}

/// Like [`journaled`], but first waits until every queued write has finished.
async fn after_queued_writes<T: Send + 'static>(
    shared: &Shared,
    body: impl FnOnce(&mut Journal) -> Result<T, JournalError> + Send + 'static,
) -> Result<T, JournalError> {
    let writes = Arc::clone(&shared.writes);
    let wait = move || {
        let (count, finished) = &*writes;
        let mut queued = lock(count);
        while *queued > 0 {
            queued = finished
                .wait(queued)
                .unwrap_or_else(PoisonError::into_inner);
        }
    };
    on_journal(Arc::clone(&shared.journal), wait, body).await
}

/// Run one session against `journal` with `provider`.
///
/// `options.deadline`, counted from the call, bounds authorization, probes, and
/// dispatch. Reached before an attempt's intent commits, it withdraws the
/// attempt; after, it drops the provider call and the attempt stays unknown
/// (`unobserved`). A probe it cuts off stays charged and is recorded as
/// `probe_timeout`; an authorization it cuts off ends the session before dispatch.
///
/// The first operation the session dispatches, in plan order, is sent alone;
/// the others start once it has an answer, so that a provider that fails
/// every attempt receives one attempt, not as many as the approved
/// concurrency. The session stops
/// sending operations once `options.max_failures` of them
/// have ended without being applied; [`SessionOutcome::stopped_early`] then
/// says which ones and how many operations were held back. An attempt a
/// provider's rate limit refused ([`AttemptError::rate_limit`]) is sent again
/// in the same session, as a new attempt within the approved budget, after the
/// wait the provider asked for: 1 second when it named none, doubled for each
/// rate-limit answer in a row, and at most 60 seconds. Nothing else is sent to
/// the provider during the wait. An operation whose budget is spent, or whose
/// wait would pass the deadline, ends as not applied.
///
/// [`AttemptError::rate_limit`]: crate::AttemptError::rate_limit
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
    // The first operation of the list goes alone, whichever call the
    // scheduler starts first.
    let first = dispatch.first().map(|(operation, _)| operation.id.clone());
    let gate = Arc::new(Gate::new(options.max_failures, first));
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
            &gate,
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
    let stopped_early = gate.stopped_early();
    let detail = match (&interrupted, &stopped_early) {
        (Some(status), _) => {
            let status = status.as_str();
            Some(if trace.is_some() {
                format!("the dispatch run ended with {status}")
            } else {
                format!("the session ended with {status} before dispatch")
            })
        }
        (None, Some(stop)) => {
            // The outcome lists every operation; a sentence names a few.
            let named = stop.tripped_by.len().min(3);
            let more = match stop.tripped_by.len() - named {
                0 => String::new(),
                more => format!(", and {more} more"),
            };
            Some(format!(
                "stopped early: {} not applied ({}{more}); {} not sent",
                stop.tripped_by.len(),
                stop.tripped_by[..named].join(", "),
                stop.not_sent
            ))
        }
        (None, None) => None,
    };
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
        stopped_early,
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
    if current.definitions != approved.definitions {
        return Err(
            "the provider's tool definitions changed since approval; review and approve again"
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
            let start = Record::probe_start(&operation.id, session, &keys)?;
            let probe = committed(journal, start).await?;
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
        let Some((probe, elapsed, outcome)) = result.map_err(storage)? else {
            continue;
        };
        committed(
            journal,
            Record::probe_answer(probe, elapsed, &outcome, retention),
        )
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
    gate: &Arc<Gate>,
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
        // The run's results are this module's own short replies, so their
        // budget grows with the plan instead of ending a large one.
        max_output_bytes: operations
            .len()
            .saturating_mul(REPLY_BYTES)
            .max(RunConfig::default().max_output_bytes),
        allowed_tools: tools.into_iter().collect(),
        ..RunConfig::default()
    };
    let entries = operations
        .into_iter()
        .map(|(operation, number)| (operation.id.clone(), Entry { operation, number }))
        .collect();
    let journaling = JournalingProvider {
        journal_id: journal.id(),
        journal: journal.clone(),
        provider: Arc::clone(provider),
        entries,
        dispatched: Mutex::new(HashSet::new()),
        session,
        retention,
        max_attempts: policy.max_attempts,
        gate: Arc::clone(gate),
        cancellation: cancellation.clone(),
    };
    run_calls(&calls, config, journaling, cancellation.clone()).await
}

/// Most serialized bytes of one reply this module gives the runtime for a call:
/// an operation identity of at most 64 bytes with an attempt number, or a
/// machine code of at most 64 bytes with a fixed message.
const REPLY_BYTES: usize = 256;
/// The wait after a rate limit when the provider names none.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(1);
/// The longest wait after one rate-limit answer.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
/// The error code of a call that ends because of a rate limit.
const RATE_LIMITED: &str = "rate_limited";

/// The wait after the `repeats`-th rate-limit answer in a row (1 for the first):
/// what the provider asked for, or one second, doubled for each repeat, and at
/// most 60 seconds.
fn rate_limit_wait(retry_after: Option<Duration>, repeats: u32) -> Duration {
    let doublings = repeats.saturating_sub(1).min(6);
    retry_after
        .unwrap_or(RATE_LIMIT_WAIT)
        .saturating_mul(1 << doublings)
        .min(MAX_RATE_LIMIT_WAIT)
}

/// What a session's calls share: the first attempt that goes alone, the
/// failures that stop the session early, and the wait after a rate limit.
struct Gate {
    /// [`SessionOptions::max_failures`].
    max_failures: usize,
    /// The operation whose first attempt is sent alone: the first one the
    /// session dispatches. `None` when the session dispatches nothing.
    first: Option<String>,
    state: Mutex<GateState>,
    /// Bumped after every change a waiting call must look at.
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct GateState {
    /// Operations that ended without being applied, in the order they ended.
    failed: Vec<String>,
    /// Calls turned away after the session stopped.
    held: usize,
    /// Rate-limit answers in a row; 0 when the provider is not limiting.
    limited: u32,
    /// Nothing is sent before this.
    resume_at: Option<Instant>,
    /// Whether the one attempt sent alone is still out: the first attempt of
    /// the session, or the first one after a rate-limit wait.
    probing: bool,
    /// Whether any attempt of this session has an answer. Until one has,
    /// only the first attempt is sent.
    answered: bool,
}

/// What a call may do now.
enum Admission {
    /// Send an attempt. `probe` marks the one attempt sent alone: the first
    /// of the session, or the first after a rate-limit wait. Its answer
    /// decides whether the others follow.
    Send { probe: bool },
    /// The session stopped early: send nothing.
    Stopped,
    /// The rate-limit wait ends after the session deadline: send nothing.
    Paused,
    /// Cancellation or the deadline ended the wait.
    Ended,
}

impl Gate {
    fn new(max_failures: usize, first: Option<String>) -> Self {
        let state = GateState {
            // Without a first operation nothing waits for its answer.
            answered: first.is_none(),
            ..GateState::default()
        };
        Self {
            max_failures,
            first,
            state: Mutex::new(state),
            changed: watch::Sender::new(0),
        }
    }

    fn announce(&self) {
        self.changed.send_modify(|version| *version += 1);
    }

    fn stopped(&self, state: &GateState) -> bool {
        self.max_failures > 0 && state.failed.len() >= self.max_failures
    }

    /// Wait until an attempt of `operation` may be sent, or until none may.
    async fn admit(&self, context: &CallContext, operation: &str) -> Admission {
        // Subscribed before the state is read, so no change is missed.
        let mut changes = self.changed.subscribe();
        loop {
            let resume_at = {
                let mut state = lock(&self.state);
                if self.stopped(&state) {
                    state.held += 1;
                    return Admission::Stopped;
                }
                if state.limited == 0 {
                    if state.answered {
                        return Admission::Send { probe: false };
                    }
                    if !state.probing && self.first.as_deref() == Some(operation) {
                        state.probing = true;
                        return Admission::Send { probe: true };
                    }
                    // The first operation has no answer yet.
                    None
                } else {
                    match state.resume_at.filter(|at| *at > Instant::now()) {
                        Some(at) if at >= context.deadline => return Admission::Paused,
                        Some(at) => Some(at),
                        None if !state.probing => {
                            state.probing = true;
                            return Admission::Send { probe: true };
                        }
                        None => None,
                    }
                }
            };
            let wait = async {
                match resume_at {
                    Some(at) => time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = context.cancellation.cancelled() => return Admission::Ended,
                () = time::sleep_until(context.deadline) => return Admission::Ended,
                _ = changes.changed() => {}
                () = wait => {}
            }
        }
    }

    /// An attempt sent at `sent` has an answer, or ended without one.
    ///
    /// `rate_limit` is set when a rate limit refused it, with the wait the
    /// provider asked for: nothing is sent until the wait it starts, or
    /// extends, is over. Any other end of the one attempt sent alone lets the
    /// others follow. `failed` names the operation when it ended this session
    /// without being applied. All of it is one change, so that no waiting
    /// call is admitted between the answer and the count of the failure.
    fn observed(
        &self,
        probe: bool,
        sent: Instant,
        rate_limit: Option<Option<Duration>>,
        failed: Option<&str>,
    ) {
        let mut state = lock(&self.state);
        state.answered = true;
        if probe {
            state.probing = false;
        }
        match rate_limit {
            Some(retry_after) => {
                // Attempts sent before the wait began were refused by the same
                // limit as the attempt that began it; they do not count as a
                // repeat.
                if state.resume_at.is_none_or(|at| sent >= at) {
                    state.limited += 1;
                }
                let resume_at = Instant::now() + rate_limit_wait(retry_after, state.limited);
                state.resume_at = Some(state.resume_at.map_or(resume_at, |at| at.max(resume_at)));
            }
            // The answer to the attempt sent first after a wait shows that
            // the provider is no longer limiting.
            None if probe => {
                state.limited = 0;
                state.resume_at = None;
            }
            None => {}
        }
        if let Some(operation) = failed {
            state.failed.push(operation.to_string());
        }
        drop(state);
        self.announce();
    }

    /// `operation` ended this session without being applied and without
    /// being sent.
    fn failed(&self, operation: &str) {
        lock(&self.state).failed.push(operation.to_string());
        self.announce();
    }

    /// How the session stopped early, if it held any operation back.
    fn stopped_early(&self) -> Option<StoppedEarly> {
        let state = lock(&self.state);
        (state.held > 0).then(|| StoppedEarly {
            max_failures: self.max_failures,
            tripped_by: state
                .failed
                .iter()
                .take(self.max_failures)
                .cloned()
                .collect(),
            not_sent: state.held,
        })
    }
}

/// One operation approved for dispatch in this run.
#[derive(Clone)]
struct Entry {
    operation: Operation,
    /// The attempt number of its first dispatch in this run.
    number: u32,
}

/// The runtime's provider for a dispatch run: it journals each attempt around the
/// trusted provider's call.
struct JournalingProvider<P> {
    journal: Shared,
    /// The journal's identity, which starts every attempt key.
    journal_id: String,
    provider: Arc<P>,
    /// Operation identity -> its entry.
    entries: HashMap<String, Entry>,
    dispatched: Mutex<HashSet<String>>,
    session: u64,
    /// What the journal keeps of the provider's replies, from the approval.
    retention: Retention,
    /// The approved number of dispatches of one operation across all sessions.
    max_attempts: u32,
    gate: Arc<Gate>,
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
    fn claim(&self, call: &ToolCall) -> Result<Entry, ProviderError> {
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
            .filter(|entry| entry.operation.tool == call.tool)
            .ok_or_else(|| {
                refusal(
                    "plan_violation",
                    "the operation is not approved for dispatch now",
                )
            })?;
        let fresh = lock(&self.dispatched).insert(id.to_string());
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
        let Entry {
            operation,
            mut number,
        } = self.claim(&call)?;
        let id = operation.id.clone();
        // Whether a rate limit refused an earlier attempt of this call.
        let mut limited = false;
        loop {
            let probe = match self.gate.admit(&context, &id).await {
                Admission::Send { probe } => probe,
                Admission::Stopped => {
                    return Err(refusal(
                        "stopped_early",
                        "the session stopped sending operations before this one was sent",
                    ));
                }
                Admission::Paused => {
                    if limited {
                        self.gate.failed(&id);
                    }
                    return Err(refusal(
                        RATE_LIMITED,
                        "the provider's rate limit lasts beyond the session deadline",
                    ));
                }
                Admission::Ended => {
                    return Err(refusal("cancelled", "the run stopped before dispatch"));
                }
            };
            let key = format!("{}-{id}-{number}", self.journal_id);
            let intent = Record::intent(&key, &id, number, self.session, Some(call.id));
            if let Err(error) = committed(&self.journal, intent).await {
                return Err(self.journal_failed(&error, "the dispatch intent was not recorded"));
            }
            if context.cancellation.is_cancelled() || Instant::now() >= context.deadline {
                if let Ok(withdrawn) =
                    Record::observation(&key, Observed::Withdrawn, Retention::Metadata)
                {
                    let _ = committed(&self.journal, withdrawn).await;
                }
                return Err(refusal("cancelled", "the run stopped before dispatch"));
            }
            let attempt = Attempt {
                operation: operation.clone(),
                key: key.clone(),
                number,
            };
            let sent = Instant::now();
            // The run's deadline drops this future; the session then records the
            // attempt as unobserved, which is unknown.
            let outcome = self.provider.execute(attempt, context.clone()).await;
            // The gate learns of the answer before the answer is committed, so
            // that no other call is sent in between.
            let limit = outcome.as_ref().err().and_then(|error| {
                error
                    .rate_limit
                    .filter(|_| error.effect == FailureEffect::NotApplied)
            });
            // A refused attempt is sent again while the budget lasts.
            let again = limit.is_some() && number < self.max_attempts;
            self.gate.observed(
                probe,
                sent,
                limit.map(|limit| limit.retry_after),
                (outcome.is_err() && !again).then_some(id.as_str()),
            );
            let (observed, reply) = match outcome {
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
            let shown = observed.clone();
            let recorded = match Record::observation(&key, observed, self.retention) {
                Ok(observation) => committed(&self.journal, observation).await,
                Err(error) => Err(error),
            };
            if let Err(error) = recorded {
                return Err(self.journal_failed(&error, "the observed result was not recorded"));
            }
            self.provider.recorded(&operation, number, &shown);
            if !again {
                return reply;
            }
            // Sent again once the wait is over, as the next attempt.
            number += 1;
            limited = true;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Commit-before-dispatch, proved on [`JournalingProvider::execute`] with a
    //! journal hook that holds or fails the intent's transaction before it commits.
    //! The execute future is polled by hand, so "the provider has not been entered"
    //! is checked after every step that could reach it, never after a sleep.
    //! Group commit is proved by holding the journal while records queue, so the
    //! next commit finds all of them waiting.

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
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum Event {
        IntentHeld(String),
        IntentCommitted(String),
        ProviderEntered(String),
        ObservationHeld(String),
        ObservationCommitted(String),
    }

    type Events = Arc<Mutex<Vec<Event>>>;

    fn events(log: &Events) -> Vec<Event> {
        lock(log).clone()
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
            lock(&self.0).push(Event::ProviderEntered(attempt.key));
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

    /// Which writes the hook holds open until the test releases them.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Hold {
        /// Hold intents; observations are neither held nor logged.
        Intents,
        /// Hold and log observations; intents are logged, not held.
        Observations,
        /// Hold nothing; log intents, and fail the commit holding this attempt
        /// key's intent, if any.
        Nothing(Option<&'static str>),
    }

    fn harness(name: &str) -> Harness {
        harness_with(name, &["op"], Hold::Intents)
    }

    /// A journaling provider for a plan of `operations`, whose attempt keys are
    /// `key-<operation>-1`, with a commit hook that logs and holds as `hold` says.
    fn harness_with(name: &str, operations: &[&str], hold: Hold) -> Harness {
        let dir =
            std::env::temp_dir().join(format!("codemode-executor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let listed: Vec<Value> = operations
            .iter()
            .map(|id| {
                json!({"id": id, "tool": "t", "scope": {"provider": "p", "account": "x"},
                       "arguments": {}})
            })
            .collect();
        let plan =
            Plan::parse(&json!({"version": 1, "operations": listed}).to_string()).expect("plan");
        let mut journal =
            Journal::create(dir.join("journal.db"), &plan, Durability::Process).expect("journal");
        let session = journal.begin_session(SessionKind::Run).expect("session");

        let events = Events::default();
        let (held_sender, held) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        // A held transaction waits here until the test releases it with `Ok` or a failure.
        let release_receiver = Mutex::new(release_receiver);
        let wait = move |key: &str| {
            let _ = held_sender.send(key.to_string());
            lock(&release_receiver)
                .recv_timeout(BOUND)
                .unwrap_or_else(|_| Err("the test never released the write".into()))
        };
        let log = Arc::clone(&events);
        let hook: CommitHook = Arc::new(move |point, kind| {
            let push = |event| {
                lock(&log).push(event);
            };
            match (point, kind, hold) {
                (Point::BeforeCommit, WriteKind::Intent(key), _) => {
                    push(Event::IntentHeld(key.to_string()));
                    match hold {
                        Hold::Intents => wait(key),
                        Hold::Nothing(Some(failing)) if failing == key => Err("disk full".into()),
                        _ => Ok(()),
                    }
                }
                (Point::Committed, WriteKind::Intent(key), _) => {
                    push(Event::IntentCommitted(key.to_string()));
                    Ok(())
                }
                (Point::BeforeCommit, WriteKind::Observation(key), Hold::Observations) => {
                    push(Event::ObservationHeld(key.to_string()));
                    wait(key)
                }
                (Point::Committed, WriteKind::Observation(key), Hold::Observations) => {
                    push(Event::ObservationCommitted(key.to_string()));
                    Ok(())
                }
                _ => Ok(()),
            }
        });
        journal.set_commit_hook(hook);

        let entries = operations
            .iter()
            .map(|id| {
                let operation = plan.operation(id).expect("planned").clone();
                let entry = Entry {
                    operation,
                    number: 1,
                };
                (id.to_string(), entry)
            })
            .collect();
        let provider = JournalingProvider {
            journal: Shared::new(journal),
            journal_id: "key".into(),
            provider: Arc::new(Recording(Arc::clone(&events))),
            entries,
            dispatched: Mutex::new(HashSet::new()),
            session,
            retention: Retention::Metadata,
            max_attempts: 3,
            // These tests poll several calls at once, so the first attempt of
            // the session already has its answer.
            gate: Arc::new(open_gate(1)),
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
        call_for(1, "op")
    }

    fn call_for(id: u64, operation: &str) -> ToolCall {
        ToolCall {
            id,
            tool: "t".into(),
            arguments: json!({"operation": operation}),
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
        *lock(&harness.provider.journal.writes.0)
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

    #[test]
    fn the_wait_after_a_rate_limit_doubles_and_is_capped() {
        let seconds = |asked: Option<u64>, repeats| {
            rate_limit_wait(asked.map(Duration::from_secs), repeats).as_secs()
        };
        // One second when the provider names no wait.
        assert_eq!(
            [1, 2, 3, 4, 7, 100].map(|repeats| seconds(None, repeats)),
            [1, 2, 4, 8, 60, 60]
        );
        assert_eq!(
            [1, 2, 3].map(|repeats| seconds(Some(5), repeats)),
            [5, 10, 20]
        );
        assert_eq!(seconds(Some(0), 3), 0);
        assert_eq!(seconds(Some(3600), 1), 60);
        assert_eq!(seconds(Some(u64::MAX), 9), 60);
    }

    fn far(seconds: u64) -> CallContext {
        context(
            Instant::now() + Duration::from_secs(seconds),
            RunCancellation::default(),
        )
    }

    /// A gate whose first operation already has an answer.
    fn open_gate(max_failures: usize) -> Gate {
        Gate::new(max_failures, None)
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_operation_of_a_session_goes_alone() {
        let first = || Some("first".to_string());
        // The scheduler may start another call before the first operation's:
        // it waits all the same.
        let gate = Gate::new(1, first());
        let context = far(600);
        let mut second = pin!(gate.admit(&context, "second"));
        assert!(poll_once(second.as_mut()).is_pending());
        assert!(matches!(
            gate.admit(&context, "first").await,
            Admission::Send { probe: true }
        ));
        let mut third = pin!(gate.admit(&context, "third"));
        assert!(poll_once(second.as_mut()).is_pending());
        assert!(poll_once(third.as_mut()).is_pending());
        time::advance(Duration::from_secs(30)).await;
        assert!(poll_once(second.as_mut()).is_pending());
        // It fails: the session stops with one failure, and nothing else was sent.
        gate.observed(true, Instant::now(), None, Some("first"));
        assert!(matches!(second.await, Admission::Stopped));
        assert!(matches!(third.await, Admission::Stopped));
        assert_eq!(
            gate.stopped_early(),
            Some(StoppedEarly {
                max_failures: 1,
                tripped_by: vec!["first".into()],
                not_sent: 2,
            })
        );

        // It is applied: the others follow at once.
        let gate = Gate::new(1, first());
        assert!(matches!(
            gate.admit(&context, "first").await,
            Admission::Send { probe: true }
        ));
        let mut second = pin!(gate.admit(&context, "second"));
        assert!(poll_once(second.as_mut()).is_pending());
        gate.observed(true, Instant::now(), None, None);
        assert!(matches!(second.await, Admission::Send { probe: false }));
        assert!(matches!(
            gate.admit(&context, "third").await,
            Admission::Send { probe: false }
        ));

        // A rate limit refuses it: the wait begins, and then one attempt of
        // any operation goes first.
        let gate = Gate::new(1, first());
        assert!(matches!(
            gate.admit(&context, "first").await,
            Admission::Send { probe: true }
        ));
        let refused = Instant::now();
        gate.observed(true, refused, Some(Some(Duration::from_secs(3))), None);
        assert!(matches!(
            gate.admit(&context, "second").await,
            Admission::Send { probe: true }
        ));
        assert_eq!(refused.elapsed(), Duration::from_secs(3));
        let mut waiting = pin!(gate.admit(&context, "first"));
        assert!(poll_once(waiting.as_mut()).is_pending());

        // Cancellation releases a call that waits for the first answer.
        let gate = Gate::new(1, first());
        let mut waiting = pin!(gate.admit(&context, "second"));
        assert!(poll_once(waiting.as_mut()).is_pending());
        context.cancellation.cancel();
        assert!(matches!(waiting.await, Admission::Ended));
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_rate_limit_one_attempt_goes_first_and_the_rest_follow_its_answer() {
        let gate = open_gate(0);
        let context = far(600);
        assert!(matches!(
            gate.admit(&context, "op").await,
            Admission::Send { probe: false }
        ));
        let sent = Instant::now();
        gate.observed(false, sent, Some(Some(Duration::from_secs(5))), None);
        // A second refusal of the same burst is not a repeat and asks for less.
        gate.observed(false, sent, Some(None), None);

        let started = Instant::now();
        assert!(matches!(
            gate.admit(&context, "op").await,
            Admission::Send { probe: true }
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        // The next call waits for the answer to the one that went first.
        let mut second = pin!(gate.admit(&context, "op"));
        assert!(poll_once(second.as_mut()).is_pending());
        time::advance(Duration::from_secs(30)).await;
        assert!(poll_once(second.as_mut()).is_pending());

        // Refused again: a repeat, so the wait is twice the second asked for.
        let refused = Instant::now();
        gate.observed(true, refused, Some(Some(Duration::from_secs(1))), None);
        assert!(matches!(second.await, Admission::Send { probe: true }));
        assert_eq!(refused.elapsed(), Duration::from_secs(2));
        let mut third = pin!(gate.admit(&context, "op"));
        assert!(poll_once(third.as_mut()).is_pending());

        // Answered: the limit is over, and everything else is sent at once.
        gate.observed(true, Instant::now(), None, None);
        assert!(matches!(third.await, Admission::Send { probe: false }));
        assert!(matches!(
            gate.admit(&context, "op").await,
            Admission::Send { probe: false }
        ));
        assert!(gate.stopped_early().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_that_passes_the_deadline_or_a_stopped_session_sends_nothing() {
        let gate = open_gate(0);
        let limited = |gate: &Gate, retry_after| {
            gate.observed(false, Instant::now(), Some(retry_after), None);
        };
        limited(&gate, Some(Duration::from_secs(3600)));
        // The wait is capped at 60 seconds, which is still after this deadline.
        assert!(matches!(
            gate.admit(&far(59), "op").await,
            Admission::Paused
        ));
        let started = Instant::now();
        assert!(matches!(
            gate.admit(&far(600), "op").await,
            Admission::Send { probe: true }
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(60));

        // A session with no limit never stops; one with a limit stops at it.
        for operation in ["a", "b", "c"] {
            gate.failed(operation);
        }
        gate.observed(true, Instant::now(), None, None);
        assert!(matches!(
            gate.admit(&far(600), "op").await,
            Admission::Send { probe: false }
        ));
        let gate = open_gate(2);
        gate.failed("a");
        assert!(matches!(
            gate.admit(&far(600), "op").await,
            Admission::Send { probe: false }
        ));
        gate.failed("b");
        gate.failed("c");
        for _ in 0..3 {
            assert!(matches!(
                gate.admit(&far(600), "op").await,
                Admission::Stopped
            ));
        }
        assert_eq!(
            gate.stopped_early(),
            Some(StoppedEarly {
                max_failures: 2,
                tripped_by: vec!["a".into(), "b".into()],
                not_sent: 3,
            })
        );

        // A call waiting for a rate limit is released when the session stops,
        // or when it is cancelled.
        let gate = open_gate(1);
        limited(&gate, None);
        let context = far(600);
        let mut waiting = pin!(gate.admit(&context, "op"));
        assert!(poll_once(waiting.as_mut()).is_pending());
        gate.failed("a");
        assert!(matches!(waiting.await, Admission::Stopped));
        let gate = open_gate(0);
        limited(&gate, None);
        let mut waiting = pin!(gate.admit(&context, "op"));
        assert!(poll_once(waiting.as_mut()).is_pending());
        context.cancellation.cancel();
        assert!(matches!(waiting.await, Admission::Ended));
    }

    /// Wait, without awaiting, until `count` journal writes are queued.
    fn wait_until_queued(shared: &Shared, count: usize) {
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            let queued = *lock(&shared.writes.0);
            if queued >= count {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "only {queued} of {count} writes were queued"
            );
            std::thread::yield_now();
        }
    }

    /// Execute one call per operation at once while the journal is held, and
    /// release the journal once every intent is queued behind it.
    async fn execute_together(
        provider: JournalingProvider<Recording>,
        operations: &[&str],
    ) -> (
        Arc<JournalingProvider<Recording>>,
        Vec<Result<Value, ProviderError>>,
    ) {
        let provider = Arc::new(provider);
        let far = Instant::now() + BOUND;
        let held = provider.journal.journal.lock().expect("journal");
        let calls: Vec<_> = (1..)
            .zip(operations)
            .map(|(id, operation)| {
                let provider = Arc::clone(&provider);
                let call = call_for(id, operation);
                tokio::spawn(async move {
                    provider
                        .execute(call, context(far, RunCancellation::default()))
                        .await
                })
            })
            .collect();
        wait_until_queued(&provider.journal, operations.len());
        drop(held);
        let mut replies = Vec::new();
        for call in calls {
            let reply = time::timeout(BOUND, call).await.expect("bounded");
            replies.push(reply.expect("joined"));
        }
        (provider, replies)
    }

    fn history_of(provider: &JournalingProvider<Recording>) -> History {
        provider
            .journal
            .journal
            .lock()
            .expect("journal")
            .history()
            .expect("history")
    }

    const FOUR: [&str; 4] = ["a", "b", "c", "d"];

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_intents_commit_together_before_any_reaches_the_provider() {
        let harness = harness_with("group", &FOUR, Hold::Nothing(None));
        let (provider, replies) = execute_together(harness.provider, &FOUR).await;
        for reply in &replies {
            assert!(reply.is_ok(), "{reply:?}");
        }
        let log = events(&harness.events);
        let first_commit = log
            .iter()
            .position(|event| matches!(event, Event::IntentCommitted(_)))
            .expect("an intent committed");
        let held: BTreeSet<&Event> = log[..first_commit].iter().collect();
        let all_held: BTreeSet<Event> = FOUR
            .iter()
            .map(|id| Event::IntentHeld(format!("key-{id}-1")))
            .collect();
        assert_eq!(
            held,
            all_held.iter().collect(),
            "all four intents are in the first commit: {log:?}"
        );
        for id in FOUR {
            let key = format!("key-{id}-1");
            let at = |wanted: &Event| log.iter().position(|event| event == wanted);
            let committed = at(&Event::IntentCommitted(key.clone())).expect("committed");
            let entered = at(&Event::ProviderEntered(key.clone())).expect("entered");
            assert!(
                committed < entered,
                "{key} reached the provider first: {log:?}"
            );
        }
        let history = history_of(&provider);
        assert_eq!(history.attempts.len(), 4);
        assert!(history.attempts.iter().all(|attempt| matches!(
            attempt.observation.as_ref().map(|o| &o.observed),
            Some(Observed::Succeeded { .. })
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_group_commit_dispatches_none_of_its_operations() {
        let harness = harness_with("group-failed", &FOUR, Hold::Nothing(Some("key-c-1")));
        let (provider, replies) = execute_together(harness.provider, &FOUR).await;
        for reply in &replies {
            let error = reply.as_ref().expect_err("nothing was recorded");
            assert_eq!(error.code, "journal_unavailable", "{error:?}");
        }
        assert!(provider.cancellation.is_cancelled(), "the run is stopped");
        let log = events(&harness.events);
        assert!(
            !log.iter().any(|event| matches!(
                event,
                Event::ProviderEntered(_) | Event::IntentCommitted(_)
            )),
            "{log:?}"
        );
        let history = history_of(&provider);
        assert!(
            history.attempts.is_empty(),
            "the rolled-back group recorded attempts: {:?}",
            history.attempts
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_observation_is_acknowledged_only_after_its_commit() {
        for outcome in [Ok(()), Err("disk full".to_string())] {
            let harness = harness_with("observation", &["op"], Hold::Observations);
            let far = Instant::now() + BOUND;
            let mut execute = pin!(
                harness
                    .provider
                    .execute(call(), context(far, RunCancellation::default()))
            );
            wait_until_held(&harness, execute.as_mut());
            assert_eq!(
                events(&harness.events),
                [
                    Event::IntentHeld("key-op-1".into()),
                    Event::IntentCommitted("key-op-1".into()),
                    Event::ProviderEntered("key-op-1".into()),
                    Event::ObservationHeld("key-op-1".into()),
                ],
                "the provider answered; its observation is not committed yet"
            );

            let failed = outcome.is_err();
            harness.release.send(outcome).expect("release");
            let reply = time::timeout(BOUND, execute).await.expect("bounded");
            if failed {
                assert_eq!(reply.expect_err("not recorded").code, "journal_unavailable");
                assert!(
                    history_of(&harness.provider).attempts[0]
                        .observation
                        .is_none()
                );
            } else {
                assert_eq!(reply, Ok(json!({"operation": "op", "attempt": 1})));
                assert_eq!(
                    events(&harness.events).last(),
                    Some(&Event::ObservationCommitted("key-op-1".into()))
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_committed_with_an_observation_stays_charged_after_a_kill() {
        let dir = std::env::temp_dir().join(format!(
            "codemode-executor-{}-probe-group",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("killed")).expect("scratch");
        let _scratch = Scratch(dir.clone());
        let plan = Plan::parse(
            r#"{"version":1,"operations":[
                {"id":"a","tool":"t","scope":{"provider":"p","account":"x"},"arguments":{}},
                {"id":"b","tool":"t","scope":{"provider":"p","account":"x"},"arguments":{}}]}"#,
        )
        .expect("plan");
        let mut journal =
            Journal::create(dir.join("journal.db"), &plan, Durability::Process).expect("journal");
        let run = journal.begin_session(SessionKind::Run).expect("run");
        journal
            .record_intent("key-a-1", "a", 1, run, None)
            .expect("a's outcome is unknown");
        let resume = journal.begin_session(SessionKind::Resume).expect("resume");
        journal
            .record_intent("key-b-1", "b", 1, resume, None)
            .expect("b dispatched");
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooked = Arc::clone(&log);
        journal.set_commit_hook(Arc::new(move |point, _| {
            lock(&hooked).push(point);
            Ok(())
        }));
        let shared = Shared::new(journal);

        let held = shared.journal.lock().expect("journal");
        let probe = Record::probe_start("a", resume, &["key-a-1".to_string()]).expect("probe");
        let observation = Record::observation(
            "key-b-1",
            Observed::Succeeded { bytes: 2 },
            Retention::Metadata,
        )
        .expect("observation");
        let writes = [probe, observation].map(|record| {
            let shared = shared.clone();
            tokio::spawn(async move { committed(&shared, record).await })
        });
        wait_until_queued(&shared, 2);
        drop(held);
        for write in writes {
            time::timeout(BOUND, write)
                .await
                .expect("bounded")
                .expect("joined")
                .expect("committed");
        }
        assert_eq!(
            *log.lock().expect("log"),
            [
                Point::BeforeCommit,
                Point::BeforeCommit,
                Point::Committed,
                Point::Committed
            ],
            "the probe and the observation share one commit"
        );

        // Killed before the probe's answer: copy the files a killed process leaves.
        for name in ["journal.db", "journal.db-wal"] {
            std::fs::copy(dir.join(name), dir.join("killed").join(name)).expect("copy");
        }
        drop(shared);
        let history = Journal::open(dir.join("killed/journal.db"), Durability::Process)
            .expect("reopen")
            .history()
            .expect("history");
        assert_eq!(history.probes.len(), 1);
        let unanswered = history.probes[0].outcome.as_ref().expect_err("unanswered");
        assert_eq!(unanswered.code, crate::journal::UNANSWERED);
        assert!(history.attempts[1].observation.is_some());
        let statuses = assess_history(&plan, &history, &HashSet::new());
        assert_eq!(statuses[0].assessment.probes, 1, "the probe stays charged");
    }
}
