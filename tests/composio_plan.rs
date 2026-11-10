//! Offline checks of the Composio GitHub host's plan sessions through the real
//! plan bridge: its test-only entry (`integrations/composio/testing/host.ts`)
//! runs the production host under Deno with a stand-in for the `composio run`
//! helper that keeps its own ledger of calls and GitHub state. The Rust executor
//! is killed with SIGKILL mid-session and resumed by fresh host processes. No
//! account, credential, or network is used.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const ACCOUNT: &str = "ca_fake_bot";
const SCOPE_ACCOUNT: &str = "ca_fake_bot:widget-bot";
const ADD: &str = "GITHUB_ADD_LABELS_TO_AN_ISSUE";
const REMOVE: &str = "GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE";
const UPDATE: &str = "GITHUB_UPDATE_AN_ISSUE";
const COMMENT: &str = "GITHUB_CREATE_AN_ISSUE_COMMENT";
const PLAN_ID: &str = "0123456789abcdef01234567";
/// The host's own credential; it must never reach the journal.
const CREDENTIAL: &str = "codemode-composio-plan-credential";
/// Text the stand-in puts in results and errors; none may be kept.
const PROVIDER_TEXT: [&str; 2] = ["MARK-SECRET", "ca_LEAKEDinERROR"];
static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// An edit of the stand-in's ledger between sessions.
type LedgerChange = fn(&mut Value);

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn deno() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEMODE_TEST_DENO") {
        return path.into();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("deno"))
        .find(|candidate| candidate.is_file())
        .expect("deno is on PATH")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-composio-plan-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("scratch");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn ledger(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.file("github.json")).expect("ledger"))
            .expect("ledger is JSON")
    }

    fn change(&self, edit: impl FnOnce(&mut Value)) {
        let mut ledger = self.ledger();
        edit(&mut ledger);
        std::fs::write(self.file("github.json"), ledger.to_string()).expect("ledger written");
    }

    fn issue(&self, number: u64) -> Value {
        self.ledger()["repositories"]["acme/widgets"]["issues"][number.to_string()].clone()
    }

    /// Writes the stand-in received, as (slug, target, outcome), sorted:
    /// concurrent attempts reach the stand-in in no fixed order.
    fn writes(&self) -> Vec<(String, String, String)> {
        let mut writes: Vec<(String, String, String)> = self.ledger()["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .filter(|call| {
                [ADD, REMOVE, UPDATE, COMMENT].contains(&call["slug"].as_str().unwrap_or(""))
            })
            .map(|call| {
                let field = |name: &str| call[name].as_str().unwrap_or("").to_string();
                (field("slug"), field("target"), field("outcome"))
            })
            .collect();
        writes.sort();
        writes
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

fn write(slug: &str, target: &str, outcome: &str) -> (String, String, String) {
    (slug.into(), target.into(), outcome.into())
}

fn marker(operation: &str) -> String {
    format!("<!-- codemode:{PLAN_ID}:{operation} -->")
}

/// One operation on `acme/widgets` as the host writes it, with `extra` arguments.
fn operation(id: &str, tool: &str, issue: u64, extra: Value) -> Value {
    let mut arguments = json!({"owner": "acme", "repo": "widgets", "issue_number": issue});
    arguments
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    json!({"id": id, "tool": tool, "arguments": arguments,
           "scope": {"provider": "composio.github", "account": SCOPE_ACCOUNT, "destination": "acme/widgets:1001"}})
}

fn comment(id: &str, issue: u64) -> Value {
    operation(
        id,
        COMMENT,
        issue,
        json!({"body": format!("Findings.\n\n{}", marker(id))}),
    )
}

fn issue(state: &str, labels: &[&str]) -> Value {
    json!({"title": "t", "state": state, "labels": labels, "comments": []})
}

/// The stand-in's GitHub, with `faults`.
fn github(scratch: &Scratch, faults: Value) {
    let state = json!({
        "accounts": {"ca_fake_bot": "widget-bot", "ca_other": "someone"},
        "repositories": {
            "acme/widgets": {
                "id": 1001,
                "labels": ["triage", "bug", "documentation"],
                "permissions": {"push": true, "triage": true},
                "issues": {
                    "1": issue("open", &[]), "2": issue("open", &["triage"]), "3": issue("open", &[]),
                    "4": issue("open", &[]), "5": issue("closed", &[]),
                },
            },
            "acme/private": {"id": 2002, "labels": ["triage"], "permissions": {"push": true, "triage": true},
                             "issues": {"1": issue("open", &[])}},
        },
        "faults": faults,
        "calls": [],
    });
    std::fs::write(scratch.file("github.json"), state.to_string()).expect("state");
}

