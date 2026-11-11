//! Retention: what a journal keeps of a provider's replies under each policy,
//! and which approvals a session refuses before it asks the provider anything.
//! Sessions run against an in-test provider, without a JavaScript engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use codemode::{
    Approval, Attempt, AttemptError, CallContext, Conclusion, Durability, FailureEffect, Journal,
    JournalReader, NextStep, Observed, Operation, Plan, ProbeContext, ProviderDeclaration,
    ProviderError, Reconciliation, RecoverableProvider, RecoveryPolicy, RunCancellation, Scope,
    SessionError, SessionKind, SessionOptions, SessionOutcome, Verdict, approval_binding,
    run_session,
};
use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Distinct synthetic markers for each kind of provider text.
const VALUE: &str = "MARK-VALUE";
const ERROR_MESSAGE: &str = "MARK-ERROR-MESSAGE";
const PROSE_CODE: &str = "denied MARK-CODE";
const PROBE_ERROR: &str = "MARK-PROBE-ERROR";
const EVIDENCE: &str = "MARK-EVIDENCE";
const AUTH_MESSAGE: &str = "MARK-AUTH-MESSAGE";
const AUTH_CODE: &str = "revoked MARK-AUTH-CODE";

fn journal_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "codemode-retention-{}-{}.db",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn declaration(retention: Option<&str>) -> ProviderDeclaration {
    let mut declaration = json!({
        "provider": "host.issues",
        "account": "bot",
        "contracts": {
            "label": {"repeat": "unsafe", "reconcile": "receipt"},
            "comment": {"repeat": "unsafe", "reconcile": "none"},
        },
    });
    if let Some(retention) = retention {
        declaration["retention"] = json!(retention);
    }
    serde_json::from_value(declaration).expect("declaration")
}

fn plan() -> codemode::FrozenPlan {
    let scope = json!({"provider": "host.issues", "account": "bot", "destination": "acme/widgets"});
    let operation = |id: &str, tool: &str, issue: u64| json!({"id": id, "tool": tool, "scope": scope, "arguments": {"issue": issue, "label": "triage"}});
    Plan::parse(
        &json!({"version": 1, "operations": [
            operation("label-1", "label", 1),
            operation("label-2", "label", 2),
            operation("comment-3", "comment", 3),
        ]})
        .to_string(),
    )
    .expect("valid plan")
}

fn policy() -> RecoveryPolicy {
    RecoveryPolicy {
        concurrency: 1,
        ..RecoveryPolicy::default()
    }
}

/// Approve the plan with `declaration`.
fn approve(declaration: &ProviderDeclaration) -> PathBuf {
    let plan = plan();
    let binding = approval_binding(plan.digest(), declaration, &policy());
    record_approval(&plan, declaration.clone(), binding)
}

