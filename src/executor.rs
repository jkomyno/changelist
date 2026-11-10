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
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::{self, Instant};

use crate::journal::{History, Journal, JournalError, Record, SessionKind, storage};
use crate::plan::{FrozenPlan, Operation};
use crate::recovery::{
    Approval, Assessment, Attempt, Fact, NextStep, Observed, ProbeContext, ReconcileMethod,
    RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety, Retention,
    approval_binding, assess, machine_code,
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

impl SessionOutcome {
    /// Process exit status for `codemode plan run|resume`: 0 when `completed`;
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
            (
                operation.id.clone(),
                Entry {
                    operation,
                    number,
                    key,
                },
            )
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

/// One operation approved for dispatch in this run.
#[derive(Clone)]
struct Entry {
    operation: Operation,
    /// The attempt number this dispatch records.
    number: u32,
    /// The attempt key.
    key: String,
}

/// The runtime's provider for a dispatch run: it journals each attempt around the
/// trusted provider's call.
struct JournalingProvider<P> {
    journal: Shared,
    provider: Arc<P>,
    /// Operation identity -> its entry.
    entries: HashMap<String, Entry>,
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
            number,
            key,
        } = self.claim(&call)?;
        let id = operation.id.clone();
        let session = self.session;
        let call_id = call.id;
        let intent = Record::intent(&key, &id, number, session, Some(call_id));
        let recorded = committed(&self.journal, intent).await;
        if let Err(error) = recorded {
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
        let recorded = match Record::observation(&key, observed, self.retention) {
            Ok(observation) => committed(&self.journal, observation).await,
            Err(error) => Err(error),
        };
        match recorded {
            Ok(_) => reply,
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
                    key: format!("key-{id}-1"),
                };
                (id.to_string(), entry)
            })
            .collect();
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
