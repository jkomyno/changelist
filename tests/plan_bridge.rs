//! Plan-mode bridge (protocol version 3) with an in-process fake host. Real guest
//! programs run, so Deno must be on the PATH (or named by CODEMODE_TEST_DENO).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use codemode::{
    Approval, Conclusion, Durability, FailureEffect, Journal, JournalError, NextStep, Observed,
    Plan, PlanBridgeError, ProviderDeclaration, RecoveryPolicy, RunCancellation, SessionKind,
    SessionOptions, SessionOutcome, Verdict, approval_binding, serve_plan_bridge,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::task::JoinHandle;

static SCRATCH: AtomicU64 = AtomicU64::new(0);

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

/// The parent-host end of a plan bridge.
struct Host {
    frames: Lines<BufReader<DuplexStream>>,
    input: DuplexStream,
    bridge: JoinHandle<Result<SessionOutcome, PlanBridgeError>>,
}

impl Host {
    async fn start(path: &Path, kind: SessionKind) -> Self {
        let journal = Journal::open(path, Durability::Process).expect("open");
        let (input, bridge_input) = tokio::io::duplex(1 << 20);
        let (bridge_output, output) = tokio::io::duplex(1 << 20);
        let options = SessionOptions {
            kind,
            deno: std::env::var_os("CODEMODE_TEST_DENO").map_or_else(|| "deno".into(), Into::into),
        };
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
        host.send(json!({"type": "start", "version": 3, "declaration": declaration()}))
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
        let frame = self.next().await;
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
        "the frozen arguments, not the guest's"
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
        ("label-7", Verdict::Applied, "receipt found")
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