/// The stand-in, the provider declaration, and an approved plan of `operations`.
fn setup(scratch: &Scratch, operations: &[Value], faults: Value, concurrency: u32) {
    github(scratch, faults);
    let declared = host(scratch, "declare", &[]).output().expect("declare");
    assert!(declared.status.success(), "{declared:?}");
    std::fs::write(scratch.file("declaration.json"), &declared.stdout).expect("declaration");
    std::fs::write(
        scratch.file("plan.json"),
        json!({"version": 1, "operations": operations}).to_string(),
    )
    .expect("plan");

    let checked = check(scratch);
    let approved = Command::new(CODEMODE)
        .args(["plan", "approve", path(&scratch.file("plan.json"))])
        .args(["--journal", path(&scratch.file("journal.db"))])
        .args(["--provider", path(&scratch.file("declaration.json"))])
        .args(["--digest", checked["digest"].as_str().expect("digest")])
        .args(["--approver", "tester", "--max-probes", "3"])
        .args(["--concurrency", &concurrency.to_string()])
        .output()
        .expect("approve");
    assert!(approved.status.success(), "{approved:?}");
}

fn check(scratch: &Scratch) -> Value {
    let checked = Command::new(CODEMODE)
        .args(["plan", "check", path(&scratch.file("plan.json")), "--json"])
        .output()
        .expect("check");
    assert!(checked.status.success(), "{checked:?}");
    serde_json::from_slice(&checked.stdout).expect("JSON")
}

/// The Composio host under Deno, offline, with a credential in its own environment.
fn host(scratch: &Scratch, command: &str, extra: &[&str]) -> Command {
    let mut process = Command::new(deno());
    process
        .current_dir(root())
        .env("COMPOSIO_API_KEY", CREDENTIAL)
        .args([
            "run",
            "--allow-run",
            "--allow-read",
            "--allow-write",
            "--allow-env",
        ])
        .arg("integrations/composio/testing/host.ts")
        .arg(command)
        .args(["--account", ACCOUNT, "--repo", "acme/widgets"])
        .args(["--offline", path(&scratch.file("github.json"))])
        .args(["--bin", CODEMODE]);
    if command == "run" || command == "resume" {
        process.args(["--journal", path(&scratch.file("journal.db"))]);
        process.args(["--declaration", path(&scratch.file("declaration.json"))]);
    }
    process.args(extra).stdin(Stdio::null());
    process
}

fn session(scratch: &Scratch, command: &str) -> (Value, Output) {
    let output = host(scratch, command, &[]).output().expect("host runs");
    let summary = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "host prints one JSON line; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    clean(&String::from_utf8_lossy(&output.stderr), "host stderr");
    clean(&String::from_utf8_lossy(&output.stdout), "host stdout");
    (summary, output)
}

fn operations(summary: &Value) -> Vec<(String, String, String)> {
    summary["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .map(|row| {
            let field = |index: usize| row[index].as_str().unwrap_or("").to_string();
            (field(0), field(1), field(2))
        })
        .collect()
}

fn row(id: &str, conclusion: &str, next: &str) -> (String, String, String) {
    (id.into(), conclusion.into(), next.into())
}

/// Wait until `ready` holds, then SIGKILL the host's Rust executor (not the host).
fn kill_executor_when(child: &Child, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "the session never reached the kill point"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let children = Command::new("pgrep")
        .args(["-P", &child.id().to_string(), "-x", "codemode"])
        .output()
        .expect("pgrep");
    let pid = String::from_utf8_lossy(&children.stdout).trim().to_string();
    assert!(!pid.is_empty(), "the host has a codemode child");
    let killed = Command::new("kill")
        .args(["-KILL", &pid])
        .status()
        .expect("kill");
    assert!(killed.success());
}

