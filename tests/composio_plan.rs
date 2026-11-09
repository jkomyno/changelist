//! Offline checks of the Composio plan host (`integrations/composio/plan-host.ts`)
//! through the real plan bridge: the host runs under Deno with a stand-in for the
//! `composio run` helper that keeps its own ledger of calls and label state. The
//! Rust executor is killed with SIGKILL mid-session and resumed by fresh host
//! processes. No account, credential, or network is used.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const ACCOUNT: &str = "ca_fake_bot";
const ADD: &str = "GITHUB_ADD_LABELS_TO_AN_ISSUE";
/// The host's own credential; it must never reach the journal.
const CREDENTIAL: &str = "codemode-composio-plan-credential";
static SCRATCH: AtomicU64 = AtomicU64::new(0);

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
        serde_json::from_str(&std::fs::read_to_string(self.file("labels.json")).expect("ledger"))
            .expect("ledger is JSON")
    }

    /// Mutation requests the stand-in received, as (target, outcome).
    fn mutations(&self) -> Vec<(String, String)> {
        self.ledger()["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .filter(|call| call["slug"] == ADD)
            .map(|call| {
                (
                    call["target"].as_str().unwrap_or("").to_string(),
                    call["outcome"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
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

fn operation(id: &str, issue: u64, destination: &str) -> Value {
    let (owner, repo) = destination.split_once('/').expect("owner/repo");
    json!({"id": id, "tool": ADD,
           "scope": {"provider": "composio.github", "account": ACCOUNT, "destination": destination},
           "arguments": {"owner": owner, "repo": repo, "issue_number": issue, "labels": ["triage"]}})
}

/// The stand-in's state, a host config, and an approved plan of `operations`.
fn setup(scratch: &Scratch, operations: &[Value], faults: Value, concurrency: u32) {
    let issues: serde_json::Map<String, Value> = (1..=9)
        .map(|issue| {
            (
                issue.to_string(),
                json!(if issue == 2 { vec!["triage"] } else { vec![] }),
            )
        })
        .collect();
    let state = json!({
        "accounts": [ACCOUNT],
        "repositories": {
            "acme/widgets": {"labels": ["triage", "bug"], "permissions": {"push": true, "triage": true}, "issues": issues},
            "acme/private": {"labels": ["triage"], "permissions": {"push": true, "triage": true}, "issues": {"1": []}},
        },
        "faults": faults,
        "calls": [],
    });
    std::fs::write(scratch.file("labels.json"), state.to_string()).expect("state");
    let config = json!({"account": ACCOUNT, "destinations": ["acme/widgets"]});
    std::fs::write(scratch.file("config.json"), config.to_string()).expect("config");
    let declaration = host(scratch, "declare", &[]).output().expect("declare");
    assert!(declaration.status.success(), "{declaration:?}");
    std::fs::write(scratch.file("declaration.json"), &declaration.stdout).expect("declaration");
    let plan = json!({"version": 1, "operations": operations});
    std::fs::write(scratch.file("plan.json"), plan.to_string()).expect("plan");

    let checked = Command::new(CODEMODE)
        .args(["plan", "check", path(&scratch.file("plan.json")), "--json"])
        .output()
        .expect("check");
    assert!(checked.status.success(), "{checked:?}");
    let checked: Value = serde_json::from_slice(&checked.stdout).expect("JSON");
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

/// The Composio plan host under Deno, offline, with a credential in its own environment.
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
        .arg("integrations/composio/plan-host.ts")
        .arg(command)
        .args(["--config", path(&scratch.file("config.json"))])
        .args(["--offline", path(&scratch.file("labels.json"))]);
    if command != "declare" {
        process.args(["--bin", CODEMODE]).arg("--deno").arg(deno());
    }
    if command == "run" || command == "resume" {
        process.args(["--journal", path(&scratch.file("journal.db"))]);
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
    kept
}

#[test]
fn a_lost_reply_and_a_killed_executor_reconcile_without_a_second_mutation() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[
            operation("label-1", 1, "acme/widgets"),
            operation("label-5", 5, "acme/widgets"),
            operation("label-6", 6, "acme/widgets"),
        ],
        json!({format!("{ADD} acme/widgets#5"): "hold"}),
        2,
    );

    // Session 1: label-1 is applied but its observed success is dropped by the
    // host (fault injection); label-5's request is still pending. Then the Rust
    // executor is killed.
    let stderr = scratch.file("run.stderr");
    let mut run = host(&scratch, "run", &["--drop-reply", "label-1"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("stderr file"))
        .spawn()
        .expect("host starts");
    kill_executor_when(&run, || {
        let dropped = std::fs::read_to_string(&stderr).unwrap_or_default();
        dropped.contains("dropped the reply for label-1")
            && scratch
                .mutations()
                .contains(&("acme/widgets#5".into(), "held".into()))
    });
    let status = run.wait().expect("host exits");
    assert!(!status.success(), "the host saw its executor die");
    assert_eq!(
        scratch.mutations(),
        [
            ("acme/widgets#1".to_string(), "applied".to_string()),
            ("acme/widgets#5".to_string(), "held".to_string())
        ]
    );

    // Session 2, a fresh host: label-1's label is on the issue (satisfied, not
    // attributed); label-5's is absent, which proves nothing, so it needs review;
    // label-6 runs.
    let (summary, _) = session(&scratch, "resume");
    assert_eq!(summary["status"], "needs_review", "{summary}");
    assert_eq!(
        operations(&summary),
        [
            row("label-1", "satisfied", "done"),
            row("label-5", "unknown", "review"),
            row("label-6", "applied", "done"),
        ]
    );

    // The pending request applies late.
    let mut ledger = scratch.ledger();
    ledger["repositories"]["acme/widgets"]["issues"]["5"] = json!(["triage"]);
    std::fs::write(scratch.file("labels.json"), ledger.to_string()).expect("late effect");

    // Session 3: the next probe finds it.
    let (summary, output) = session(&scratch, "resume");
    assert!(output.status.success(), "{summary}");
    assert_eq!(
        operations(&summary),
        [
            row("label-1", "satisfied", "done"),
            row("label-5", "satisfied", "done"),
            row("label-6", "applied", "done"),
        ]
    );

    // Exactly one mutation per operation, every call as the approved account.
    assert_eq!(
        scratch.mutations(),
        [
            ("acme/widgets#1".to_string(), "applied".to_string()),
            ("acme/widgets#5".to_string(), "held".to_string()),
            ("acme/widgets#6".to_string(), "applied".to_string()),
        ]
    );
    for call in scratch.ledger()["calls"].as_array().expect("calls") {
        assert_eq!(call["account"], ACCOUNT, "{call}");
    }

    let kept = kept(&scratch);
    assert!(
        !kept.contains("MARK-SECRET"),
        "Composio text was kept: {kept}"
    );
    assert!(
        !kept.contains(CREDENTIAL),
        "the host credential reached the journal"
    );
    assert!(kept.contains("retention summary/1"), "{kept}");
    assert!(
        kept.contains("satisfied - the label is on the issue; who added it is unknown"),
        "{kept}"
    );
}

#[test]
fn a_cancelled_session_leaves_the_mutation_unknown_and_never_resends_it() {
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[operation("label-7", 7, "acme/widgets")],
        json!({format!("{ADD} acme/widgets#7"): "hold"}),
        1,
    );
    let run = host(&scratch, "run", &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("host starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !scratch
        .mutations()
        .contains(&("acme/widgets#7".into(), "held".into()))
    {
        assert!(Instant::now() < deadline, "the mutation was never sent");
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
        [row("label-7", "unknown", "reconcile")]
    );

    // Resume probes, finds the label absent, and asks for review instead of resending.
    let (summary, _) = session(&scratch, "resume");
    assert_eq!(operations(&summary), [row("label-7", "unknown", "review")]);
    assert_eq!(scratch.mutations().len(), 1, "no second mutation");
}

#[test]
fn scope_and_precondition_refusals_send_no_mutation() {
    // A destination outside the host's allowlist: the session is refused at authorization.
    let scratch = Scratch::new();
    setup(
        &scratch,
        &[operation("label-1", 1, "acme/private")],
        json!({}),
        1,
    );
    let (summary, output) = session(&scratch, "run");
    assert_eq!(output.status.code(), Some(4), "{summary}");
    assert_eq!(summary["status"], "refused");
    assert!(
        summary["detail"]
            .as_str()
            .is_some_and(|detail| detail.ends_with("(destination_not_allowed)")),
        "{summary}"
    );
    assert!(
        scratch.ledger()["calls"]
            .as_array()
            .expect("calls")
            .is_empty()
    );

    // A precondition the provider cannot enforce: refused before any mutation.
    let scratch = Scratch::new();
    let mut guarded = operation("label-1", 1, "acme/widgets");
    guarded["preconditions"] = json!([{"issue_state": "open"}]);
    setup(&scratch, &[guarded], json!({}), 1);
    let (summary, _) = session(&scratch, "run");
    assert_eq!(operations(&summary), [row("label-1", "refused", "review")]);
    assert!(scratch.mutations().is_empty());
}

#[test]
fn the_handlers_pass_their_offline_checks() {
    let scratch = Scratch::new();
    let output = Command::new(deno())
        .current_dir(root())
        .args(["run", "--allow-read", "--allow-write"])
        .arg("integrations/composio/demo/plan-check.ts")
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
