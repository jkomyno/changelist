//! Sessions against an in-test provider: what each conclusion leads to, and what
//! is refused before anything is sent. Real guest programs run, so Deno must be
//! on the PATH (or named by CODEMODE_TEST_DENO).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use codemode::{
    Approval, Attempt, AttemptError, CallContext, Conclusion, Durability, FailureEffect, Journal,
    NextStep, Operation, Plan, ProbeContext, ProviderDeclaration, ProviderError, ReconcileMethod,
    Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy, RepeatSafety,
    Resolution, RunCancellation, Scope, SessionError, SessionKind, SessionOptions, Verdict,
    approval_binding, run_session,
};
use serde_json::{Value, json};

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
        deno: std::env::var_os("CODEMODE_TEST_DENO").map_or_else(|| "deno".into(), Into::into),
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

fn approve(path: &PathBuf, plan: &codemode::FrozenPlan, remote: &Remote, policy: RecoveryPolicy) {
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
        session_timeout_ms: 2_000,
        ..RecoveryPolicy::default()
    };
    approve(&path, &plan, &remote, policy);
    remote.set("b", Behavior::LoseResponse);
    remote.set("c", Behavior::LoseResponse);
    remote.set("d", Behavior::Refuse);

    let first = run_session(
        open(&path),
        remote.clone(),
        options(SessionKind::Run),
        RunCancellation::default(),
    )
    .await
    .expect("session runs");
    assert_eq!(first.status, "interrupted", "{first:?}");
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
    let second = run_session(
        open(&path),
        remote.clone(),
        options(SessionKind::Resume),
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
    let cases: [(&str, Change); 3] = [
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
