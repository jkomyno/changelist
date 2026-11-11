//! Sessions against an in-test provider: what each conclusion leads to, and what
//! is refused before anything is sent. Approved plans run without a JavaScript engine.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codemode::{
    Approval, Attempt, AttemptError, CallContext, Conclusion, Durability, FailureEffect, Journal,
    NextStep, Operation, Plan, ProbeContext, ProviderDeclaration, ProviderError, ReconcileMethod,
    Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety,
    Resolution, RunCancellation, RunStatus, Scope, SessionError, SessionKind, SessionOptions,
    Verdict, approval_binding, run_session,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A change to the provider made after approval.
type Change = Box<dyn Fn(&mut Remote)>;

fn journal_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "codemode-recovery-{}-{}.db",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn options(kind: SessionKind) -> SessionOptions {
    SessionOptions {
        kind,
        ..SessionOptions::default()
    }
}

/// A session whose whole run, from authorization to dispatch, ends after `ms`.
fn within(kind: SessionKind, ms: u64) -> SessionOptions {
    SessionOptions {
        kind,
        deadline: Duration::from_millis(ms),
    }
}

#[derive(Clone, Copy)]
enum Behavior {
    Succeed,
    /// Apply, then never answer.
    LoseResponse,
    Refuse,
}

/// A provider with a remote "applied" set, so tests can tell attempts from effects.
#[derive(Clone)]
struct Remote {
    contracts: BTreeMap<String, RecoveryContract>,
    /// Tool definition digests, as a provider with externally defined tools declares them.
    definitions: BTreeMap<String, codemode::Digest>,
    account: String,
    behaviors: Arc<Mutex<HashMap<String, Behavior>>>,
    /// Attempt keys the provider applied, in order.
    applied: Arc<Mutex<Vec<String>>>,
    /// Operations the provider received, in order.
    requests: Arc<Mutex<Vec<String>>>,
    authorized: bool,
}

impl Remote {
    fn new() -> Self {
        let contract = |repeat, reconcile| RecoveryContract { repeat, reconcile };
        Self {
            contracts: BTreeMap::from([
                (
                    "label".to_string(),
                    contract(RepeatSafety::Safe, ReconcileMethod::Receipt),
                ),
                (
                    "comment".to_string(),
                    contract(RepeatSafety::Unsafe, ReconcileMethod::None),
                ),
            ]),
            definitions: BTreeMap::from([("label".to_string(), codemode::Digest::of(b"v1"))]),
            account: "bot".into(),
            behaviors: Arc::default(),
            applied: Arc::default(),
            requests: Arc::default(),
            authorized: true,
        }
    }

    fn set(&self, operation: &str, behavior: Behavior) {
        self.behaviors
            .lock()
            .unwrap()
            .insert(operation.into(), behavior);
    }

    /// Operations the provider received, sorted: concurrent dispatch has no fixed order.
    fn requests(&self) -> Vec<String> {
        let mut requests = self.requests.lock().unwrap().clone();
        requests.sort();
        requests
    }
}

impl RecoverableProvider for Remote {
    fn declaration(&self) -> ProviderDeclaration {
        ProviderDeclaration {
            provider: "test".into(),
            account: self.account.clone(),
            contracts: self.contracts.clone(),
            retention: codemode::Retention::default(),
            definitions: self.definitions.clone(),
        }
    }

    async fn authorize(&self, _scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        if self.authorized {
            Ok(())
        } else {
            Err(ProviderError {
                code: "unauthorized".into(),
                message: "token revoked".into(),
            })
        }
    }

    async fn execute(
        &self,
        attempt: Attempt,
        _context: CallContext,
    ) -> Result<Value, AttemptError> {
        let id = attempt.operation.id.clone();
        self.requests.lock().unwrap().push(id.clone());
        let behavior = self
            .behaviors
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(Behavior::Succeed);
        match behavior {
            Behavior::Succeed => {
                self.applied.lock().unwrap().push(attempt.key);
                Ok(json!({"ok": true}))
            }
            Behavior::LoseResponse => {
                self.applied.lock().unwrap().push(attempt.key);
                std::future::pending().await
            }
            Behavior::Refuse => Err(AttemptError {
                error: ProviderError {
                    code: "precondition_failed".into(),
                    message: "issue closed".into(),
                },
                effect: FailureEffect::Refused,
            }),
        }
    }

