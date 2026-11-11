//! Sessions against an in-test provider: what each conclusion leads to, and what
//! is refused before anything is sent. Approved plans run without a JavaScript engine.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codemode::{
    Approval, Attempt, AttemptError, CallContext, Conclusion, Durability, FailureEffect, Journal,
    NextStep, Operation, Plan, ProbeContext, ProviderDeclaration, ProviderError, RateLimit,
    ReconcileMethod, Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy,
    RepeatSafety, Resolution, RunCancellation, RunStatus, Scope, SessionError, SessionKind,
    SessionOptions, StoppedEarly, Verdict, approval_binding, run_session,
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
        ..SessionOptions::default()
    }
}

#[derive(Clone, Copy)]
enum Behavior {
    Succeed,
    /// Answer after `ms`, then succeed.
    Slow {
        ms: u64,
    },
    /// Apply, then never answer.
    LoseResponse,
    Refuse,
    /// Refuse the first `attempts` attempts with a rate limit that asks for
    /// `retry_after_ms`, then succeed.
    RateLimit {
        attempts: u32,
        retry_after_ms: u64,
    },
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
    /// When each request arrived, and when each rate-limit answer was given.
    arrivals: Arc<Mutex<Vec<Instant>>>,
    limits: Arc<Mutex<Vec<Instant>>>,
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
            arrivals: Arc::default(),
            limits: Arc::default(),
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
        self.arrivals.lock().unwrap().push(Instant::now());
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
            Behavior::Slow { ms } => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
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
                rate_limit: None,
            }),
            Behavior::RateLimit {
                attempts,
                retry_after_ms,
            } if attempt.number <= attempts => {
                self.limits.lock().unwrap().push(Instant::now());
                Err(AttemptError {
                    error: ProviderError {
                        code: "rate_limited".into(),
                        message: "slow down".into(),
                    },
                    effect: FailureEffect::NotApplied,
                    rate_limit: Some(RateLimit {
                        retry_after: Some(Duration::from_millis(retry_after_ms)),
                    }),
                })
            }
            Behavior::RateLimit { .. } => {
                self.applied.lock().unwrap().push(attempt.key);
                Ok(json!({"ok": true}))
            }
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
    assert_eq!(outcome.exit_code(), 124);
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

/// Approve `plan` for `remote` with `concurrency` and the default budgets.
fn approved(plan: &codemode::FrozenPlan, remote: &Remote, concurrency: usize) -> PathBuf {
    let path = journal_path();
    let policy = RecoveryPolicy {
        concurrency,
        ..RecoveryPolicy::default()
    };
    approve(&path, plan, remote, policy);
    path
}

async fn session(
    path: &PathBuf,
    remote: &Remote,
    options: SessionOptions,
) -> codemode::SessionOutcome {
    run_session(
        open(path),
        remote.clone(),
        options,
        RunCancellation::default(),
    )
    .await
    .expect("session")
}

