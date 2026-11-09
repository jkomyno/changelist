//! Plan-mode bridge (protocol version 3) with an in-process fake host and no
//! JavaScript engine.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use codemode::{
    Approval, Conclusion, Durability, FailureEffect, Journal, JournalError, NextStep, Observed,
    Plan, PlanBridgeError, ProviderDeclaration, RecoveryPolicy, RunCancellation, SessionKind,
    SessionOptions, SessionOutcome, Verdict, approval_binding, serve_plan_bridge,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::task::JoinHandle;

static SCRATCH: AtomicU64 = AtomicU64::new(0);
/// Frames the retention tests read from the bridge.
static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn journal_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "codemode-plan-bridge-{}-{}.db",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn declaration() -> Value {
    json!({
        "provider": "host.issues",
        "account": "bot",
        "contracts": {
            "label": {"repeat": "safe", "reconcile": "receipt"},
            "comment": {"repeat": "unsafe", "reconcile": "none"},
        },
    })
}

fn label_arguments() -> Value {
    json!({"issue": 7, "label": "triage"})
}

fn comment_arguments() -> Value {
    json!({"issue": 7, "body": "reproduced"})
}

/// Two operations, dispatched one at a time in plan order.
fn approved_journal() -> PathBuf {
    let scope = json!({"provider": "host.issues", "account": "bot", "destination": "acme/widgets"});
    let plan = Plan::parse(
        &json!({"version": 1, "operations": [
            {"id": "label-7", "tool": "label", "scope": scope, "arguments": label_arguments()},
            {"id": "comment-7", "tool": "comment", "scope": scope, "arguments": comment_arguments(),
             "preconditions": [{"issue_state": "open"}]},
        ]})
        .to_string(),
    )
    .expect("valid plan");
    let path = journal_path();
    let mut journal = Journal::create(&path, &plan, Durability::Process).expect("create");
    let declaration: ProviderDeclaration =
        serde_json::from_value(declaration()).expect("declaration");
    let policy = RecoveryPolicy {
        concurrency: 1,
        ..RecoveryPolicy::default()
    };
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
    path
}

/// Open a journal once the bridge that owned it has released its lock.
async fn reopen(path: &Path) -> Journal {
    for _ in 0..100 {
        match Journal::open(path, Durability::Process) {
            Ok(journal) => return journal,
            Err(JournalError::Busy(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => panic!("journal opens: {error}"),
        }
    }
    panic!("the bridge never released the journal");
}

#[tokio::test]
async fn stalled_plan_output_is_bounded_and_the_session_remains_inspectable() {
    let path = approved_journal();
    let journal = Journal::open(&path, Durability::Process).expect("open");
    let (mut input, bridge_input) = tokio::io::duplex(4096);
    let (bridge_output, mut unread_output) = tokio::io::duplex(16);
    let cancellation = RunCancellation::default();
    let bridge = tokio::spawn(serve_plan_bridge(
        bridge_input,
        bridge_output,
        journal,
        SessionOptions::default(),
        cancellation.clone(),
    ));
    let start = json!({"type":"start","version":3,"declaration":declaration()});
    input
        .write_all(format!("{start}\n").as_bytes())
        .await
        .expect("start");
    tokio::time::timeout(Duration::from_secs(2), unread_output.read_u8())
        .await
        .expect("authorization began")
        .expect("first output byte");
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(3), bridge)
        .await
        .expect("bounded shutdown")
        .expect("bridge joined")
        .expect_err("output stalled");
    assert!(
        matches!(error, PlanBridgeError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut),
        "{error}"
    );
    let journal = reopen(&path).await;
    let history = journal.history().expect("history");
    assert!(history.attempts.is_empty(), "no mutation was sent");
    assert!(
        history
            .sessions
            .last()
            .expect("session recorded")
            .end
            .is_some()
    );
    drop(journal);
    std::fs::remove_file(path).expect("remove test journal");
}

/// The parent-host end of a plan bridge.
struct Host {
    frames: Lines<BufReader<DuplexStream>>,
    input: DuplexStream,
    bridge: JoinHandle<Result<SessionOutcome, PlanBridgeError>>,
}

impl Host {
    async fn start(path: &Path, kind: SessionKind) -> Self {
        Self::start_as(path, kind, declaration()).await
    }

    /// Start a session whose host declares `declaration`.
    async fn start_as(path: &Path, kind: SessionKind, declaration: Value) -> Self {
        let journal = Journal::open(path, Durability::Process).expect("open");
        let (input, bridge_input) = tokio::io::duplex(1 << 20);
        let (bridge_output, output) = tokio::io::duplex(1 << 20);
        let options = SessionOptions { kind };
        let bridge = tokio::spawn(serve_plan_bridge(
            bridge_input,
            bridge_output,
            journal,
            options,
            RunCancellation::default(),
        ));
        let mut host = Self {
            frames: BufReader::new(output).lines(),
            input,
            bridge,
        };
        host.send(json!({"type": "start", "version": 3, "declaration": declaration}))
            .await;
        host
    }

    async fn send(&mut self, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).expect("frame serializes");
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .expect("bridge input is open");
    }

    /// The next frame, also kept for checks of everything the bridge wrote.
    async fn next_kept(&mut self) -> Value {
        let frame = self.next().await;
        SEEN.lock().expect("seen").push(frame.to_string());
        frame
    }

    async fn next(&mut self) -> Value {
        let line = self
            .frames
            .next_line()
            .await
            .expect("bridge output is readable")
            .expect("bridge wrote a frame");
        serde_json::from_str(&line).expect("bridge frames are JSON")
    }

    /// Expect one `authorize` frame for the plan's scope and grant it.
    async fn authorize(&mut self) {
        let frame = self.next_kept().await;
        assert_eq!(frame["type"], "authorize", "{frame}");
        assert_eq!(
            frame["scope"],
            json!({"provider": "host.issues", "account": "bot", "destination": "acme/widgets"})
        );
        self.send(json!({"type": "authorized", "id": frame["id"], "ok": true}))
            .await;
    }
}