/// Start `run` with `extra` options, stderr captured to a file, and kill its
/// executor once `ready` holds for the stderr text and the stand-in.
fn run_and_kill(scratch: &Scratch, extra: &[&str], ready: impl Fn(&str, &Scratch) -> bool) {
    let stderr = scratch.file("run.stderr");
    let mut run = host(scratch, "run", extra)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("stderr file"))
        .spawn()
        .expect("host starts");
    kill_executor_when(&run, || {
        ready(
            &std::fs::read_to_string(&stderr).unwrap_or_default(),
            scratch,
        )
    });
    let status = run.wait().expect("host exits");
    assert!(!status.success(), "the host saw its executor die");
    clean(
        &std::fs::read_to_string(&stderr).unwrap_or_default(),
        "host stderr",
    );
}

fn clean(text: &str, what: &str) {
    for kept in PROVIDER_TEXT.iter().chain([&CREDENTIAL]) {
        assert!(!text.contains(kept), "{what} kept {kept}: {text}");
    }
}

/// Raw journal files and inspection with messages, which must hold no provider text.
fn kept(scratch: &Scratch) -> String {
    let mut kept = String::new();
    for name in ["journal.db", "journal.db-wal"] {
        if let Ok(bytes) = std::fs::read(scratch.file(name)) {
            kept.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    for format in [&["--json"][..], &[][..]] {
        let output = Command::new(CODEMODE)
            .args([
                "plan",
                "inspect",
                "--journal",
                path(&scratch.file("journal.db")),
                "--messages",
            ])
            .args(format)
            .output()
            .expect("inspect");
        assert!(output.status.success(), "{output:?}");
        kept.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    clean(&kept, "the journal or its diagnosis");
    kept
}

fn comments(scratch: &Scratch, issue: u64) -> Vec<(String, String)> {
    scratch.issue(issue)["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .map(|comment| {
            (
                comment["user"].as_str().expect("user").to_string(),
                comment["body"].as_str().expect("body").to_string(),
            )
        })
        .collect()
}

#[test]
fn a_lost_comment_reply_and_a_killed_executor_reconcile_by_marker_with_one_comment() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[
            comment("comment-1", 1),
            operation("label-3", ADD, 3, json!({"labels": ["triage"]})),
        ],
        json!({format!("{ADD} acme/widgets#3"): "fail_after_apply"}),
        2,
    );

    // Session 1: the comment is created but its observed success is dropped by
    // the host (fault injection); the label is added but reported as failed.
    // Then the Rust executor is killed.
    run_and_kill(
        &scratch,
        &["--drop-reply", "comment-1"],
        |stderr, scratch| {
            stderr.contains("dropped the reply for comment-1") && scratch.writes().len() == 2
        },
    );
    assert_eq!(comments(&scratch, 1).len(), 1);

    // Session 2: the marker search finds our comment (applied, attributed by
    // receipt); the label is on the issue (satisfied, not attributed).
    let (summary, output) = session(&scratch, "resume");
    assert!(output.status.success(), "{summary}");
    assert_eq!(
        operations(&summary),
        [
            row("comment-1", "applied", "done"),
            row("label-3", "satisfied", "done")
        ]
    );
    assert_eq!(
        scratch.writes(),
        [
            write(ADD, "acme/widgets#3", "applied_then_failed"),
            write(COMMENT, "acme/widgets#1", "applied"),
        ],
        "no write was sent twice"
    );
    assert_eq!(
        comments(&scratch, 1),
        [(
            "widget-bot".to_string(),
            format!("Findings.\n\n{}", marker("comment-1"))
        )]
    );
    for call in scratch.ledger()["calls"].as_array().expect("calls") {
        assert_eq!(call["account"], ACCOUNT, "{call}");
    }
    let kept = kept(&scratch);
    assert!(kept.contains("retention summary/1"), "{kept}");
    assert!(
        kept.contains("1 comment by the approved login carries this operation's marker"),
        "{kept}"
    );
}