    async fn reconcile(
        &self,
        _operation: Operation,
        keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        let applied = self.applied.lock().unwrap();
        let verdict = if keys.iter().any(|key| applied.contains(key)) {
            Verdict::Applied
        } else {
            Verdict::NotApplied
        };
        Ok(Reconciliation {
            verdict,
            evidence: "receipt lookup".into(),
        })
    }
}

fn plan(operations: &[(&str, &str)]) -> codemode::FrozenPlan {
    let operations: Vec<Value> = operations
        .iter()
        .map(|(id, tool)| {
            json!({"id": id, "tool": tool, "scope": {"provider": "test", "account": "bot"},
                   "arguments": {"issue": 1}})
        })
        .collect();
    Plan::parse(&json!({"version": 1, "operations": operations}).to_string()).expect("valid plan")
}

fn approve(
    path: &PathBuf,
    plan: &codemode::FrozenPlan,
    remote: &impl RecoverableProvider,
    policy: RecoveryPolicy,
) {
    let mut journal = Journal::create(path, plan, Durability::Process).expect("create");
    let declaration = remote.declaration().for_plan(plan);
    journal
        .approve(&Approval {
            binding: approval_binding(plan.digest(), &declaration, &policy),
            plan: plan.digest().clone(),
            declaration,
            policy,
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("approve");
}

fn open(path: &PathBuf) -> Journal {
    Journal::open(path, Durability::Process).expect("open")
}

fn conclusions(outcome: &codemode::SessionOutcome) -> Vec<(String, Conclusion, NextStep)> {
    outcome
        .operations
        .iter()
        .map(|status| {
            (
                status.operation.clone(),
                status.assessment.conclusion,
                status.assessment.next,
            )
        })
        .collect()
}

#[tokio::test]
async fn a_lost_response_is_reconciled_instead_of_repeated() {
    let path = journal_path();
    let remote = Remote::new();
    let plan = plan(&[
        ("a", "label"),
        ("b", "label"),
        ("c", "comment"),
        ("d", "label"),
    ]);
    let policy = RecoveryPolicy {
        concurrency: 4,
        ..RecoveryPolicy::default()
    };
    approve(&path, &plan, &remote, policy);
    remote.set("b", Behavior::LoseResponse);
    remote.set("c", Behavior::LoseResponse);
    remote.set("d", Behavior::Refuse);

    // The session deadline drops the calls still held after their intents committed.
    let first = run_session(
        open(&path),
        remote.clone(),
        within(SessionKind::Run, 500),
        RunCancellation::default(),
    )
    .await
    .expect("session runs");
    let trace = first.trace.as_ref().expect("dispatch trace");
    assert_eq!(trace.schema_version, 5);
    assert_eq!(trace.backend, codemode::ExecutionBackend::Native);
    assert!(trace.code.is_empty());
    assert!(trace.output.is_none());
    assert_eq!(trace.bytes.from_guest, 0);
    assert_eq!(trace.bytes.to_guest, 0);
    assert_eq!(first.status, "interrupted", "{first:?}");
    assert_eq!(first.interruption, Some(RunStatus::Deadline));
    assert_eq!(trace.status, RunStatus::Deadline);
    let history = open(&path).history().expect("history");
    for lost in ["b", "c"] {
        let attempt = history
            .attempts
            .iter()
            .find(|attempt| attempt.operation == lost)
            .expect("intent recorded");
        assert_eq!(
            attempt.observation.as_ref().map(|o| &o.observed),
            Some(&codemode::Observed::Unobserved {
                reason: "deadline".into()
            }),
            "{lost}"
        );
    }
    assert_eq!(
        conclusions(&first),
        [
            ("a".into(), Conclusion::Applied, NextStep::Done),
            ("b".into(), Conclusion::Unknown, NextStep::Reconcile),
            ("c".into(), Conclusion::Unknown, NextStep::Review),
            ("d".into(), Conclusion::Refused, NextStep::Review),
        ]
    );

    // The executor is replaced; the provider's state is not.
    remote.set("b", Behavior::Succeed);
    remote.set("c", Behavior::Succeed);
    // The deadline is not approved: a resume with another one needs no new approval.
    let second = run_session(
        open(&path),
        remote.clone(),
        within(SessionKind::Resume, 60_000),
        RunCancellation::default(),
    )
    .await
    .expect("resume runs");
    assert_eq!(second.status, "needs_review", "{second:?}");
    assert_eq!((second.probed, second.dispatched), (1, 0));
    assert_eq!(
        second.operations[1].assessment.conclusion,
        Conclusion::Applied
    );
    // Nothing was sent again: b was reconciled, c and d wait for a person.
    assert_eq!(remote.requests(), ["a", "b", "c", "d"]);

    // A person confirms c applied; d stays refused.
    let mut journal = open(&path);
    journal
        .record_review("c", Resolution::Applied, "tester", Some("comment present"))
        .expect("review");
    drop(journal);
    let third = run_session(
        open(&path),
        remote.clone(),
        options(SessionKind::Resume),
        RunCancellation::default(),
    )
    .await
    .expect("resume runs");
    assert_eq!(
        third.operations[2].assessment.conclusion,
        Conclusion::Applied
    );
    assert_eq!(third.operations[3].assessment.next, NextStep::Review);
    assert_eq!(remote.requests(), ["a", "b", "c", "d"]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn changed_scope_contracts_or_authorization_refuse_before_sending() {
    let plan = plan(&[("a", "label")]);
    let cases: [(&str, Change); 4] = [
        (
            "provider acts as test/other",
            Box::new(|r| r.account = "other".into()),
        ),
        (
            "contracts changed",
            Box::new(|r| {
                r.contracts.insert(
                    "label".into(),
                    RecoveryContract {
                        repeat: RepeatSafety::Unsafe,
                        reconcile: ReconcileMethod::Receipt,
                    },
                );
            }),
        ),
        (
            "tool definitions changed",
            Box::new(|r| {
                r.definitions
                    .insert("label".into(), codemode::Digest::of(b"v2"));
            }),
        ),
        ("refused authorization", Box::new(|r| r.authorized = false)),
    ];
    for (expected, change) in cases {
        let path = journal_path();
        let mut remote = Remote::new();
        approve(&path, &plan, &remote, RecoveryPolicy::default());
        change(&mut remote);
        let error = run_session(
            open(&path),
            remote.clone(),
            options(SessionKind::Run),
            RunCancellation::default(),
        )
        .await
        .expect_err("refused");
        assert!(
            matches!(&error, SessionError::Refused { reason, .. } if reason.contains(expected)),
            "{error} should mention {expected}"
        );
        assert!(remote.requests().is_empty());
        let history = open(&path).history().expect("history");
        assert_eq!(history.sessions[0].end.as_ref().unwrap().status, "refused");
        let _ = std::fs::remove_file(&path);
    }
}

#[tokio::test]
async fn a_second_run_is_refused_and_an_unapproved_plan_sends_nothing() {
    let path = journal_path();
    let remote = Remote::new();
    let plan = plan(&[("a", "label")]);
    drop(Journal::create(&path, &plan, Durability::Process).expect("create"));
    let error = run_session(
        open(&path),
        remote.clone(),
        options(SessionKind::Run),
        RunCancellation::default(),
    )
    .await
    .expect_err("not approved");
    assert!(error.to_string().contains("not been approved"), "{error}");
    let error = run_session(
        open(&path),
        remote.clone(),
        options(SessionKind::Run),
        RunCancellation::default(),
    )
    .await
    .expect_err("already started");
    assert!(matches!(error, SessionError::AlreadyStarted), "{error}");
    assert!(remote.requests().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[derive(Clone, Copy)]
enum HeldPhase {
    Authorization,
    Reconciliation,
}

/// Yields while a read is pending, deliberately ignoring its cancellation context.
#[derive(Clone)]
struct HeldReads {
    remote: Remote,
    phase: HeldPhase,
    entered: Arc<Notify>,
    dropped: Arc<AtomicU64>,
    scopes: Arc<Mutex<Vec<Scope>>>,
    probes: Arc<Mutex<Vec<String>>>,
}

struct PendingRead(Arc<AtomicU64>);

impl Drop for PendingRead {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl HeldReads {
    fn new(remote: Remote, phase: HeldPhase) -> Self {
        Self {
            remote,
            phase,
            entered: Arc::default(),
            dropped: Arc::default(),
            scopes: Arc::default(),
            probes: Arc::default(),
        }
    }

    async fn hold(&self) {
        let _pending = PendingRead(Arc::clone(&self.dropped));
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}

impl RecoverableProvider for HeldReads {
    fn declaration(&self) -> ProviderDeclaration {
        self.remote.declaration()
    }

    async fn authorize(&self, scope: &Scope, context: ProbeContext) -> Result<(), ProviderError> {
        self.scopes.lock().unwrap().push(scope.clone());
        if matches!(self.phase, HeldPhase::Authorization) {
            self.hold().await;
        }
        self.remote.authorize(scope, context).await
    }

    async fn execute(&self, attempt: Attempt, context: CallContext) -> Result<Value, AttemptError> {
        self.remote.execute(attempt, context).await
    }

    async fn reconcile(
        &self,
        operation: Operation,
        keys: Vec<String>,
        context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        self.probes.lock().unwrap().push(operation.id.clone());
        if matches!(self.phase, HeldPhase::Reconciliation) {
            self.hold().await;
        }
        self.remote.reconcile(operation, keys, context).await
    }
}

#[tokio::test]
async fn cancellation_interrupts_authorization_without_reading_the_next_scope() {
    let path = journal_path();
    let plan = Plan::parse(
        r#"{"version":1,"operations":[
          {"id":"a","tool":"label","scope":{"provider":"test","account":"bot","destination":"a"},"arguments":{}},
          {"id":"b","tool":"label","scope":{"provider":"test","account":"bot","destination":"b"},"arguments":{}}
        ]}"#,
    )
    .expect("distinct scopes");
    let provider = HeldReads::new(Remote::new(), HeldPhase::Authorization);
    approve(&path, &plan, &provider, RecoveryPolicy::default());
    let cancellation = RunCancellation::default();
    let session = tokio::spawn(run_session(
        open(&path),
        provider.clone(),
        options(SessionKind::Run),
        cancellation.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .expect("authorization entered");
    cancellation.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(2), session)
        .await
        .expect("Rust cancellation must not wait for the session deadline")
        .expect("session task")
        .expect("cancellation interrupts rather than refusing authorization");
    assert_eq!(outcome.status, "interrupted");
    assert_eq!(outcome.interruption, Some(RunStatus::Cancelled));
    assert_eq!((outcome.probed, outcome.dispatched), (0, 0));
    assert!(outcome.trace.is_none());
    assert_eq!(provider.scopes.lock().unwrap().len(), 1);
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.probes.lock().unwrap().is_empty());
    assert!(provider.remote.requests().is_empty());

    // A pre-cancelled resume must not even enter the first authorization.
    let outcome = run_session(
        open(&path),
        provider.clone(),
        options(SessionKind::Resume),
        cancellation,
    )
    .await
    .expect("pre-cancelled resume");
    assert_eq!(outcome.status, "interrupted");
    assert_eq!(provider.scopes.lock().unwrap().len(), 1);
    let history = open(&path)
        .history()
        .expect("journal reopens after cancellation");
    assert!(history.attempts.is_empty());
    assert!(history.probes.is_empty());
    assert_eq!(history.sessions.len(), 2);
    assert!(
        history
            .sessions
            .iter()
            .all(|session| { session.end.as_ref().unwrap().status == "interrupted" })
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cancellation_joins_probes_and_keeps_their_unanswered_budget_across_resumes() {
    let path = journal_path();
    let plan = plan(&[
        ("a", "label"),
        ("b", "label"),
        ("d", "label"),
        ("c", "label"),
    ]);
    let mut remote = Remote::new();
    remote.contracts.insert(
        "label".into(),
        RecoveryContract {
            repeat: RepeatSafety::Unsafe,
            reconcile: ReconcileMethod::Receipt,
        },
    );
    let provider = HeldReads::new(remote, HeldPhase::Reconciliation);
    approve(
        &path,
        &plan,
        &provider,
        RecoveryPolicy {
            concurrency: 2,
            max_probes: 2,
            ..RecoveryPolicy::default()
        },
    );
    // Model a crash after three intents; c has never been dispatched.
    let mut journal = open(&path);
    let initial = journal
        .begin_session(SessionKind::Run)
        .expect("initial session");
    journal
        .record_intent("key-a", "a", 1, initial, None)
        .expect("intent a");
    journal
        .record_intent("key-b", "b", 1, initial, None)
        .expect("intent b");
    journal
        .record_intent("key-d", "d", 1, initial, None)
        .expect("intent d");
    journal
        .end_session(initial, "interrupted", None, None)
        .expect("initial end");
    drop(journal);

    for (round, started, total) in [(1_usize, 2, 2), (2, 2, 4), (3, 1, 5), (4, 1, 6)] {
        let cancellation = RunCancellation::default();
        let session = tokio::spawn(run_session(
            open(&path),
            provider.clone(),
            options(SessionKind::Resume),
            cancellation.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while provider.probes.lock().unwrap().len() < total {
                provider.entered.notified().await;
            }
        })
        .await
        .expect("admitted probes entered");
        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(2), session)
            .await
            .expect("Rust cancellation must not wait for the session deadline")
            .expect("session task")
            .expect("interrupted resume");
        assert_eq!(outcome.status, "interrupted");
        assert_eq!((outcome.probed, outcome.dispatched), (started, 0));
        assert!(outcome.trace.is_none());
        assert_eq!(provider.dropped.load(Ordering::SeqCst), total as u64);
        assert!(provider.remote.requests().is_empty());
        assert_eq!(
            conclusions(&outcome),
            [
                ("a".into(), Conclusion::Unknown, NextStep::Review),
                ("b".into(), Conclusion::Unknown, NextStep::Review),
                (
                    "d".into(),
                    Conclusion::Unknown,
                    if round <= 2 {
                        NextStep::Reconcile
                    } else {
                        NextStep::Review
                    }
                ),
                ("c".into(), Conclusion::Pending, NextStep::Dispatch),
            ]
        );
        let history = open(&path)
            .history()
            .expect("journal reopens after probe cancellation");
        assert_eq!(history.attempts.len(), 3);
        assert_eq!(history.probes.len(), total);
        assert!(
            history
                .probes
                .iter()
                .all(|probe| { probe.outcome.as_ref().unwrap_err().code == codemode::UNANSWERED })
        );
        assert_eq!(
            history
                .probes
                .iter()
                .filter(|probe| probe.operation == "a")
                .count(),
            round.min(2)
        );
        assert_eq!(
            history
                .probes
                .iter()
                .filter(|probe| probe.operation == "b")
                .count(),
            round.min(2)
        );
        assert_eq!(
            history
                .probes
                .iter()
                .filter(|probe| probe.operation == "d")
                .count(),
            round.saturating_sub(2)
        );
        assert!(
            history
                .probes
                .iter()
                .all(|probe| probe.keys == [format!("key-{}", probe.operation)])
        );
        assert_eq!(
            history
                .sessions
                .last()
                .unwrap()
                .end
                .as_ref()
                .unwrap()
                .status,
            "interrupted"
        );
    }

    // Exhausted unanswered probes stay unknown and cannot trigger more reads or retries.
    let outcome = run_session(
        open(&path),
        provider.clone(),
        options(SessionKind::Resume),
        RunCancellation::default(),
    )
    .await
    .expect("resume after probe budget exhaustion");
    assert_eq!(outcome.status, "needs_review");
    assert_eq!((outcome.probed, outcome.dispatched), (0, 1));
    assert_eq!(provider.probes.lock().unwrap().len(), 6);
    assert_eq!(provider.remote.requests(), ["c"]);
    assert_eq!(
        outcome.operations[0].assessment.conclusion,
        Conclusion::Unknown
    );
    assert_eq!(
        outcome.operations[1].assessment.conclusion,
        Conclusion::Unknown
    );
    assert_eq!(
        outcome.operations[2].assessment.conclusion,
        Conclusion::Unknown
    );
    assert_eq!(
        outcome.operations[3].assessment.conclusion,
        Conclusion::Applied
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn an_unanswered_authorization_ends_the_session_at_its_deadline_without_dispatch() {
    let path = journal_path();
    let plan = plan(&[("a", "label")]);
    let provider = HeldReads::new(Remote::new(), HeldPhase::Authorization);
    approve(&path, &plan, &provider, RecoveryPolicy::default());
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(
            open(&path),
            provider.clone(),
            within(SessionKind::Run, 100),
            RunCancellation::default(),
        ),
    )
    .await
    .expect("the session deadline bounds authorization")
    .expect("a deadline interrupts rather than refusing");
    assert_eq!(outcome.status, "interrupted", "{outcome:?}");
    assert_eq!(outcome.interruption, Some(RunStatus::Deadline));
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("deadline")),
        "{outcome:?}"
    );
    assert_eq!((outcome.probed, outcome.dispatched), (0, 0));
    assert!(outcome.trace.is_none());
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    assert!(provider.remote.requests().is_empty());
    let history = open(&path).history().expect("history");
    assert!(history.attempts.is_empty());
    assert_eq!(
        history.sessions[0].end.as_ref().unwrap().status,
        "interrupted"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn an_unanswered_probe_is_charged_and_recorded_at_the_deadline() {
    let path = journal_path();
    let plan = plan(&[("a", "label")]);
    let mut remote = Remote::new();
    remote.contracts.insert(
        "label".into(),
        RecoveryContract {
            repeat: RepeatSafety::Unsafe,
            reconcile: ReconcileMethod::Receipt,
        },
    );
    let provider = HeldReads::new(remote, HeldPhase::Reconciliation);
    approve(
        &path,
        &plan,
        &provider,
        RecoveryPolicy {
            max_probes: 1,
            ..RecoveryPolicy::default()
        },
    );
    // Model a crash after a's intent.
    let mut journal = open(&path);
    let initial = journal.begin_session(SessionKind::Run).expect("session");
    journal
        .record_intent("key-a", "a", 1, initial, None)
        .expect("intent a");
    journal
        .end_session(initial, "interrupted", None, None)
        .expect("end");
    drop(journal);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(
            open(&path),
            provider.clone(),
            within(SessionKind::Resume, 100),
            RunCancellation::default(),
        ),
    )
    .await
    .expect("the session deadline bounds probes")
    .expect("interrupted resume");
    assert_eq!(outcome.status, "interrupted", "{outcome:?}");
    assert_eq!(outcome.interruption, Some(RunStatus::Deadline));
    assert_eq!((outcome.probed, outcome.dispatched), (1, 0));
    assert_eq!(provider.dropped.load(Ordering::SeqCst), 1);
    let history = open(&path).history().expect("history");
    assert_eq!(history.probes.len(), 1);
    assert_eq!(
        history.probes[0].outcome.as_ref().unwrap_err().code,
        "probe_timeout"
    );

    // The next resume sees the charge: the probe budget is spent, so a person decides.
    let outcome = run_session(
        open(&path),
        provider.remote.clone(),
        options(SessionKind::Resume),
        RunCancellation::default(),
    )
    .await
    .expect("resume");
    assert_eq!(outcome.status, "needs_review", "{outcome:?}");
    assert_eq!((outcome.probed, outcome.dispatched), (0, 0));
    assert_eq!(
        conclusions(&outcome),
        [("a".into(), Conclusion::Unknown, NextStep::Review)]
    );
    assert_eq!(provider.probes.lock().unwrap().len(), 1);
    assert!(provider.remote.requests().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn concurrency_is_approved_and_the_deadline_is_not() {
    let path = journal_path();
    let remote = Remote::new();
    let plan = plan(&[("a", "label")]);
    let approved = RecoveryPolicy::default();
    approve(&path, &plan, &remote, approved);
    let changed = RecoveryPolicy {
        concurrency: approved.concurrency + 1,
        ..approved
    };
    let declaration = remote.declaration().for_plan(&plan);
    assert_ne!(
        approval_binding(plan.digest(), &declaration, &approved),
        approval_binding(plan.digest(), &declaration, &changed),
        "the binding covers concurrency"
    );
    // A record claiming another concurrency under the old binding is refused.
    let mut journal = open(&path);
    journal
        .approve(&Approval {
            binding: approval_binding(plan.digest(), &declaration, &approved),
            plan: plan.digest().clone(),
            declaration,
            policy: changed,
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("record");
    drop(journal);
    let error = run_session(
        open(&path),
        remote.clone(),
        within(SessionKind::Run, 60_000),
        RunCancellation::default(),
    )
    .await
    .expect_err("refused");
    assert!(error.to_string().contains("does not bind"), "{error}");
    assert!(remote.requests().is_empty());
    let _ = std::fs::remove_file(&path);
}
