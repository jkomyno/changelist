//! Crash and recovery through the real binaries: the fixture binary's plan
//! session is killed with SIGKILL while the fixture provider holds requests, the
//! `codemode` CLI inspects and reviews the journal, and the fixture's own ledger
//! checks what actually happened.
#![cfg(all(unix, feature = "testkit"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use codemode::fixture::Fixture;
use codemode::{Conclusion, JournalReader, NextStep, assess_history};
use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const FIXTURE: &str = env!("CARGO_BIN_EXE_codemode-fixture");
const ACCOUNT: &str = "triage-bot";

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
        FIXTURE,
        &[
            "session",
            kind,
            "--journal",
            path(&journal),
            "--db",
            path(&fixture),
            "--account",
            account,
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

/// Wait for both the held requests and the intended provider ledger checkpoint.
fn kill_when_held(scratch: &Scratch, mut child: Child, count: usize, requests: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let checkpoint = ledger(scratch);
        let held = checkpoint
            .rows
            .iter()
            .filter(|row| row.outcome == "held")
            .count();
        if held >= count && checkpoint.rows.len() >= requests {
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
    let journal = JournalReader::open(scratch.journal()).expect("journal opens");
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
    let journal = JournalReader::open(scratch.journal()).expect("journal opens");
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
    // Concurrent provider entry can differ from dispatch order. Do not kill
    // until the lost-response effects have reached the independent ledger too.
    kill_when_held(&scratch, child, 1, operations.len());
    let checkpoint = ledger(&scratch);
    for (tool, issue) in [("issues.add_label", 104), ("issues.add_comment", 105)] {
        assert!(
            checkpoint
                .rows
                .iter()
                .any(|row| row.tool == tool && row.issue == Some(issue) && row.effect),
            "missing intended lost-response effect: {tool} #{issue}: {checkpoint:?}"
        );
    }

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
    kill_when_held(&scratch, child, 1, 1);
    // Reconciliation fences attempt 1, then attempt 2 is held too.
    let child = session(&scratch, "resume", ACCOUNT)
        .spawn()
        .expect("resume starts");
    kill_when_held(&scratch, child, 2, 2);
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
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("locked by another codemode executor")
    );
    kill_when_held(&scratch, first, 1, 1);
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
    kill_when_held(&scratch, child, 1, 1);
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

/// A provider declaration for plans whose operations use tool `t` as `host.issues/bot`.
fn host_declaration() -> Value {
    json!({"provider": "host.issues", "account": "bot",
           "contracts": {"t": {"repeat": "safe", "reconcile": "receipt"}}})
}

/// A plan whose operation `id` passes `arguments`, written verbatim.
fn host_operation(id: &str, arguments: &str) -> String {
    format!(
        r#"{{"id":"{id}","tool":"t","scope":{{"provider":"host.issues","account":"bot","destination":"acme/widgets"}},"arguments":{arguments}}}"#
    )
}

fn host_plan(operations: &[String]) -> String {
    format!(r#"{{"version":1,"operations":[{}]}}"#, operations.join(","))
}

fn check_json(plan: &Path) -> Value {
    serde_json::from_str(&ok(CODEMODE, &["plan", "check", path(plan), "--json"]))
        .expect("check prints JSON")
}

/// Approve `plan` with `digest` against the host declaration.
fn approve_with(scratch: &Scratch, plan: &Path, digest: &str) -> Output {
    std::fs::write(
        scratch.file("provider.json"),
        host_declaration().to_string(),
    )
    .expect("write declaration");
    let journal = scratch.journal();
    let provider = scratch.file("provider.json");
    command(
        CODEMODE,
        &[
            "plan",
            "approve",
            path(plan),
            "--journal",
            path(&journal),
            "--provider",
            path(&provider),
            "--digest",
            digest,
            "--approver",
            "tester",
        ],
    )
    .output()
    .expect("approve starts")
}

/// Run the approved journal through `plan run --bridge`, acting as the host, and
/// return the raw text of every call frame the host received.
fn run_as_host(scratch: &Scratch) -> Vec<String> {
    use std::io::{BufRead, BufReader, Write};
    let journal = scratch.journal();
    let mut run = command(
        CODEMODE,
        &["plan", "run", "--journal", path(&journal), "--bridge"],
    );
    let mut child = run
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run starts");
    let mut input = child.stdin.take().expect("stdin");
    let frames = BufReader::new(child.stdout.take().expect("stdout"));
    let start = json!({"type": "start", "version": 3, "declaration": host_declaration()});
    writeln!(input, "{start}").expect("start");
    let mut calls = Vec::new();
    for line in frames.lines() {
        let line = line.expect("frame");
        let frame: Value = serde_json::from_str(&line).expect("frame is JSON");
        let reply = match frame["type"].as_str() {
            Some("authorize") => json!({"type": "authorized", "id": frame["id"], "ok": true}),
            Some("call") => {
                calls.push(line.clone());
                json!({"type": "result", "id": frame["id"], "ok": true, "value": null})
            }
            Some("outcome") => break,
            _ => panic!("unexpected frame {line}"),
        };
        writeln!(input, "{reply}").expect("reply");
    }
    drop(input);
    let output = child.wait_with_output().expect("run exits");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    calls
}

#[test]
fn numbers_a_json_host_would_round_are_refused_by_check() {
    let scratch = Scratch::new();
    let plan = scratch.file("plan.json");
    for token in [
        "18446744073709551616",
        "-9223372036854775809",
        "9007199254740992",
    ] {
        let text = host_plan(&[
            host_operation("fine", r#"{"n":1}"#),
            host_operation(
                "big",
                &format!(r#"{{"text":"{token}","nested":{{"n":[0,{token}]}}}}"#),
            ),
        ]);
        std::fs::write(&plan, text).expect("write plan");
        let output = command(CODEMODE, &["plan", "check", path(&plan)])
            .output()
            .expect("check starts");
        assert_eq!(output.status.code(), Some(2), "{token}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("operation big")
                && stderr.contains("arguments.nested.n[1]")
                && stderr.contains(token),
            "{stderr}"
        );
    }
}

#[test]
fn exact_values_and_explicit_nulls_reach_the_host_unchanged() {
    let scratch = Scratch::new();
    let with_null = scratch.file("with-null.json");
    let without = scratch.file("without.json");
    std::fs::write(
        &with_null,
        host_plan(&[host_operation("op", r#"{"a":null,"b":1}"#)]),
    )
    .expect("write");
    std::fs::write(&without, host_plan(&[host_operation("op", r#"{"b":1}"#)])).expect("write");
    assert_ne!(
        check_json(&with_null)["digest"],
        check_json(&without)["digest"],
        "an explicit null is part of the approved plan"
    );

    let plan = scratch.file("plan.json");
    std::fs::write(
        &plan,
        host_plan(&[
            host_operation(
                "exact",
                r#"{"max":9007199254740991,"min":-9007199254740991,"text":"18446744073709551616"}"#,
            ),
            host_operation("null", r#"{"a":null,"b":1}"#),
            host_operation("absent", r#"{"b":1}"#),
        ]),
    )
    .expect("write plan");
    let digest = check_json(&plan)["digest"]
        .as_str()
        .expect("digest")
        .to_string();
    let approved = approve_with(&scratch, &plan, &digest);
    assert!(approved.status.success(), "{approved:?}");
    let calls = run_as_host(&scratch);
    assert_eq!(calls.len(), 3, "{calls:?}");
    let arguments = |operation: &str| {
        let line = calls
            .iter()
            .find(|line| line.contains(&format!(r#""operation":"{operation}""#)))
            .expect("operation called");
        let frame: Value = serde_json::from_str(line).expect("JSON");
        (line.clone(), frame["arguments"].clone())
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
}

#[test]
fn one_changed_character_changes_the_digest_and_refuses_the_earlier_approval() {
    let scratch = Scratch::new();
    let plan = scratch.file("plan.json");
    let body = |text: &str| {
        host_plan(&[
            host_operation("label", r#"{"issue":7,"label":"triage"}"#),
            host_operation("comment", &format!(r#"{{"issue":7,"body":"{text}"}}"#)),
        ])
    };
    std::fs::write(&plan, body("Reproduced on main.")).expect("write plan");
    let reviewed = check_json(&plan)["digest"]
        .as_str()
        .expect("digest")
        .to_string();
    std::fs::write(&plan, body("Reproduced on main!")).expect("edit plan");
    let changed = check_json(&plan)["digest"]
        .as_str()
        .expect("digest")
        .to_string();
    assert_ne!(reviewed, changed);
    let refused = approve_with(&scratch, &plan, &reviewed);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("does not match"),
        "{refused:?}"
    );
    assert!(
        !scratch.journal().exists(),
        "a refused approval creates nothing"
    );
    let approved = approve_with(&scratch, &plan, &changed);
    assert!(approved.status.success(), "{approved:?}");
}

#[test]
fn check_shows_every_value_approval_binds() {
    let scratch = Scratch::new();
    // 256 operations whose bodies bring the plan near its size limit.
    let operations: Vec<String> = (0..codemode::MAX_OPERATIONS)
        .map(|index| {
            let body = format!("comment {index} ").repeat(200);
            host_operation(
                &format!("op-{index}"),
                &format!(r#"{{"issue":{index},"body":"{body}","label":null}}"#),
            )
        })
        .collect();
    let text = host_plan(&operations);
    assert!(text.len() > codemode::MAX_PLAN_BYTES / 2, "{}", text.len());
    let plan = scratch.file("plan.json");
    std::fs::write(&plan, &text).expect("write plan");
    std::fs::write(
        scratch.file("provider.json"),
        host_declaration().to_string(),
    )
    .expect("write declaration");
    let frozen = codemode::Plan::parse(&text).expect("valid plan");
    let provider = scratch.file("provider.json");

    let human = ok(
        CODEMODE,
        &["plan", "check", path(&plan), "--provider", path(&provider)],
    );
    for operation in &frozen.plan().operations {
        let mut arguments = operation.arguments.clone();
        arguments.sort_all_objects();
        assert!(
            human.contains(&format!("arguments {arguments}\n")),
            "{} missing from the review",
            operation.id
        );
    }
    assert!(human.contains(frozen.digest().as_str()), "{human}");
    assert!(human.contains("reconcile by receipt, repeat safe"));
    assert!(human.contains("retention metadata/1"), "{human}");
    assert!(
        human.contains("concurrency 4, max-attempts 3, max-probes 3"),
        "{human}"
    );

    let output = ok(
        CODEMODE,
        &[
            "plan",
            "check",
            path(&plan),
            "--provider",
            path(&provider),
            "--json",
        ],
    );
    assert!(
        output.len() < 2 * codemode::MAX_PLAN_BYTES,
        "{} bytes",
        output.len()
    );
    assert!(
        output.contains(&format!(r#""plan":{}"#, frozen.canonical())),
        "--json carries the canonical plan verbatim"
    );
    let checked: Value = serde_json::from_str(&output).expect("JSON");
    assert_eq!(checked["digest"], frozen.digest().as_str());
    let reread: codemode::Plan = serde_json::from_value(checked["plan"].clone()).expect("a plan");
    let refrozen = reread.freeze().expect("valid");
    assert_eq!(refrozen.canonical(), frozen.canonical());
    assert_eq!(refrozen.digest(), frozen.digest());
    assert_eq!(checked["declaration"]["retention"], "metadata/1");
    assert_eq!(
        checked["declaration"]["contracts"]["t"],
        json!({"repeat": "safe", "reconcile": "receipt"})
    );
    assert_eq!(
        checked["default_policy"],
        json!({"concurrency": 4, "max_attempts": 3, "max_probes": 3})
    );
}