#[test]
fn a_held_comment_stays_unknown_until_it_lands_and_another_authors_copy_settles_nothing() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[comment("comment-4", 4)],
        json!({format!("{COMMENT} acme/widgets#4"): "hold"}),
        1,
    );
    run_and_kill(&scratch, &[], |_, scratch| {
        scratch.writes() == [write(COMMENT, "acme/widgets#4", "held")]
    });

    // Someone else posts a comment carrying our marker: evidence, not a receipt.
    let body = format!("Findings.\n\n{}", marker("comment-4"));
    let copied = body.clone();
    scratch.change(move |ledger| {
        ledger["repositories"]["acme/widgets"]["issues"]["4"]["comments"] =
            json!([{"id": 1, "user": "mallory", "body": copied}]);
    });
    let (summary, _) = session(&scratch, "resume");
    assert_eq!(summary["status"], "needs_review", "{summary}");
    assert_eq!(
        operations(&summary),
        [row("comment-4", "unknown", "review")]
    );
    assert!(
        kept(&scratch).contains("a comment by another GitHub user carries this operation's marker"),
        "the other author's copy is recorded as evidence"
    );

    // Our held request lands late; the next probe finds it.
    scratch.change(move |ledger| {
        let comments = &mut ledger["repositories"]["acme/widgets"]["issues"]["4"]["comments"];
        comments
            .as_array_mut()
            .expect("comments")
            .push(json!({"id": 2, "user": "widget-bot", "body": body}));
    });
    let (summary, output) = session(&scratch, "resume");
    assert!(output.status.success(), "{summary}");
    assert_eq!(operations(&summary), [row("comment-4", "applied", "done")]);
    assert_eq!(scratch.writes().len(), 1, "the comment was never resent");
}

#[test]
fn a_comment_whose_marker_is_absent_stays_unknown_and_is_never_resent() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[comment("comment-1", 1)],
        json!({format!("{COMMENT} acme/widgets#1"): "hold"}),
        1,
    );
    run_and_kill(&scratch, &[], |_, scratch| scratch.writes().len() == 1);
    let (summary, _) = session(&scratch, "resume");
    assert_eq!(
        operations(&summary),
        [row("comment-1", "unknown", "review")]
    );
    assert!(
        kept(&scratch).contains("marker_absent"),
        "absence is a probe error"
    );
    assert_eq!(scratch.writes().len(), 1);
}

#[test]
fn a_lost_label_removal_reply_reconciles_as_satisfied_not_attributed() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[operation("unlabel-2", REMOVE, 2, json!({"name": "triage"}))],
        json!({}),
        1,
    );
    run_and_kill(&scratch, &["--drop-reply", "unlabel-2"], |stderr, _| {
        stderr.contains("dropped the reply for unlabel-2")
    });
    let (summary, output) = session(&scratch, "resume");
    assert!(output.status.success(), "{summary}");
    assert_eq!(
        operations(&summary),
        [row("unlabel-2", "satisfied", "done")]
    );
    assert_eq!(
        scratch.writes(),
        [write(REMOVE, "acme/widgets#2", "applied")]
    );
    assert!(kept(&scratch).contains("who changed it is unknown"));
}

#[test]
fn a_close_whose_open_precondition_no_longer_holds_is_refused_with_nothing_sent() {
    let scratch = Scratch::new();
    let mut close = operation(
        "close-1",
        UPDATE,
        1,
        json!({"state": "closed", "state_reason": "not_planned"}),
    );
    close["preconditions"] = json!([{"issue_state": "open"}]);
    setup(&scratch, &[close], json!({}), 1);
    // A maintainer closes the issue after approval.
    scratch.change(|ledger| {
        ledger["repositories"]["acme/widgets"]["issues"]["1"]["state"] = json!("closed");
    });
    let (summary, _) = session(&scratch, "run");
    assert_eq!(operations(&summary), [row("close-1", "refused", "review")]);
    assert!(scratch.writes().is_empty());
    assert!(kept(&scratch).contains("precondition_failed"));
}

#[test]
fn a_dry_run_envelope_ends_the_session_not_applied_and_never_successful() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[
            operation("label-1", ADD, 1, json!({"labels": ["triage"]})),
            operation("label-3", ADD, 3, json!({"labels": ["triage"]})),
        ],
        json!({format!("{ADD} acme/widgets#1"): "dry_run"}),
        1,
    );
    let (summary, output) = session(&scratch, "run");
    assert_eq!(output.status.code(), Some(3), "{summary}");
    assert_eq!(summary["status"], "interrupted", "{summary}");
    assert_eq!(
        operations(&summary),
        [
            row("label-1", "not_applied", "dispatch"),
            row("label-3", "pending", "dispatch")
        ]
    );
    assert_eq!(scratch.writes(), [write(ADD, "acme/widgets#1", "dry_run")]);
    assert_eq!(scratch.issue(1)["labels"], json!([]));
}

