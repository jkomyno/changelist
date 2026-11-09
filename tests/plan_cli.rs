//! Crash and recovery through the real binaries: the executor is killed with
//! SIGKILL while the fixture provider holds requests, and the fixture's own
//! ledger checks what actually happened. The legacy Deno setting names no executable.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use codemode::fixture::Fixture;
use codemode::{Conclusion, Durability, Journal, NextStep, assess_history};
use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const FIXTURE: &str = env!("CARGO_BIN_EXE_codemode-fixture");
const ACCOUNT: &str = "triage-bot";

#[test]
fn plan_preview_escapes_terminal_text_without_changing_the_approved_data() {
    let scratch = Scratch::new();
    let summary = "safe\u{1b}[2J\u{1b}[HAPPROVED\nforged line";
    let key = "x\u{1b}[31m";
    let plan = json!({"version":1,"summary":summary,"operations":[{
        "id":"op","tool":"test.action",
        "scope":{"provider":"test","account":"bot","destination":"repo"},
        "arguments":{key:1}
    }]})
    .to_string();
    let path = scratch.file("plan.json");
    std::fs::write(&path, &plan).expect("write plan");
    let expected = codemode::Plan::parse(&plan).expect("valid plan");
    let preview = ok(CODEMODE, &["plan", "check", path.to_str().expect("path")]);
    assert!(!preview.chars().any(|c| c.is_control() && c != '\n'));
    assert!(!preview.contains("\nforged line"));
    assert!(preview.contains(expected.digest().as_str()));
    let output = ok(
        CODEMODE,
        &["plan", "check", path.to_str().expect("path"), "--json"],
    );
    let decoded: Value = serde_json::from_str(&output).expect("JSON preview");
    assert_eq!(decoded["digest"], expected.digest().as_str());
    assert_eq!(decoded["summary"], summary);
    assert_eq!(decoded["operations"][0]["argument_keys"][0], key);
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-plan-cli-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("create scratch directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn fixture(&self) -> PathBuf {
        self.file("fixture.db")
    }

    fn journal(&self) -> PathBuf {
        self.file("journal.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    command
}

fn ok(program: &str, args: &[&str]) -> String {
    let output = command(program, args).output().expect("starts");
    assert!(output.status.success(), "{program} {args:?}: {output:?}");
    String::from_utf8(output.stdout).expect("UTF-8")
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// Seed the fixture, write a plan, and approve it.
fn setup(scratch: &Scratch, operations: &[Value], policy: &[&str]) {
    let fixture = scratch.fixture();
    ok(
        FIXTURE,
        &[
            "init",
            "--db",
            path(&fixture),
            "--repo",
            "acme/widgets",
            "--account",
            ACCOUNT,
            "--account",
            "other-bot",
            "--issues",
            "101-110",
            "--label",
            "103:needs-info",
        ],
    );
    let declaration = ok(
        FIXTURE,
        &["declare", "--db", path(&fixture), "--account", ACCOUNT],
    );
    std::fs::write(scratch.file("provider.json"), declaration).expect("write declaration");
    let plan = json!({"version": 1, "operations": operations});
    std::fs::write(scratch.file("plan.json"), plan.to_string()).expect("write plan");
    approve(scratch, "plan.json", policy).expect("approved");
}

fn approve(scratch: &Scratch, plan: &str, policy: &[&str]) -> Result<String, Output> {
    let plan = scratch.file(plan);
    let checked: Value =
        serde_json::from_str(&ok(CODEMODE, &["plan", "check", path(&plan), "--json"]))
            .expect("check prints JSON");
    let digest = checked["digest"].as_str().expect("digest").to_string();
    let journal = scratch.journal();
    let provider = scratch.file("provider.json");
    let mut args = vec![
        "plan",
        "approve",
        path(&plan),
        "--journal",
        path(&journal),
        "--provider",
        path(&provider),
        "--digest",
        &digest,
        "--approver",
        "tester",
    ];
    args.extend_from_slice(policy);
    let output = command(CODEMODE, &args).output().expect("starts");
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        Err(output)
    }
}

fn label(issue: u64, action: &str) -> Value {
    json!({"id": format!("label-{issue}"), "tool": format!("issues.{action}_label"),
           "scope": {"provider": "fixture.issues", "account": ACCOUNT, "destination": "acme/widgets"},
           "arguments": {"issue": issue, "label": if action == "add" { "triage" } else { "needs-info" }}})
}

fn comment(issue: u64) -> Value {
    json!({"id": format!("comment-{issue}"), "tool": "issues.add_comment",
           "scope": {"provider": "fixture.issues", "account": ACCOUNT, "destination": "acme/widgets"},
           "arguments": {"issue": issue, "body": "triaged"}, "preconditions": [{"issue_state": "open"}]})
}

fn fault(scratch: &Scratch, tool: &str, issue: u64, mode: &str, count: u32) {
    ok(
        FIXTURE,
        &[
            "fault",
            "--db",
            path(&scratch.fixture()),
            "--tool",
            tool,
            "--issue",
            &issue.to_string(),
            "--mode",
            mode,
            "--count",
            &count.to_string(),
        ],
    );
}

fn session(scratch: &Scratch, kind: &str, account: &str) -> Command {
    let journal = scratch.journal();
    let fixture = scratch.fixture();
    let mut command = command(
        CODEMODE,
        &[
            "plan",
            kind,
            "--journal",
            path(&journal),
            "--fixture",
            path(&fixture),
            "--account",
            account,
            "--deno",
            "/codemode-test-no-deno/deno",
        ],
    );
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

fn ledger(scratch: &Scratch) -> codemode::fixture::Ledger {
    Fixture::open(scratch.fixture())
        .expect("fixture opens")
        .ledger()
        .expect("ledger reads")
}

/// Wait until the fixture has received `count` held requests, then SIGKILL the executor.
fn kill_when_held(scratch: &Scratch, mut child: Child, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let held = ledger(scratch)
            .rows
            .iter()
            .filter(|row| row.outcome == "held")
            .count();
        if held >= count {
            break;
        }
        if let Some(status) = child.try_wait().expect("poll") {
            panic!("executor exited early with {status}");
        }
        assert!(Instant::now() < deadline, "the held request never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("SIGKILL");
    child.wait().expect("reap");
}

fn statuses(scratch: &Scratch) -> Vec<(String, Conclusion, NextStep)> {
    let journal = Journal::open(scratch.journal(), Durability::Process).expect("journal opens");
    let history = journal.history().expect("history");
    assess_history(journal.plan(), &history, &std::collections::HashSet::new())
        .into_iter()
        .map(|status| {
            (
                status.operation,
                status.assessment.conclusion,
                status.assessment.next,
            )
        })
        .collect()
}

/// Every request the fixture received has a journaled intent. This checks
/// membership after the fact; the order (intent committed before the provider is
/// entered) is proved in the executor's unit tests.
fn assert_intent_preceded_every_request(scratch: &Scratch) {
    let journal = Journal::open(scratch.journal(), Durability::Process).expect("journal opens");
    let keys: std::collections::HashSet<String> = journal
        .history()
        .expect("history")
        .attempts
        .into_iter()
        .map(|attempt| attempt.key)
        .collect();
    for row in ledger(scratch).rows {
        if let Some(key) = row.key {
            assert!(
                keys.contains(&key),
                "request {key} reached the provider without an intent"
            );
        }
    }
}

fn status_of(statuses: &[(String, Conclusion, NextStep)], id: &str) -> (Conclusion, NextStep) {
    statuses
        .iter()
        .find(|(operation, _, _)| operation == id)
        .map(|(_, conclusion, next)| (*conclusion, *next))
        .expect("operation exists")
}

#[test]
fn a_killed_executor_is_reconciled_without_repeating_effects() {
    let scratch = Scratch::new();
    let operations = [
        label(101, "add"),
        label(102, "add"),
        label(103, "remove"),
        label(104, "add"),
        comment(105),
        label(109, "add"),
        label(110, "add"),
    ];
    setup(&scratch, &operations, &["--concurrency", "4"]);
    fault(&scratch, "issues.add_label", 104, "lose-response", 1);
    fault(&scratch, "issues.add_comment", 105, "lose-response", 1);
    fault(&scratch, "issues.add_label", 109, "hold", 1);
    let child = session(&scratch, "run", ACCOUNT)
        .spawn()
        .expect("run starts");
    kill_when_held(&scratch, child, 1);

    // The effect of 104 happened; its response never arrived.
    assert_intent_preceded_every_request(&scratch);
    let after_kill = statuses(&scratch);
    assert_eq!(
        status_of(&after_kill, "label-104"),
        (Conclusion::Unknown, NextStep::Reconcile)
    );
    assert_eq!(
        status_of(&after_kill, "comment-105"),
        (Conclusion::Unknown, NextStep::Review)
    );
    assert_eq!(
        status_of(&after_kill, "label-109"),
        (Conclusion::Unknown, NextStep::Reconcile)
    );

    let output = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let outcome: Value = serde_json::from_slice(&output.stdout).expect("outcome JSON");
    assert_eq!(outcome["status"], "needs_review");
    let resumed = statuses(&scratch);
    for id in [
        "label-101",
        "label-102",
        "label-103",
        "label-104",
        "label-109",
        "label-110",
    ] {
        assert_eq!(
            status_of(&resumed, id),
            (Conclusion::Applied, NextStep::Done),
            "{id}"
        );
    }
    assert_eq!(
        status_of(&resumed, "comment-105"),
        (Conclusion::Unknown, NextStep::Review)
    );

    // The provider's ledger: 109 was requested twice and applied once; nothing duplicated.
    assert_intent_preceded_every_request(&scratch);
    let ledger = ledger(&scratch);
    assert!(
        ledger.changes.iter().all(|change| change.duplicates == 0),
        "{ledger:?}"
    );
    let requests = |tool: &str, issue: u64| {
        ledger
            .rows
            .iter()
            .filter(|row| row.tool == tool && row.issue == Some(issue))
            .count()
    };
    assert_eq!(requests("issues.add_label", 104), 1);
    assert_eq!(requests("issues.add_label", 109), 2);
    assert_eq!(requests("issues.add_comment", 105), 1);

    // A person confirms the comment; nothing else remains.
    let journal = scratch.journal();
    ok(
        CODEMODE,
        &[
            "plan",
            "review",
            "--journal",
            path(&journal),
            "--operation",
            "comment-105",
            "--as",
            "applied",
            "--reviewer",
            "tester",
        ],
    );
    let output = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let inspection = ok(CODEMODE, &["plan", "inspect", "--journal", path(&journal)]);
    assert!(
        inspection.contains("nothing: every operation is applied or satisfied"),
        "{inspection}"
    );
    assert!(
        !inspection.contains("triaged"),
        "inspection printed an argument value"
    );
}

#[test]
fn repeated_crashes_stop_at_the_attempt_budget() {
    let scratch = Scratch::new();
    setup(&scratch, &[label(101, "add")], &["--max-attempts", "2"]);
    fault(&scratch, "issues.add_label", 101, "hold", 3);

    let child = session(&scratch, "run", ACCOUNT)
        .spawn()
        .expect("run starts");
    kill_when_held(&scratch, child, 1);
    // Reconciliation fences attempt 1, then attempt 2 is held too.
    let child = session(&scratch, "resume", ACCOUNT)
        .spawn()
        .expect("resume starts");
    kill_when_held(&scratch, child, 2);
    assert_intent_preceded_every_request(&scratch);

    let output = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let resumed = statuses(&scratch);
    assert_eq!(
        status_of(&resumed, "label-101"),
        (Conclusion::NotApplied, NextStep::Review)
    );
    let ledger = ledger(&scratch);
    assert_eq!(ledger.rows.len(), 2, "no third request: {ledger:?}");
    assert!(ledger.rows.iter().all(|row| !row.effect));
}

#[test]
fn a_second_executor_cannot_take_over_a_running_journal() {
    let scratch = Scratch::new();
    setup(&scratch, &[label(101, "add")], &[]);
    fault(&scratch, "issues.add_label", 101, "hold", 1);
    let first = session(&scratch, "run", ACCOUNT)
        .spawn()
        .expect("run starts");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ledger(&scratch)
        .rows
        .iter()
        .any(|row| row.outcome == "held")
    {
        assert!(Instant::now() < deadline, "the held request never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(second.status.code(), Some(2), "{second:?}");
    assert!(String::from_utf8_lossy(&second.stderr).contains("locked by another codemode process"));
    kill_when_held(&scratch, first, 1);
    assert_eq!(ledger(&scratch).rows.len(), 1);
}

#[test]
fn revoked_authorization_or_another_account_sends_nothing() {
    let scratch = Scratch::new();
    setup(&scratch, &[label(101, "add"), label(102, "add")], &[]);
    fault(&scratch, "issues.add_label", 101, "hold", 1);
    let child = session(&scratch, "run", ACCOUNT)
        .spawn()
        .expect("run starts");
    kill_when_held(&scratch, child, 1);
    let requests = ledger(&scratch).rows.len();

    let fixture = scratch.fixture();
    ok(
        FIXTURE,
        &["revoke", "--db", path(&fixture), "--account", ACCOUNT],
    );
    let output = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("refused authorization"));
    let output = session(&scratch, "resume", "other-bot")
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("acts as fixture.issues/other-bot"));
    assert_eq!(
        ledger(&scratch).rows.len(),
        requests,
        "a refused session sent requests"
    );

    ok(
        FIXTURE,
        &["restore", "--db", path(&fixture), "--account", ACCOUNT],
    );
    let output = session(&scratch, "resume", ACCOUNT)
        .output()
        .expect("resume");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn a_changed_plan_needs_a_new_journal_and_damage_fails_closed() {
    let scratch = Scratch::new();
    setup(&scratch, &[label(101, "add")], &[]);
    let changed = json!({"version": 1, "operations": [label(102, "add")]});
    std::fs::write(scratch.file("changed.json"), changed.to_string()).expect("write");
    let refused = approve(&scratch, "changed.json", &[]).expect_err("conflict");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("create a new journal"));

    let damaged = scratch.file("damaged.db");
    std::fs::write(&damaged, b"SQLite format 3\0 but not really").expect("write");
    let output = command(CODEMODE, &["plan", "inspect", "--journal", path(&damaged)])
        .output()
        .expect("inspect");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("damaged") || stderr.contains("not a journal"),
        "{stderr}"
    );
}

#[test]
fn the_fixture_refuses_a_scope_it_does_not_act_as() {
    let scratch = Scratch::new();
    setup(&scratch, &[label(101, "add")], &[]);
    let fixture = scratch.fixture();
    let call = |scope: Value| {
        command(
            FIXTURE,
            &[
                "call",
                "--db",
                path(&fixture),
                "--account",
                ACCOUNT,
                "--tool",
                "issues.add_label",
                "--key",
                "k1",
                "--arguments",
                r#"{"issue":101,"label":"triage"}"#,
                "--scope",
                &scope.to_string(),
            ],
        )
        .output()
        .expect("call runs")
    };
    let other = json!({"provider": "fixture.issues", "account": "other-bot", "destination": "acme/widgets"});
    let refused = call(other.clone());
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stdout).contains("permission_denied"));
    let elsewhere =
        json!({"provider": "fixture.issues", "account": ACCOUNT, "destination": "acme/other"});
    assert_eq!(call(elsewhere).status.code(), Some(1));
    assert!(
        ledger(&scratch).rows.iter().all(|row| !row.effect),
        "a refused scope applied an effect"
    );

    let authorize = |scope: Value| {
        command(
            FIXTURE,
            &[
                "authorize",
                "--db",
                path(&fixture),
                "--account",
                ACCOUNT,
                "--scope",
                &scope.to_string(),
            ],
        )
        .output()
        .expect("authorize runs")
        .status
        .code()
    };
    let reconcile = command(
        FIXTURE,
        &[
            "reconcile",
            "--db",
            path(&fixture),
            "--account",
            ACCOUNT,
            "--tool",
            "issues.add_label",
            "--arguments",
            r#"{"issue":101,"label":"triage"}"#,
            "--keys",
            "k1",
            "--scope",
            r#"{"provider":"fixture.issues","account":"triage-bot","destination":"acme/other"}"#,
        ],
    )
    .output()
    .expect("reconcile runs");
    assert!(
        String::from_utf8_lossy(&reconcile.stdout).contains("unauthorized"),
        "{reconcile:?}"
    );

    let own =
        json!({"provider": "fixture.issues", "account": ACCOUNT, "destination": "acme/widgets"});
    assert_eq!(authorize(own.clone()), Some(0));
    assert_eq!(authorize(other), Some(1));
    ok(
        FIXTURE,
        &["revoke", "--db", path(&fixture), "--account", ACCOUNT],
    );
    assert_eq!(authorize(own), Some(1));
}