#[tokio::test]
async fn host_executes_journaled_attempts_and_answers_probes() {
    let path = approved_journal();

    // Session 1: the host receives the first attempt, then crashes without replying.
    let mut host = Host::start(&path, SessionKind::Run).await;
    host.authorize().await;
    let call = host.next().await;
    assert_eq!(call["type"], "call", "{call}");
    assert_eq!(call["tool"], "label");
    assert_eq!(call["operation"], "label-7");
    assert_eq!(
        call["arguments"],
        label_arguments(),
        "the approved frozen arguments"
    );
    assert_eq!(call["preconditions"], json!([]));
    let key = call["attempt"].as_str().expect("attempt key").to_string();
    host.bridge.abort();
    drop(host);

    // Nothing ran after the host saw the call, so its intent was already durable.
    let history = reopen(&path).await.history().expect("history");
    assert_eq!(history.attempts.len(), 1, "{:?}", history.attempts);
    let attempt = &history.attempts[0];
    assert_eq!(
        (
            attempt.key.as_str(),
            attempt.operation.as_str(),
            attempt.number
        ),
        (key.as_str(), "label-7", 1)
    );
    assert!(attempt.observation.is_none(), "{attempt:?}");

    // Session 2: the host's probe settles label-7; its comment attempt fails without
    // stating an effect.
    let mut host = Host::start(&path, SessionKind::Resume).await;
    host.authorize().await;
    let probe = host.next().await;
    assert_eq!(probe["type"], "reconcile", "{probe}");
    assert_eq!(probe["operation"], "label-7");
    assert_eq!(probe["tool"], "label");
    assert_eq!(probe["arguments"], label_arguments());
    assert_eq!(probe["keys"], json!([key]));
    host.send(
        json!({"type": "reconciled", "id": probe["id"], "verdict": "applied",
                     "evidence": "receipt found"}),
    )
    .await;
    let call = host.next().await;
    assert_eq!(call["type"], "call", "{call}");
    assert_eq!(call["operation"], "comment-7");
    assert_eq!(call["arguments"], comment_arguments());
    assert_eq!(call["preconditions"], json!([{"issue_state": "open"}]));
    host.send(json!({"type": "result", "id": call["id"], "ok": false,
                     "error": {"code": "tool_error", "message": "blocked by a host gate"}}))
        .await;
    let frame = host.next().await;
    assert_eq!(frame["type"], "outcome", "{frame}");
    assert!(frame.get("diagnostic").is_none(), "{frame}");
    let outcome = host
        .bridge
        .await
        .expect("bridge task completes")
        .expect("session runs");
    assert_eq!(outcome.status, "needs_review");
    assert_eq!(frame["outcome"]["status"], "needs_review");
    assert_eq!(outcome.probed, 1);

    let label = &outcome.operations[0].assessment;
    assert_eq!(
        (label.conclusion, label.next),
        (Conclusion::Applied, NextStep::Done)
    );
    let comment = &outcome.operations[1].assessment;
    assert_eq!(
        (comment.conclusion, comment.next),
        (Conclusion::Unknown, NextStep::Review),
        "a host error without an effect is not a refusal"
    );

    let history = reopen(&path).await.history().expect("history");
    let probe = &history.probes[0];
    let reconciliation = probe.outcome.as_ref().expect("probe answered");
    assert_eq!(
        (
            probe.operation.as_str(),
            reconciliation.verdict,
            reconciliation.evidence.as_str()
        ),
        ("label-7", Verdict::Applied, ""),
        "the default retention policy keeps the verdict, not the host's evidence"
    );
    let failed = history
        .attempts
        .iter()
        .find(|attempt| attempt.operation == "comment-7")
        .and_then(|attempt| attempt.observation.as_ref())
        .expect("comment attempt observed");
    assert!(
        matches!(
            failed.observed,
            Observed::Failed {
                effect: FailureEffect::Unknown,
                ..
            }
        ),
        "{failed:?}"
    );
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Retention: what the journal keeps of the host's replies.

/// Distinct synthetic markers for each kind of provider text.
const VALUE: &str = "MARK-VALUE";
const ERROR_MESSAGE: &str = "MARK-ERROR-MESSAGE";
const PROSE_CODE: &str = "denied MARK-CODE";
const PROBE_ERROR: &str = "MARK-PROBE-ERROR";
const EVIDENCE: &str = "MARK-EVIDENCE";
const AUTH_MESSAGE: &str = "MARK-AUTH-MESSAGE";
const AUTH_CODE: &str = "revoked MARK-AUTH-CODE";

fn retention_declaration(retention: Option<&str>) -> Value {
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
    declaration
}

fn retention_plan() -> codemode::FrozenPlan {
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

/// Approve the retention plan with `declaration`.
fn approve_retention(declaration: &Value) -> PathBuf {
    let plan = retention_plan();
    let parsed: ProviderDeclaration =
        serde_json::from_value(declaration.clone()).expect("declaration");
    let policy = RecoveryPolicy {
        concurrency: 1,
        ..RecoveryPolicy::default()
    };
    let binding = approval_binding(plan.digest(), &parsed, &policy);
    record_approval(&plan, parsed, policy, binding)
}

/// A fresh journal for `plan` holding one approval with `binding`.
fn record_approval(
    plan: &codemode::FrozenPlan,
    declaration: ProviderDeclaration,
    policy: RecoveryPolicy,
    binding: codemode::Digest,
) -> PathBuf {
    let path = journal_path();
    let mut journal = Journal::create(&path, plan, Durability::Process).expect("create");
    journal
        .approve(&Approval {
            binding,
            plan: plan.digest().clone(),
            declaration,
            policy,
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("approve");
    path
}

/// Expect a `call` for `operation` and answer it with `reply`.
async fn answer_call(host: &mut Host, operation: &str, reply: Value) {
    let call = host.next_kept().await;
    assert_eq!(
        (call["type"].as_str(), call["operation"].as_str()),
        (Some("call"), Some(operation)),
        "{call}"
    );
    let mut frame = json!({"type": "result", "id": call["id"]});
    frame
        .as_object_mut()
        .expect("object")
        .extend(reply.as_object().expect("object").clone());
    host.send(frame).await;
}

/// Expect a `reconcile` for `operation` and answer it with `reply`.
async fn answer_probe(host: &mut Host, operation: &str, reply: Value) {
    let probe = host.next_kept().await;
    assert_eq!(
        (probe["type"].as_str(), probe["operation"].as_str()),
        (Some("reconcile"), Some(operation)),
        "{probe}"
    );
    let mut frame = json!({"type": "reconciled", "id": probe["id"]});
    frame
        .as_object_mut()
        .expect("object")
        .extend(reply.as_object().expect("object").clone());
    host.send(frame).await;
}

/// The session's outcome frame and its result.
async fn finish(mut host: Host) -> (Value, Result<SessionOutcome, PlanBridgeError>) {
    let frame = host.next_kept().await;
    assert_eq!(frame["type"], "outcome", "{frame}");
    let result = host.bridge.await.expect("bridge task completes");
    (frame, result)
}

/// Four sessions whose host replies carry provider text in every place it can:
/// a result value, an error message, a prose error code, a probe error, probe
/// evidence, and an authorization error. Returns the journal and each session's
/// operations as (operation, conclusion, next).
async fn provider_text_scenario(
    declaration: &Value,
) -> (PathBuf, Vec<Vec<(String, Conclusion, NextStep)>>) {
    let path = approve_retention(declaration);
    let statuses = |outcome: &SessionOutcome| {
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
            .collect::<Vec<_>>()
    };
    let mut sessions = Vec::new();

    // 1. A success with a value, a failure with a message and unknown effect, and
    //    a refusal whose code is prose.
    let mut host = Host::start_as(&path, SessionKind::Run, declaration.clone()).await;
    host.authorize().await;
    answer_call(
        &mut host,
        "label-1",
        json!({"ok": true, "value": {"note": VALUE}}),
    )
    .await;
    answer_call(
        &mut host,
        "label-2",
        json!({"ok": false, "error": {"code": "tool_error", "message": ERROR_MESSAGE}}),
    )
    .await;
    answer_call(
        &mut host,
        "comment-3",
        json!({"ok": false, "error": {"code": PROSE_CODE, "message": ERROR_MESSAGE},
               "effect": "refused"}),
    )
    .await;
    let (_, outcome) = finish(host).await;
    sessions.push(statuses(&outcome.expect("session 1 runs")));

    // 2. The probe of label-2 fails with a message.
    let mut host = Host::start_as(&path, SessionKind::Resume, declaration.clone()).await;
    host.authorize().await;
    answer_probe(
        &mut host,
        "label-2",
        json!({"error": {"code": "probe_failed", "message": PROBE_ERROR}}),
    )
    .await;
    let (_, outcome) = finish(host).await;
    sessions.push(statuses(&outcome.expect("session 2 runs")));

    // 3. The next probe finds a receipt, with evidence.
    let mut host = Host::start_as(&path, SessionKind::Resume, declaration.clone()).await;
    host.authorize().await;
    answer_probe(
        &mut host,
        "label-2",
        json!({"verdict": "applied", "evidence": EVIDENCE}),
    )
    .await;
    let (_, outcome) = finish(host).await;
    sessions.push(statuses(&outcome.expect("session 3 runs")));

    // 4. Authorization is refused with a prose code and a message.
    let mut host = Host::start_as(&path, SessionKind::Resume, declaration.clone()).await;
    let frame = host.next_kept().await;
    assert_eq!(frame["type"], "authorize", "{frame}");
    host.send(json!({"type": "authorized", "id": frame["id"], "ok": false,
                     "error": {"code": AUTH_CODE, "message": AUTH_MESSAGE}}))
        .await;
    let (frame, outcome) = finish(host).await;
    assert_eq!(frame["outcome"]["status"], "refused", "{frame}");
    assert!(
        frame["outcome"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.ends_with("(unrecognized_code)")),
        "{frame}"
    );
    assert!(matches!(
        outcome,
        Err(PlanBridgeError::Session(
            codemode::SessionError::Refused { .. }
        ))
    ));
    (path, sessions)
}

/// Everything durable or printed about the journal: the database and its
/// write-ahead log as raw bytes, the reopened history, every stored trace,
/// inspection with messages (JSON and text), and every bridge frame read.
async fn everything_kept(path: &Path) -> String {
    let journal = reopen(path).await;
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
    kept.push_str(&SEEN.lock().expect("seen").join("\n"));
    kept
}

#[tokio::test]
async fn the_default_retention_keeps_no_provider_text() {
    let (path, sessions) = provider_text_scenario(&retention_declaration(None)).await;
    let kept = everything_kept(&path).await;
    assert!(!kept.contains("MARK-"), "provider text was kept: {kept}");
    // The facts recovery needs are still there.
    let history = reopen(&path).await.history().expect("history");
    let observed: Vec<&Observed> = history
        .attempts
        .iter()
        .filter_map(|attempt| attempt.observation.as_ref().map(|o| &o.observed))
        .collect();
    assert!(matches!(observed[0], Observed::Succeeded { bytes } if *bytes > 0));
    assert!(matches!(
        observed[2],
        Observed::Failed { error, effect: FailureEffect::Refused }
            if error.code == codemode::UNRECOGNIZED_CODE && error.message.is_empty()
    ));
    let render = codemode::inspect_journal(&reopen(&path).await, &Default::default())
        .expect("inspects")
        .render();
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
async fn a_summary_policy_keeps_the_hosts_explanations_and_the_same_decisions() {
    let (metadata_path, metadata) = provider_text_scenario(&retention_declaration(None)).await;
    let (path, summary) = provider_text_scenario(&retention_declaration(Some("summary/1"))).await;
    assert_eq!(
        metadata, summary,
        "retention never changes a recovery decision"
    );

    let kept = everything_kept(&path).await;
    assert!(!kept.contains(VALUE), "result values are never kept");
    assert!(
        !kept.contains("MARK-CODE") && !kept.contains("MARK-AUTH-CODE"),
        "codes stay codes"
    );
    let render = codemode::inspect_journal(&reopen(&path).await, &Default::default())
        .expect("inspects")
        .render();
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
    // Approved under metadata/1; the host now declares summary/1.
    let path = approve_retention(&retention_declaration(None));
    let host = Host::start_as(
        &path,
        SessionKind::Run,
        retention_declaration(Some("summary/1")),
    )
    .await;
    let (frame, _) = finish(host).await;
    assert_eq!(frame["outcome"]["status"], "refused", "{frame}");
    assert!(
        frame["outcome"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("retention policy changed")),
        "{frame}"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn an_approval_bound_under_the_previous_format_is_refused_before_any_host_request() {
    let plan = retention_plan();
    let declaration = retention_declaration(None);
    let parsed: ProviderDeclaration =
        serde_json::from_value(declaration.clone()).expect("declaration");
    let policy = RecoveryPolicy {
        concurrency: 1,
        ..RecoveryPolicy::default()
    };
    let mut previous = json!({"format": "codemode-approval/2", "plan": plan.digest(),
                              "declaration": parsed, "policy": policy});
    previous.sort_all_objects();
    let binding = codemode::Digest::of(previous.to_string().as_bytes());
    let path = record_approval(&plan, parsed, policy, binding);

    let host = Host::start_as(&path, SessionKind::Run, declaration).await;
    // The first frame the host receives is the outcome: no authorize, call, or reconcile.
    let (frame, _) = finish(host).await;
    assert_eq!(frame["outcome"]["status"], "refused", "{frame}");
    assert!(
        frame["outcome"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("does not bind this plan")),
        "{frame}"
    );
    let journal = reopen(&path).await;
    assert!(
        journal.history().expect("history").attempts.is_empty(),
        "nothing ran"
    );
    drop(journal);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_fact_frame_is_a_protocol_error_and_nothing_is_dispatched() {
    let path = approved_journal();
    let mut host = Host::start(&path, SessionKind::Run).await;
    let authorize = host.next().await;
    assert_eq!(authorize["type"], "authorize", "{authorize}");
    host.send(json!({"type": "fact", "id": authorize["id"], "kind": "received"}))
        .await;
    let (frame, _) = tokio::time::timeout(Duration::from_secs(5), finish(host))
        .await
        .expect("the fact frame ends the session");
    assert!(
        frame["diagnostic"]
            .as_str()
            .is_some_and(|diagnostic| diagnostic.starts_with("invalid host frame")),
        "{frame}"
    );
    let journal = reopen(&path).await;
    assert!(
        journal.history().expect("history").attempts.is_empty(),
        "nothing was dispatched"
    );
    drop(journal);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_malformed_host_reply_is_described_without_its_values() {
    let path = approve_retention(&retention_declaration(None));
    let mut host = Host::start_as(&path, SessionKind::Run, retention_declaration(None)).await;
    host.authorize().await;
    let call = host.next_kept().await;
    assert_eq!(call["type"], "call", "{call}");
    // A verdict where a result belongs: serde would quote the unknown variant.
    host.send(
        json!({"type": "reconciled", "id": call["id"], "verdict": "MARK-SECRET-VERDICT",
                     "evidence": "x"}),
    )
    .await;
    let (frame, _) = finish(host).await;
    let diagnostic = frame["diagnostic"].as_str().expect("diagnostic");
    assert!(
        diagnostic == "invalid host frame: data error",
        "{diagnostic}"
    );
    assert!(!frame.to_string().contains("MARK-"), "{frame}");
    let _ = std::fs::remove_file(&path);
}