#[test]
fn a_revoked_or_switched_account_or_a_changed_repository_refuses_the_session() {
    let cases: [(&str, LedgerChange); 3] = [
        ("account_inactive", |ledger| {
            ledger["connections"] = json!([{"id": "ca_fake_bot", "status": "EXPIRED"}]);
        }),
        ("account_changed", |ledger| {
            ledger["accounts"]["ca_fake_bot"] = json!("someone-else");
        }),
        ("repository_changed", |ledger| {
            ledger["repositories"]["acme/widgets"]["id"] = json!(3003);
        }),
    ];
    for (code, change) in cases {
        let scratch = Scratch::new();
        setup(
            &scratch,
            &[operation("label-1", ADD, 1, json!({"labels": ["triage"]}))],
            json!({}),
            1,
        );
        scratch.change(change);
        let (summary, output) = session(&scratch, "run");
        assert_eq!(output.status.code(), Some(4), "{code}: {summary}");
        assert_eq!(summary["status"], "refused", "{code}: {summary}");
        assert!(
            summary["detail"]
                .as_str()
                .is_some_and(|detail| detail.ends_with(&format!("({code})"))),
            "{code}: {summary}"
        );
        assert!(scratch.writes().is_empty(), "{code}");
    }
}

#[test]
fn a_cancelled_session_leaves_the_write_unknown_and_never_resends_it() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[operation("label-1", ADD, 1, json!({"labels": ["triage"]}))],
        json!({format!("{ADD} acme/widgets#1"): "hold"}),
        1,
    );
    let run = host(&scratch, "run", &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("host starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    while scratch.writes() != [write(ADD, "acme/widgets#1", "held")] {
        assert!(Instant::now() < deadline, "the write was never sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    let terminated = Command::new("kill")
        .args(["-TERM", &run.id().to_string()])
        .status()
        .expect("kill");
    assert!(terminated.success());
    let output = run.wait_with_output().expect("host exits");
    let summary: Value = serde_json::from_slice(&output.stdout).expect("summary");
    assert_eq!(summary["status"], "interrupted", "{summary}");
    assert_eq!(
        operations(&summary),
        [row("label-1", "unknown", "reconcile")]
    );

    // Resume probes, finds the label absent, and asks for review instead of resending.
    let (summary, _) = session(&scratch, "resume");
    assert_eq!(operations(&summary), [row("label-1", "unknown", "review")]);
    assert_eq!(scratch.writes().len(), 1, "no second write");
}

#[test]
fn the_handlers_pass_their_offline_checks() {
    let scratch = Scratch::new();
    let output = Command::new(deno())
        .current_dir(root())
        .args(["run", "--allow-read", "--allow-write"])
        .arg("integrations/composio/testing/plan-check.ts")
        .arg(&scratch.0)
        .output()
        .expect("deno runs");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn preflight_reports_the_exact_plan_and_refuses_what_the_contract_does_not_allow() {
    let scratch = Scratch::new();
    let mut close = operation("close-1", UPDATE, 1, json!({"state": "closed"}));
    close["preconditions"] = json!([{"issue_state": "open"}]);
    setup(
        &scratch,
        &[
            operation("label-1", ADD, 1, json!({"labels": ["triage"]})),
            comment("comment-1", 1),
            close,
        ],
        json!({}),
        1,
    );
    let checked = check(&scratch);
    let output = host(
        &scratch,
        "preflight",
        &["--plan", path(&scratch.file("plan.json"))],
    )
    .output()
    .expect("preflight");
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("preflight JSON");
    assert_eq!(report["preflight"], "ready", "{report}");
    assert_eq!(report["digest"], checked["digest"]);
    assert_eq!(report["checked_operations"], 3);
    assert_eq!(
        report["operations"],
        json!([
            {"operation": "label-1", "authorized": true, "label_exists": true, "already_done": false},
            {"operation": "comment-1", "authorized": true, "already_done": false},
            {"operation": "close-1", "authorized": true, "precondition_holds": true, "already_done": false},
        ])
    );

    // Valid plans that the provider contract refuses, each reported without a write.
    let mut outside = operation("label-9", ADD, 1, json!({"repo": "private"}));
    outside["scope"]["destination"] = json!("acme/private:2002");
    outside["arguments"]["labels"] = json!(["triage"]);
    let refused = [
        operation(
            "comment-2",
            COMMENT,
            1,
            json!({"body": "no marker at the end"}),
        ),
        operation(
            "close-2",
            UPDATE,
            1,
            json!({"state": "closed", "labels": []}),
        ),
        operation("label-2", ADD, 1, json!({"labels": ["no-such-label"]})),
        outside,
    ];
    std::fs::write(
        scratch.file("plan.json"),
        json!({"version": 1, "operations": refused}).to_string(),
    )
    .expect("plan");
    let output = host(
        &scratch,
        "preflight",
        &["--plan", path(&scratch.file("plan.json"))],
    )
    .output()
    .expect("preflight");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("preflight JSON");
    assert_eq!(report["preflight"], "not_ready");
    assert_eq!(
        report["operations"],
        json!([
            {"operation": "comment-2", "refused": "comment_unmarked"},
            {"operation": "close-2", "refused": "unsupported_arguments"},
            {"operation": "label-2", "authorized": true, "label_exists": false, "already_done": false},
            {"operation": "label-9", "refused": "destination_not_allowed"},
        ])
    );
    assert!(scratch.writes().is_empty());

    // No stand-in state exists: contacting any provider would fail with a file error.
    std::fs::remove_file(scratch.file("github.json")).expect("remove stand-in");
    for invalid in ["{}", r#"{"version":1,"operations":[]}"#] {
        std::fs::write(scratch.file("plan.json"), invalid).expect("invalid plan");
        let output = host(
            &scratch,
            "preflight",
            &["--plan", path(&scratch.file("plan.json"))],
        )
        .output()
        .expect("preflight");
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "no readiness claim");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("plan check"), "{error}");
        assert!(
            !error.contains("github.json"),
            "provider not contacted: {error}"
        );
    }
}

#[test]
fn preflight_stops_probing_a_scope_once_authorization_refuses_it() {
    let cases: [(&str, LedgerChange); 2] = [
        ("account_ambiguous", |ledger| {
            ledger["connections"] = json!([
                {"id": "ca_fake_bot", "status": "ACTIVE"},
                {"id": "ca_other", "status": "ACTIVE", "alias": "ca_fake_bot"},
            ]);
        }),
        ("repository_changed", |ledger| {
            ledger["repositories"]["acme/widgets"]["id"] = json!(3003);
        }),
    ];
    for (code, change) in cases {
        let scratch = Scratch::new();
        let mut close = operation("close-1", UPDATE, 1, json!({"state": "closed"}));
        close["preconditions"] = json!([{"issue_state": "open"}]);
        setup(
            &scratch,
            &[
                operation("label-1", ADD, 1, json!({"labels": ["triage"]})),
                comment("comment-1", 1),
                close,
            ],
            json!({}),
            1,
        );
        scratch.change(|ledger| ledger["calls"] = json!([]));
        scratch.change(change);
        let output = host(
            &scratch,
            "preflight",
            &["--plan", path(&scratch.file("plan.json"))],
        )
        .output()
        .expect("preflight");
        assert_eq!(output.status.code(), Some(3), "{code}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).expect("preflight JSON");
        assert_eq!(
            report["operations"],
            json!([
                {"operation": "label-1", "authorized": code},
                {"operation": "comment-1", "authorized": code},
                {"operation": "close-1", "authorized": code},
            ]),
            "{code}"
        );
        // Only the identity reads: no label, issue, or reconcile read through the refused selector.
        let slugs: Vec<String> = scratch.ledger()["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|call| call["slug"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            slugs.iter().all(|slug| [
                "GITHUB_GET_THE_AUTHENTICATED_USER",
                "GITHUB_GET_A_REPOSITORY"
            ]
            .contains(&slug.as_str())),
            "{code}: {slugs:?}"
        );
    }
}

#[test]
fn an_identity_read_that_never_answers_ends_declare_with_an_error_naming_the_step() {
    let scratch = Scratch::new();
    github(
        &scratch,
        json!({"GITHUB_GET_A_REPOSITORY acme/widgets": "hang"}),
    );
    let mut declare = host(&scratch, "declare", &["--read-timeout-ms", "300"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("host starts");
    let deadline = Instant::now() + Duration::from_secs(15);
    while declare.try_wait().expect("host status").is_none() {
        if Instant::now() >= deadline {
            let _ = declare.kill();
            panic!("declare hung on a read that never answers");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = declare.wait_with_output().expect("host exits");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "no declaration: {output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("resolving the account's GitHub login and the repository")
            && error.contains("did not finish within 300 ms"),
        "{error}"
    );
    clean(&error, "host stderr");
}