/// A fresh journal for `plan` holding one approval with `binding`.
fn record_approval(
    plan: &codemode::FrozenPlan,
    declaration: ProviderDeclaration,
    binding: codemode::Digest,
) -> PathBuf {
    let path = journal_path();
    let mut journal = Journal::create(&path, plan, Durability::Process).expect("create");
    journal
        .approve(&Approval {
            binding,
            plan: plan.digest().clone(),
            declaration,
            policy: policy(),
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("approve");
    path
}

fn error(code: &str, message: &str) -> ProviderError {
    ProviderError {
        code: code.into(),
        message: message.into(),
    }
}

/// A provider that answers one session from a script, and counts every request.
#[derive(Clone)]
struct Scripted {
    declaration: ProviderDeclaration,
    authorization: Result<(), ProviderError>,
    calls: HashMap<&'static str, Result<Value, AttemptError>>,
    probes: HashMap<&'static str, Result<Reconciliation, ProviderError>>,
    requests: Arc<AtomicUsize>,
}

impl Scripted {
    fn new(declaration: &ProviderDeclaration) -> Self {
        Self {
            declaration: declaration.clone(),
            authorization: Ok(()),
            calls: HashMap::new(),
            probes: HashMap::new(),
            requests: Arc::default(),
        }
    }
}

impl RecoverableProvider for Scripted {
    fn declaration(&self) -> ProviderDeclaration {
        self.declaration.clone()
    }

    async fn authorize(&self, _scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.authorization.clone()
    }

    async fn execute(
        &self,
        attempt: Attempt,
        _context: CallContext,
    ) -> Result<Value, AttemptError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let id = attempt.operation.id.as_str();
        self.calls
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no scripted call for {id}"))
    }

    async fn reconcile(
        &self,
        operation: Operation,
        _keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let id = operation.id.as_str();
        self.probes
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no scripted probe for {id}"))
    }
}

async fn session(
    path: &Path,
    kind: SessionKind,
    provider: Scripted,
) -> Result<SessionOutcome, SessionError> {
    let journal = Journal::open(path, Durability::Process).expect("open");
    let options = SessionOptions {
        kind,
        // These sessions record every operation's outcome, failed ones included.
        max_failures: 0,
        ..SessionOptions::default()
    };
    run_session(journal, provider, options, RunCancellation::default()).await
}

/// Four sessions whose provider replies carry provider text in every place it
/// can: a result value, an error message, a prose error code, a probe error,
/// probe evidence, and an authorization error. Returns the journal, each
/// session's operations as (operation, conclusion, next), and everything the
/// sessions returned, as text.
async fn provider_text_scenario(
    declaration: &ProviderDeclaration,
) -> (PathBuf, Vec<Vec<(String, Conclusion, NextStep)>>, String) {
    let path = approve(declaration);
    let mut returned = String::new();
    let mut sessions = Vec::new();
    let mut record = |outcome: &SessionOutcome| {
        returned.push_str(&serde_json::to_string(outcome).expect("outcome serializes"));
        sessions.push(
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
                .collect::<Vec<_>>(),
        );
    };

    // 1. A success with a value, a failure with a message and unknown effect, and
    //    a refusal whose code is prose.
    let mut provider = Scripted::new(declaration);
    provider.calls = HashMap::from([
        ("label-1", Ok(json!({"note": VALUE}))),
        (
            "label-2",
            Err(AttemptError {
                error: error("tool_error", ERROR_MESSAGE),
                effect: FailureEffect::Unknown,
                rate_limit: None,
            }),
        ),
        (
            "comment-3",
            Err(AttemptError {
                error: error(PROSE_CODE, ERROR_MESSAGE),
                effect: FailureEffect::Refused,
                rate_limit: None,
            }),
        ),
    ]);
    record(
        &session(&path, SessionKind::Run, provider)
            .await
            .expect("session 1 runs"),
    );

    // 2. The probe of label-2 fails with a message.
    let mut provider = Scripted::new(declaration);
    provider.probes = HashMap::from([("label-2", Err(error("probe_failed", PROBE_ERROR)))]);
    record(
        &session(&path, SessionKind::Resume, provider)
            .await
            .expect("session 2 runs"),
    );

    // 3. The next probe finds a receipt, with evidence.
    let mut provider = Scripted::new(declaration);
    provider.probes = HashMap::from([(
        "label-2",
        Ok(Reconciliation {
            verdict: Verdict::Applied,
            evidence: EVIDENCE.into(),
        }),
    )]);
    record(
        &session(&path, SessionKind::Resume, provider)
            .await
            .expect("session 3 runs"),
    );

    // 4. Authorization is refused with a prose code and a message.
    let mut provider = Scripted::new(declaration);
    provider.authorization = Err(error(AUTH_CODE, AUTH_MESSAGE));
    let refused = session(&path, SessionKind::Resume, provider)
        .await
        .expect_err("session 4 is refused");
    let SessionError::Refused { reason, .. } = &refused else {
        panic!("session 4 is refused: {refused}");
    };
    assert!(reason.ends_with("(unrecognized_code)"), "{reason}");
    returned.push_str(&refused.to_string());
    (path, sessions, returned)
}

/// Everything durable or printed about the journal: the database and its
/// write-ahead log as raw bytes, the reopened history, every stored trace, and
/// inspection with messages (JSON and text).
fn everything_kept(path: &Path) -> String {
    let journal = JournalReader::open(path).expect("read");
    let mut kept = String::new();
    for file in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
    ] {
        if let Ok(bytes) = std::fs::read(&file) {
            kept.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    let history = journal.history().expect("history");
    kept.push_str(&format!("{history:?}"));
    for session in &history.sessions {
        if let Some(trace) = journal.trace(session.session).expect("trace reads") {
            kept.push_str(&serde_json::to_string(&trace).expect("trace serializes"));
        }
    }
    let inspection = codemode::inspect_journal(
        &journal,
        &codemode::JournalInspectOptions { messages: true },
    )
    .expect("inspects");
    kept.push_str(&serde_json::to_string(&inspection).expect("inspection serializes"));
    kept.push_str(&inspection.render());
    kept
}

fn rendered(path: &Path) -> String {
    codemode::inspect_journal(
        &JournalReader::open(path).expect("read"),
        &Default::default(),
    )
    .expect("inspects")
    .render()
}

#[tokio::test]
async fn the_default_retention_keeps_no_provider_text() {
    let (path, sessions, returned) = provider_text_scenario(&declaration(None)).await;
    let kept = everything_kept(&path) + returned.as_str();
    assert!(!kept.contains("MARK-"), "provider text was kept: {kept}");
    // The facts recovery needs are still there.
    let history = JournalReader::open(&path)
        .expect("read")
        .history()
        .expect("history");
    let observed: Vec<&Observed> = history
        .attempts
        .iter()
        .filter_map(|attempt| attempt.observation.as_ref().map(|o| &o.observed))
        .collect();
    assert!(matches!(observed[0], Observed::Succeeded { bytes } if *bytes > 0));
    assert!(matches!(
        observed[1],
        Observed::Failed {
            effect: FailureEffect::Unknown,
            ..
        }
    ));
    assert!(matches!(
        observed[2],
        Observed::Failed { error, effect: FailureEffect::Refused }
            if error.code == codemode::UNRECOGNIZED_CODE && error.message.is_empty()
    ));
    let probe = history
        .probes
        .last()
        .and_then(|probe| probe.outcome.as_ref().ok())
        .expect("the last probe was answered");
    assert_eq!(
        (probe.verdict, probe.evidence.as_str()),
        (Verdict::Applied, ""),
        "the default retention policy keeps the verdict, not the provider's evidence"
    );
    let render = rendered(&path);
    assert!(render.contains("retention metadata/1"), "{render}");
    assert!(
        render.contains("applied - evidence not retained"),
        "{render}"
    );
    assert_eq!(
        sessions.last().expect("sessions")[1],
        ("label-2".into(), Conclusion::Applied, NextStep::Done)
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_summary_policy_keeps_the_providers_explanations_and_the_same_decisions() {
    let (metadata_path, metadata, _) = provider_text_scenario(&declaration(None)).await;
    let (path, summary, returned) = provider_text_scenario(&declaration(Some("summary/1"))).await;
    assert_eq!(
        metadata, summary,
        "retention never changes a recovery decision"
    );

    let kept = everything_kept(&path) + returned.as_str();
    assert!(!kept.contains(VALUE), "result values are never kept");
    assert!(
        !kept.contains("MARK-CODE") && !kept.contains("MARK-AUTH-CODE"),
        "codes stay codes"
    );
    let render = rendered(&path);
    assert!(render.contains("retention summary/1"), "{render}");
    assert!(
        render.contains(&format!("applied - {EVIDENCE}")),
        "{render}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&metadata_path);
}

#[tokio::test]
async fn a_changed_retention_policy_needs_a_new_approval() {
    // Approved under metadata/1; the provider now declares summary/1.
    let path = approve(&declaration(None));
    let provider = Scripted::new(&declaration(Some("summary/1")));
    let refused = session(&path, SessionKind::Run, provider.clone())
        .await
        .expect_err("refused");
    assert!(
        matches!(&refused, SessionError::Refused { reason, .. }
            if reason.contains("retention policy changed")),
        "{refused}"
    );
    assert_eq!(provider.requests.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn an_approval_bound_under_the_previous_format_is_refused_before_any_provider_request() {
    let plan = plan();
    let declaration = declaration(None);
    let mut previous = json!({"format": "codemode-approval/2", "plan": plan.digest(),
                              "declaration": declaration, "policy": policy()});
    previous.sort_all_objects();
    let binding = codemode::Digest::of(previous.to_string().as_bytes());
    let path = record_approval(&plan, declaration.clone(), binding);

    let provider = Scripted::new(&declaration);
    let refused = session(&path, SessionKind::Run, provider.clone())
        .await
        .expect_err("refused");
    assert!(
        matches!(&refused, SessionError::Refused { reason, .. }
            if reason.contains("does not bind this plan")),
        "{refused}"
    );
    // No authorization, call, or probe was asked of the provider.
    assert_eq!(provider.requests.load(Ordering::SeqCst), 0);
    let journal = JournalReader::open(&path).expect("read");
    assert!(
        journal.history().expect("history").attempts.is_empty(),
        "nothing ran"
    );
    drop(journal);
    let _ = std::fs::remove_file(&path);
}