#[tokio::test]
async fn a_session_stops_sending_once_enough_operations_are_not_applied() {
    let remote = Remote::new();
    let plan = plan(&[
        ("a", "comment"),
        ("b", "comment"),
        ("c", "comment"),
        ("d", "comment"),
        ("e", "comment"),
    ]);
    let path = approved(&plan, &remote, 1);
    remote.set("b", Behavior::Refuse);
    remote.set("d", Behavior::Refuse);

    // The default stops after the first operation that is not applied.
    let first = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(
        (first.status.as_str(), first.exit_code()),
        ("incomplete", 3)
    );
    assert_eq!(remote.requests(), ["a", "b"]);
    assert_eq!(
        first.stopped_early,
        Some(StoppedEarly {
            max_failures: 1,
            tripped_by: vec!["b".into()],
            not_sent: 3,
        })
    );
    assert_eq!(first.dispatched, 2);
    assert_eq!(
        conclusions(&first),
        [
            ("a".into(), Conclusion::Applied, NextStep::Done),
            ("b".into(), Conclusion::Refused, NextStep::Review),
            ("c".into(), Conclusion::Pending, NextStep::Dispatch),
            ("d".into(), Conclusion::Pending, NextStep::Dispatch),
            ("e".into(), Conclusion::Pending, NextStep::Dispatch),
        ],
        "an operation that was not sent stays as it was"
    );
    let failed = first.operations[1]
        .last_attempt
        .as_ref()
        .expect("attempted");
    assert_eq!(failed.code.as_deref(), Some("precondition_failed"));
    assert!(failed.answered, "the provider answered with an error");
    assert!(first.operations[2].last_attempt.is_none());

    // A resume sends what was held back, and stops again in the same way.
    let second = session(&path, &remote, options(SessionKind::Resume)).await;
    assert_eq!(remote.requests(), ["a", "b", "c", "d"]);
    assert_eq!(
        second.stopped_early,
        Some(StoppedEarly {
            max_failures: 1,
            tripped_by: vec!["d".into()],
            not_sent: 1,
        })
    );

    // A limit of 0 never stops.
    let third = session(
        &path,
        &remote,
        SessionOptions {
            max_failures: 0,
            ..options(SessionKind::Resume)
        },
    )
    .await;
    assert_eq!(remote.requests(), ["a", "b", "c", "d", "e"]);
    assert_eq!(third.stopped_early, None);
    assert_eq!(third.status, "needs_review");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_first_operation_of_a_session_is_sent_alone() {
    // Every operation would be refused. Four may be in flight at once, but
    // the first goes alone, so the provider receives one.
    let remote = Remote::new();
    let ids: Vec<String> = (0..12).map(|index| format!("op-{index:02}")).collect();
    let operations: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "comment")).collect();
    let plan = plan(&operations);
    let path = approved(&plan, &remote, 4);
    for id in &ids {
        remote.set(id, Behavior::Refuse);
    }
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(remote.requests(), ["op-00"]);
    assert_eq!(outcome.dispatched, 1);
    assert_eq!(
        outcome.stopped_early,
        Some(StoppedEarly {
            max_failures: 1,
            tripped_by: vec!["op-00".into()],
            not_sent: 11,
        })
    );

    // So does a resume: the first operation it sends goes alone again.
    let resumed = session(&path, &remote, options(SessionKind::Resume)).await;
    assert_eq!(remote.requests(), ["op-00", "op-01"]);
    assert_eq!(resumed.stopped_early.expect("stopped early").not_sent, 10);
    let _ = std::fs::remove_file(&path);

    // The others start only once the first has its answer, then run together.
    const FIRST: Duration = Duration::from_millis(300);
    let remote = Remote::new();
    let six =
        Plan::parse(&json!({"version": 1, "operations": plan.plan().operations[..6]}).to_string())
            .expect("valid plan");
    let path = approved(&six, &remote, 4);
    remote.set("op-00", Behavior::Slow { ms: 300 });
    for id in &ids[1..6] {
        remote.set(id, Behavior::Slow { ms: 100 });
    }
    let started = Instant::now();
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(outcome.status, "completed");
    let arrivals = remote.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 6);
    assert!(
        arrivals[1].duration_since(arrivals[0]) >= FIRST,
        "the second operation was sent before the first was answered"
    );
    // Five operations of 100 ms, four at a time, take two rounds after the first.
    assert!(
        started.elapsed() < FIRST + Duration::from_millis(100) * 5,
        "the others did not run together: {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn calls_already_sent_finish_after_the_session_stops() {
    let remote = Remote::new();
    let ids: Vec<String> = (0..12).map(|index| format!("op-{index:02}")).collect();
    let operations: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "comment")).collect();
    let plan = plan(&operations);
    let path = approved(&plan, &remote, 4);
    // The first operation is applied; every other one is refused.
    for id in &ids[1..] {
        remote.set(id, Behavior::Refuse);
    }
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    let refused = remote.requests().len() - 1;
    assert!(
        (1..=4).contains(&refused),
        "at most the calls in flight: {refused}"
    );
    assert_eq!(outcome.dispatched, refused + 1);
    let stop = outcome.stopped_early.expect("stopped early");
    assert_eq!((stop.tripped_by.len(), stop.not_sent), (1, 11 - refused));
    for status in &outcome.operations {
        let expected = if status.operation == "op-00" {
            (Conclusion::Applied, NextStep::Done)
        } else if remote.requests().contains(&status.operation) {
            (Conclusion::Refused, NextStep::Review)
        } else {
            (Conclusion::Pending, NextStep::Dispatch)
        };
        assert_eq!(
            (status.assessment.conclusion, status.assessment.next),
            expected,
            "{}",
            status.operation
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// A provider that records the arguments of every attempt it receives.
#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<(String, Value)>>>);

impl RecoverableProvider for Recording {
    fn declaration(&self) -> ProviderDeclaration {
        Remote::new().declaration()
    }

    async fn authorize(&self, _scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn execute(
        &self,
        attempt: Attempt,
        _context: CallContext,
    ) -> Result<Value, AttemptError> {
        let Operation { id, arguments, .. } = attempt.operation;
        self.0.lock().unwrap().push((id, arguments));
        Ok(Value::Null)
    }

    async fn reconcile(
        &self,
        _operation: Operation,
        _keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        Ok(Reconciliation {
            verdict: Verdict::Unknown,
            evidence: String::new(),
        })
    }
}

#[tokio::test]
async fn exact_values_and_explicit_nulls_reach_the_provider_unchanged() {
    // A plan whose operations pass `arguments`, written verbatim.
    let parse = |operations: &[(&str, &str)]| {
        let listed: Vec<String> = operations
            .iter()
            .map(|(id, arguments)| {
                format!(
                    r#"{{"id":"{id}","tool":"label","scope":{{"provider":"test","account":"bot"}},"arguments":{arguments}}}"#
                )
            })
            .collect();
        Plan::parse(&format!(
            r#"{{"version":1,"operations":[{}]}}"#,
            listed.join(",")
        ))
        .expect("valid plan")
    };
    assert_ne!(
        parse(&[("op", r#"{"a":null,"b":1}"#)]).digest(),
        parse(&[("op", r#"{"b":1}"#)]).digest(),
        "an explicit null is part of the approved plan"
    );

    let plan = parse(&[
        (
            "exact",
            r#"{"max":9007199254740991,"min":-9007199254740991,"text":"18446744073709551616"}"#,
        ),
        ("null", r#"{"a":null,"b":1}"#),
        ("absent", r#"{"b":1}"#),
    ]);
    let provider = Recording::default();
    let path = journal_path();
    approve(&path, &plan, &provider, RecoveryPolicy::default());
    let outcome = run_session(
        open(&path),
        provider.clone(),
        options(SessionKind::Run),
        RunCancellation::default(),
    )
    .await
    .expect("session runs");
    assert_eq!(outcome.status, "completed", "{outcome:?}");
    let calls = provider.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "{calls:?}");
    let arguments = |operation: &str| {
        let (_, arguments) = calls
            .iter()
            .find(|(id, _)| id == operation)
            .expect("operation called");
        (arguments.to_string(), arguments.clone())
    };
    let (line, exact) = arguments("exact");
    assert!(
        line.contains("9007199254740991") && line.contains("-9007199254740991"),
        "{line}"
    );
    assert_eq!(
        exact,
        json!({"max": 9_007_199_254_740_991_u64, "min": -9_007_199_254_740_991_i64,
               "text": "18446744073709551616"})
    );
    let (line, null) = arguments("null");
    assert!(line.contains(r#""a":null"#), "{line}");
    assert_eq!(null, json!({"a": null, "b": 1}));
    let (_, absent) = arguments("absent");
    assert_eq!(absent, json!({"b": 1}), "absent stays absent");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_rate_limit_holds_every_operation_until_the_wait_is_over() {
    const WAIT: Duration = Duration::from_millis(400);
    let remote = Remote::new();
    let plan = plan(&[
        ("a", "comment"),
        ("b", "comment"),
        ("c", "comment"),
        ("d", "comment"),
        ("e", "comment"),
    ]);
    let path = approved(&plan, &remote, 2);
    remote.set(
        "a",
        Behavior::RateLimit {
            attempts: 1,
            retry_after_ms: 400,
        },
    );
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(
        (outcome.status.as_str(), outcome.exit_code()),
        ("completed", 0)
    );
    assert_eq!(outcome.stopped_early, None);
    // "a" was sent twice: the refused attempt came out of the approved budget.
    assert_eq!(remote.requests(), ["a", "a", "b", "c", "d", "e"]);
    assert_eq!(outcome.operations[0].assessment.attempts, 2);
    assert_eq!(outcome.dispatched, 6);
    let limited = remote.limits.lock().unwrap()[0];
    let arrivals = remote.arrivals.lock().unwrap().clone();
    let later: Vec<Duration> = arrivals
        .iter()
        .filter_map(|at| at.checked_duration_since(limited))
        .filter(|after| !after.is_zero())
        .collect();
    // "b" was sent with "a" and may arrive just after the refusal. Without
    // the wait, "c", "d", and "e" would follow it at once.
    let during = later.iter().filter(|after| **after < WAIT).count();
    assert!(
        during <= 1,
        "only a call already sent arrives during the wait: {later:?}"
    );
    assert!(
        later.len() - during >= 4,
        "the retry and the three queued operations follow the wait: {later:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_lasting_rate_limit_spends_the_attempt_budget_and_stops_the_session() {
    let remote = Remote::new();
    let plan = plan(&[("a", "comment"), ("b", "comment"), ("c", "comment")]);
    let path = approved(&plan, &remote, 1);
    remote.set(
        "a",
        Behavior::RateLimit {
            attempts: u32::MAX,
            retry_after_ms: 50,
        },
    );
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(
        (outcome.status.as_str(), outcome.exit_code()),
        ("incomplete", 3)
    );
    assert_eq!(
        remote.requests(),
        ["a", "a", "a"],
        "the default budget is 3"
    );
    let limited = &outcome.operations[0];
    assert_eq!(
        (limited.assessment.conclusion, limited.assessment.next),
        (Conclusion::NotApplied, NextStep::Review)
    );
    let last = limited.last_attempt.as_ref().expect("attempted");
    assert_eq!(
        (last.number, last.code.as_deref(), last.effect.as_deref()),
        (3, Some("rate_limited"), Some("not_applied"))
    );
    assert_eq!(
        outcome.stopped_early,
        Some(StoppedEarly {
            max_failures: 1,
            tripped_by: vec!["a".into()],
            not_sent: 2,
        })
    );
    // The wait doubles on each repeat: 50 ms, then 100 ms.
    let arrivals = remote.arrivals.lock().unwrap().clone();
    assert!(arrivals[1] - arrivals[0] >= Duration::from_millis(50));
    assert!(arrivals[2] - arrivals[1] >= Duration::from_millis(100));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_rate_limit_that_outlasts_the_deadline_ends_the_operation_at_once() {
    let remote = Remote::new();
    let plan = plan(&[("a", "comment"), ("b", "comment")]);
    let path = approved(&plan, &remote, 1);
    remote.set(
        "a",
        Behavior::RateLimit {
            attempts: u32::MAX,
            retry_after_ms: 30_000,
        },
    );
    let started = Instant::now();
    let outcome = session(&path, &remote, within(SessionKind::Run, 5_000)).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the session did not wait for its deadline"
    );
    assert_eq!(outcome.interruption, None);
    assert_eq!(
        (outcome.status.as_str(), outcome.exit_code()),
        ("incomplete", 3)
    );
    assert_eq!(remote.requests(), ["a"]);
    assert_eq!(
        conclusions(&outcome),
        [
            ("a".into(), Conclusion::NotApplied, NextStep::Dispatch),
            ("b".into(), Conclusion::Pending, NextStep::Dispatch),
        ],
        "both are left for a resume"
    );
    assert_eq!(
        outcome.stopped_early,
        Some(StoppedEarly {
            max_failures: 1,
            tripped_by: vec!["a".into()],
            not_sent: 1,
        })
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_large_plan_is_not_cut_short_by_the_size_of_its_own_replies() {
    // 3,000 operations with the longest identities: their acknowledgments
    // together are larger than a run's default output budget.
    let remote = Remote::new();
    let ids: Vec<String> = (0..3_000).map(|index| format!("{index:0>64}")).collect();
    let operations: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "comment")).collect();
    let plan = plan(&operations);
    let path = approved(&plan, &remote, 4);
    let outcome = session(&path, &remote, options(SessionKind::Run)).await;
    assert_eq!(
        (outcome.status.as_str(), outcome.detail.as_deref()),
        ("completed", None)
    );
    assert_eq!(remote.requests().len(), 3_000);
    assert!(
        outcome
            .operations
            .iter()
            .all(|status| status.assessment.conclusion == Conclusion::Applied)
    );
    let _ = std::fs::remove_file(&path);
}
