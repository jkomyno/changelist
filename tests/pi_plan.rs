//! An approved plan run and recovered through a real Pi process, with the offline
//! fixture as the provider. Needs Pi 1.4.0; ignored by default, run it with
//! `mise run pi-check`.
#![cfg(unix)]

use std::collections::HashSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use codemode::fixture::{Fixture, Ledger};
use codemode::{
    Basis, Conclusion, Durability, History, Journal, JournalError, NextStep, OperationStatus,
    Verdict, assess_history,
};
use serde_json::{Value, json};

const SECRET: &str = "codemode-pi-plan-secret-value";
const ACCOUNT: &str = "triage-bot";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("codemode-pi-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create a fresh scratch directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.file("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit lines are JSON"))
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Run a codemode binary and require success.
fn run(binary: &str, args: &[&str]) -> Output {
    let output = Command::new(binary)
        .args(args)
        .output()
        .expect("binary starts");
    assert!(output.status.success(), "{binary} {args:?}: {output:?}");
    output
}

fn json_output(binary: &str, args: &[&str]) -> Value {
    serde_json::from_slice(&run(binary, args).stdout).expect("JSON output")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// The fixture's own declaration, with Pi's tool names.
fn declaration(fixture: &Path) -> Value {
    let mut declaration = json_output(
        env!("CARGO_BIN_EXE_codemode-fixture"),
        &["declare", "--db", path(fixture), "--account", ACCOUNT],
    );
    let contracts = declaration["contracts"]
        .as_object()
        .expect("contracts")
        .iter()
        .map(|(tool, contract)| (tool.replace('.', "_"), contract.clone()))
        .collect();
    declaration["contracts"] = Value::Object(contracts);
    declaration
}

fn plan() -> Value {
    let scope =
        json!({"provider": "fixture.issues", "account": ACCOUNT, "destination": "acme/widgets"});
    let open = json!([{"issue_state": "open"}]);
    json!({"version": 1, "summary": "Triage five issues through Pi", "operations": [
        {"id": "label-101", "tool": "issues_add_label", "scope": scope,
         "arguments": {"issue": 101, "label": "triage"}},
        {"id": "comment-102", "tool": "issues_add_comment", "scope": scope,
         "arguments": {"issue": 102, "body": "Reproduced on main."}, "preconditions": open},
        {"id": "label-103", "tool": "issues_add_label", "scope": scope,
         "arguments": {"issue": 103, "label": "triage"}},
        {"id": "unlabel-104", "tool": "issues_remove_label", "scope": scope,
         "arguments": {"issue": 104, "label": "needs-info"}, "preconditions": open},
        {"id": "label-105", "tool": "issues_add_label", "scope": scope,
         "arguments": {"issue": 105, "label": "triage"}},
    ]})
}

/// Pi with the plan tool, the fixture tools, a scripted model, and the approval gate.
fn pi(scratch: &Scratch, action: &str) -> Command {
    let mut command = Command::new(std::env::var_os("CODEMODE_PI").unwrap_or_else(|| "pi".into()));
    command
        .current_dir(root())
        .env("PI_CODING_AGENT_DIR", scratch.file("agent"))
        .env("CODEMODE_PI_AUDIT", scratch.file("audit.jsonl"))
        .env("CODEMODE_PI_PLAN_JOURNAL", scratch.file("plan.journal"))
        .env("CODEMODE_PI_PLAN_ACTION", action)
        .env(
            "CODEMODE_FIXTURE_BIN",
            env!("CARGO_BIN_EXE_codemode-fixture"),
        )
        .env("CODEMODE_FIXTURE_DB", scratch.file("fixture.db"))
        .env("CODEMODE_FIXTURE_ACCOUNT", ACCOUNT)
        // Present in Pi's environment; it must never reach the journal or outcome.
        .env("CODEMODE_DEMO_SECRET", SECRET)
        .args([
            "--print",
            "--offline",
            "--no-session",
            "--no-extensions",
            "--no-context-files",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--extension",
            "integrations/pi/codemode-bridge.ts",
            "--extension",
            "integrations/pi/demo/plan-host.ts",
            "--extension",
            "integrations/pi/demo/fixture-tools.ts",
            "--extension",
            "integrations/pi/demo/approval-gate.ts",
            "--model",
            "codemode-plan-demo/scripted",
            "--tools",
            "rust_codemode_plan,issues_add_label,issues_remove_label,issues_add_comment",
            "--codemode-bin",
            env!("CARGO_BIN_EXE_codemode"),
            "--codemode-plan-declaration",
        ])
        .arg(scratch.file("declaration.json"))
        .stdin(Stdio::null());
    if let Some(deno) = std::env::var_os("CODEMODE_TEST_DENO") {
        command.arg("--codemode-deno").arg(deno);
    }
    command.arg(format!(
        "Use rust_codemode_plan to {action} the approved plan."
    ));
    command
}

fn ledger(scratch: &Scratch) -> Ledger {
    Fixture::open(scratch.file("fixture.db"))
        .expect("fixture opens")
        .ledger()
        .expect("ledger reads")
}

/// The journal's history and every operation's assessment, once no process owns it.
fn journal(scratch: &Scratch) -> (History, Vec<OperationStatus>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let journal = loop {
        match Journal::open(scratch.file("plan.journal"), Durability::Process) {
            Ok(journal) => break journal,
            Err(JournalError::Busy(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("journal opens: {error}"),
        }
    };
    let history = journal.history().expect("history");
    let statuses = assess_history(journal.plan(), &history, &HashSet::new());
    (history, statuses)
}

fn status<'a>(statuses: &'a [OperationStatus], operation: &str) -> &'a OperationStatus {
    statuses
        .iter()
        .find(|status| status.operation == operation)
        .expect("operation is in the plan")
}

#[test]
#[ignore = "requires Pi 1.4.0; run `mise run pi-check`"]
fn pi_runs_and_recovers_an_approved_plan_through_its_own_gates() {
    let scratch = Scratch::new();
    let fixture = scratch.file("fixture.db");
    let codemode = env!("CARGO_BIN_EXE_codemode");
    let fixture_bin = env!("CARGO_BIN_EXE_codemode-fixture");
    run(
        fixture_bin,
        &[
            "init",
            "--db",
            path(&fixture),
            "--repo",
            "acme/widgets",
            "--account",
            ACCOUNT,
            "--issues",
            "101-105",
            "--label",
            "104:needs-info",
        ],
    );
    fs::write(
        scratch.file("declaration.json"),
        declaration(&fixture).to_string(),
    )
    .expect("write declaration");
    fs::write(scratch.file("plan.json"), plan().to_string()).expect("write plan");

    // 1. Approve: one operation at a time, so the lost response holds back the rest.
    let checked = json_output(
        codemode,
        &[
            "plan",
            "check",
            path(&scratch.file("plan.json")),
            "--provider",
            path(&scratch.file("declaration.json")),
            "--json",
        ],
    );
    let digest = checked["digest"].as_str().expect("digest");
    run(
        codemode,
        &[
            "plan",
            "approve",
            path(&scratch.file("plan.json")),
            "--journal",
            path(&scratch.file("plan.journal")),
            "--provider",
            path(&scratch.file("declaration.json")),
            "--digest",
            digest,
            "--approver",
            "tester",
            "--concurrency",
            "1",
            "--attempt-timeout-ms",
            "60000",
            "--session-timeout-ms",
            "120000",
        ],
    );
    run(
        fixture_bin,
        &[
            "fault",
            "--db",
            path(&fixture),
            "--tool",
            "issues.add_label",
            "--issue",
            "103",
            "--mode",
            "lose-response",
        ],
    );

    // 2-3. Run through Pi; once label-103's effect is in the ledger, kill Pi's whole
    // process group (Pi, the bridge, the guest, and the hung fixture call).
    let mut session = pi(&scratch, "run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("Pi starts");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let applied = ledger(&scratch)
            .rows
            .iter()
            .any(|row| row.tool == "issues.add_label" && row.issue == Some(103) && row.effect);
        if applied {
            break;
        }
        assert!(
            session.try_wait().expect("Pi status").is_none(),
            "Pi exited before the lost response"
        );
        assert!(
            Instant::now() < deadline,
            "label-103 never reached the fixture"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", session.id())])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    session.wait().expect("Pi is reaped");

    let (history, statuses) = journal(&scratch);
    assert_eq!(history.sessions.len(), 1);
    assert!(history.sessions[0].end.is_none(), "the session was killed");
    let lost: Vec<_> = history
        .attempts
        .iter()
        .filter(|attempt| attempt.operation == "label-103")
        .collect();
    assert_eq!(lost.len(), 1, "intent recorded before dispatch");
    assert!(lost[0].observation.is_none());
    let label = &status(&statuses, "label-103").assessment;
    assert_eq!(
        (label.conclusion, label.next),
        (Conclusion::Unknown, NextStep::Reconcile)
    );
    let applied = &status(&statuses, "label-101").assessment;
    assert_eq!(applied.conclusion, Conclusion::Applied);
    let gated = &status(&statuses, "comment-102").assessment;
    assert_eq!(
        (gated.conclusion, gated.next),
        (Conclusion::Unknown, NextStep::Review),
        "Pi's gate block is a host error, not proof of no effect"
    );
    for pending in ["unlabel-104", "label-105"] {
        assert_eq!(
            status(&statuses, pending).assessment.conclusion,
            Conclusion::Pending
        );
    }

    // 4. Resume through Pi: the host's reconciler settles label-103, and the rest runs.
    let output = pi(&scratch, "resume").output().expect("Pi resumes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains(r#""status":"needs_review""#), "{stdout}");

    let (history, statuses) = journal(&scratch);
    let label = &status(&statuses, "label-103").assessment;
    assert_eq!(
        (label.conclusion, label.basis, label.attempts),
        (Conclusion::Applied, Basis::Reconciled, 1)
    );
    let probe = history
        .probes
        .iter()
        .find(|probe| probe.operation == "label-103")
        .expect("label-103 was probed");
    let reconciliation = probe.outcome.as_ref().expect("the probe answered");
    assert_eq!(reconciliation.verdict, Verdict::Applied);
    assert!(
        reconciliation.evidence.contains("receipt"),
        "{reconciliation:?}"
    );
    for done in ["label-101", "unlabel-104", "label-105"] {
        let assessment = &status(&statuses, done).assessment;
        assert_eq!(
            (assessment.conclusion, assessment.next),
            (Conclusion::Applied, NextStep::Done),
            "{done}"
        );
    }
    let gated = &status(&statuses, "comment-102").assessment;
    assert_eq!(
        (gated.conclusion, gated.next, gated.attempts),
        (Conclusion::Unknown, NextStep::Review, 1),
        "the gated comment is never repeated"
    );
    assert_eq!(
        history.sessions[1]
            .end
            .as_ref()
            .map(|end| end.status.as_str()),
        Some("needs_review")
    );

    let ledger = ledger(&scratch);
    assert!(
        ledger
            .rows
            .iter()
            .all(|row| row.tool != "issues.add_comment"),
        "the gated comment never reached the fixture: {:?}",
        ledger.rows
    );
    assert_eq!(
        ledger
            .changes
            .iter()
            .map(|change| change.duplicates)
            .sum::<u64>(),
        0
    );
    let reconciled = ledger
        .changes
        .iter()
        .find(|change| change.tool == "issues.add_label" && change.issue == Some(103))
        .expect("label-103 reached the fixture");
    assert_eq!((reconciled.requests, reconciled.effects), (1, 1));

    // 5. Every dispatched attempt passed Pi's gate, which saw its attempt key.
    let audit = scratch.audit();
    let nested: Vec<_> = audit
        .iter()
        .filter(|entry| entry["nested"] == true)
        .collect();
    assert_eq!(nested.len(), history.attempts.len(), "{audit:?}");
    for attempt in &history.attempts {
        let entry = nested
            .iter()
            .find(|entry| entry["attempt"] == attempt.key.as_str())
            .unwrap_or_else(|| panic!("attempt {} passed Pi's gate", attempt.key));
        let expected = if attempt.operation == "comment-102" {
            "blocked"
        } else {
            "allowed"
        };
        assert_eq!(entry["decision"], expected, "{entry}");
    }

    let trace = run(
        codemode,
        &[
            "plan",
            "trace",
            "--journal",
            path(&scratch.file("plan.journal")),
            "--session",
            "2",
        ],
    );
    let mut stored = Vec::new();
    for entry in fs::read_dir(&scratch.0).expect("scratch lists") {
        let entry = entry.expect("entry");
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("plan.journal")
        {
            stored.extend(fs::read(entry.path()).expect("journal file reads"));
        }
    }
    for (what, bytes) in [
        ("journal", stored.as_slice()),
        ("trace", trace.stdout.as_slice()),
        ("outcome", output.stdout.as_slice()),
    ] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(SECRET),
            "Pi's environment leaked into the {what}"
        );
    }
}
