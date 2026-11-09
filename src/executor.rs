//! Execute and recover an approved plan through the existing runtime.
//!
//! A session checks the latest approval against the current provider declaration
//! and policy, asks the provider whether each scope is still authorized,
//! reconciles operations whose outcome is unknown, and then dispatches every
//! operation whose next step is `dispatch`. Dispatch reuses [`run`]: a generated
//! program names only operation identities, Rust admits and schedules those calls
//! under the approved concurrency, and a journaling provider looks up each
//! operation's frozen arguments, records the intent durably, calls the trusted
//! provider, and records the observation durably before replying.
//!
//! Recovery never reruns an agent's program: it works only from the approved list.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
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
    RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety, approval_binding, assess,
};
use crate::runtime::{
    CallContext, DispatchPolicy, ProviderError, RunCancellation, RunConfig, RunError, RunReport,
    ToolCall, ToolProvider, millis, run,
};

/// Process settings for a session that are not part of the approval.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// `run` for the first session, `resume` afterwards.
    pub kind: SessionKind,
    /// Deno executable for the dispatch run.
    pub deno: PathBuf,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            kind: SessionKind::Run,
            deno: PathBuf::from("deno"),
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
    /// The dispatch run, including guest startup; see the trace for its phases.
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
    /// Operations probed in this session.
    pub probed: usize,
    /// Operations dispatched in this session.
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
/// # Errors
/// Returns [`SessionError::Refused`] (already recorded in the journal) when no
/// approval matches the current plan, provider declaration, and policy, or a
/// scope is not authorized now; [`SessionError::AlreadyStarted`] for `run` on a
/// journal with sessions; and journal or runtime failures. A dispatch run that
/// ends early, for example on cancellation, returns an `interrupted` outcome.
pub async fn run_session<P: RecoverableProvider>(
    journal: Journal,
    provider: P,
    options: SessionOptions,
    cancellation: RunCancellation,
) -> Result<SessionOutcome, SessionError> {
    let started = Instant::now();
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
    if let Err(reason) = authorize(&plan, provider.as_ref(), &policy, &cancellation).await {
        return refuse(&journal, session, reason).await;
    }
    let checks_ms = millis(started.elapsed());

    let reconcile_started = Instant::now();
    let statuses = assess_history(&plan, &history, &HashSet::new());
    let to_probe: Vec<&OperationStatus> = statuses
        .iter()
        .filter(|status| status.assessment.next == NextStep::Reconcile)
        .collect();
    let probed = probe(
        &journal,
        &plan,
        &history,
        &to_probe,
        &provider,
        &policy,
        session,
        &cancellation,
    )
    .await?;
    let reconcile_ms = millis(reconcile_started.elapsed());

    // Probes are the only facts written since the first read.
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
    let dispatched = dispatch.len();

    let dispatch_started = Instant::now();
    let mut trace = None;
    let mut interrupted = None;
    if !dispatch.is_empty() && !cancellation.is_cancelled() {
        let report = dispatch_run(
            &journal,
            &provider,
            dispatch,
            &policy,
            session,
            &options,
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
            interrupted = Some(report.status.as_str().to_string());
        }
        trace = Some(report);
    } else if cancellation.is_cancelled() {
        interrupted = Some("cancelled".into());
    }
    let dispatch_ms = millis(dispatch_started.elapsed());

    // An attempt the run stopped waiting for keeps an unknown outcome, with the reason.
    let reason = interrupted.clone().unwrap_or_else(|| "run_finished".into());
    // Writes the run abandoned may still commit; wait for them so a recorded result
    // is never replaced by "unobserved", and a late intent is still marked.
    let history = after_queued_writes(&journal, move |journal| {
        journal.record_unobserved(session, &reason)?;
        journal.history()
    })
    .await?;
    let operations = assess_history(&plan, &history, &probed);
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
    let detail = interrupted.map(|status| format!("the dispatch run ended with {status}"));
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
    approval.policy.validate()?;
    let current = provider.declaration();
    current.admits(plan)?;
    let current = current.for_plan(plan);
    let approved = &approval.declaration;
    if current.provider != approved.provider || current.account != approved.account {
        return Err(format!(
            "approved for {}/{}, but the provider now acts as {}/{}",
            approved.provider, approved.account, current.provider, current.account
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

/// Ask the provider whether every scope the plan uses is authorized now.
async fn authorize(
    plan: &FrozenPlan,
    provider: &impl RecoverableProvider,
    policy: &RecoveryPolicy,
    cancellation: &RunCancellation,
) -> Result<(), String> {
    let scopes: BTreeSet<_> = plan
        .plan()
        .operations
        .iter()
        .map(|operation| &operation.scope)
        .collect();
    for scope in scopes {
        let timeout = Duration::from_millis(policy.probe_timeout_ms);
        let context = ProbeContext {
            deadline: Instant::now() + timeout,
            cancellation: cancellation.clone(),
        };
        match time::timeout(timeout, provider.authorize(scope, context)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(format!(
                    "the provider refused authorization for {scope} ({})",
                    error.code
                ));
            }
            Err(_) => {
                return Err(format!(
                    "authorization for {scope} did not answer within {} ms",
                    policy.probe_timeout_ms
                ));
            }
        }
    }
    Ok(())
}

/// Probe each operation once, at most `policy.concurrency` at a time, and record
/// every answer. Returns the operations probed.
#[expect(clippy::too_many_arguments, reason = "one internal call site")]
async fn probe<P: RecoverableProvider>(
    journal: &Shared,
    plan: &FrozenPlan,
    history: &History,
    statuses: &[&OperationStatus],
    provider: &Arc<P>,
    policy: &RecoveryPolicy,
    session: u64,
    cancellation: &RunCancellation,
) -> Result<HashSet<String>, JournalError> {
    let mut probed = HashSet::new();
    let mut pending = statuses.iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < policy.concurrency && !cancellation.is_cancelled() {
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
            let provider = Arc::clone(provider);
            let timeout = Duration::from_millis(policy.probe_timeout_ms);
            let context = ProbeContext {
                deadline: Instant::now() + timeout,
                cancellation: cancellation.clone(),
            };
            running.spawn(async move {
                let started = Instant::now();
                let outcome = time::timeout(
                    timeout,
                    provider.reconcile(operation.clone(), keys.clone(), context),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(ProviderError {
                        code: "probe_timeout".into(),
                        message: "the reconciliation probe did not answer in time".into(),
                    })
                });
                (probe, operation.id, millis(started.elapsed()), outcome)
            });
        }
        let Some(result) = running.join_next().await else {
            break;
        };
        let (probe, operation, elapsed, outcome) =
            result.map_err(|error| JournalError::Storage(error.to_string()))?;
        probed.insert(operation);
        journaled(journal, move |journal| {
            journal.record_probe(probe, elapsed, &outcome)
        })
        .await?;
    }
    Ok(probed)
}

/// The trusted program that offers each operation to the runtime's scheduler.
/// It names operations only; their arguments never pass through the guest.
fn dispatcher(operations: &[(Operation, u32)]) -> String {
    let list: Vec<Value> = operations
        .iter()
        .map(|(operation, _)| json!([operation.id, operation.tool]))
        .collect();
    format!(
        "const operations = {};\n\
         const settled = await Promise.allSettled(operations.map(([operation, tool]) => tools.call(tool, {{ operation }})));\n\
         return {{ offered: settled.length, failed: settled.filter((result) => result.status === \"rejected\").length }};\n",
        Value::Array(list)
    )
}

async fn dispatch_run<P: RecoverableProvider>(
    journal: &Shared,
    provider: &Arc<P>,
    operations: Vec<(Operation, u32)>,
    policy: &RecoveryPolicy,
    session: u64,
    options: &SessionOptions,
    cancellation: &RunCancellation,
) -> Result<RunReport, RunError> {
    let code = dispatcher(&operations);
    let tools: BTreeSet<String> = operations
        .iter()
        .map(|(operation, _)| operation.tool.clone())
        .collect();
    let config = RunConfig {
        timeout_ms: policy.session_timeout_ms,
        max_calls: operations.len(),
        max_concurrency: policy.concurrency,
        deno: options.deno.clone(),
        allowed_tools: tools.into_iter().collect(),
        dispatch: DispatchPolicy::Rolling,
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
        attempt_timeout: Duration::from_millis(policy.attempt_timeout_ms),
        cancellation: cancellation.clone(),
    };
    run(&code, config, journaling, cancellation.clone()).await
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
    attempt_timeout: Duration,
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
                journal.record_observation(&withdrawn, Observed::Withdrawn)
            })
            .await;
            return Err(refusal("cancelled", "the run stopped before dispatch"));
        }
        let deadline = context.deadline.min(Instant::now() + self.attempt_timeout);
        let attempt = Attempt {
            operation,
            key: key.clone(),
            number,
        };
        let observed =
            match time::timeout_at(deadline, self.provider.execute(attempt, context)).await {
                Ok(Ok(value)) => Observed::Succeeded {
                    bytes: serde_json::to_vec(&value).map_or(0, |bytes| bytes.len() as u64),
                    value: Some(value),
                },
                Ok(Err(error)) => Observed::Failed {
                    error: error.error,
                    effect: error.effect,
                },
                Err(_) => Observed::TimedOut,
            };
        let reply = match &observed {
            Observed::Succeeded { .. } => Ok(json!({"operation": id, "attempt": number})),
            Observed::Failed { error, .. } => Err(refusal(&error.code, "the provider failed")),
            _ => Err(refusal(
                "attempt_timeout",
                "no result before the attempt deadline",
            )),
        };
        let recorded = journaled(&self.journal, move |journal| {
            journal.record_observation(&key, observed)
        })
        .await;
        match recorded {
            Ok(()) => reply,
            Err(error) => Err(self.journal_failed(&error, "the observed result was not recorded")),
        }
    }
}
