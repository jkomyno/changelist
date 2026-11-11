//! `codemode mcp` through the real binaries, against the offline MCP fixture
//! server in both protocol eras and on both transports. The fixture's own
//! ledger shows which effects were applied, whatever codemode believes.
#![cfg(all(unix, feature = "testkit"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const FIXTURE: &str = env!("CARGO_BIN_EXE_codemode-mcp-fixture");

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-mcp-{}-{}",
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

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        std::fs::write(&path, text).expect("write");
        path
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

/// An HTTP fixture server, killed when dropped.
struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the fixture on a free loopback port with its ledger in `scratch`.
fn serve(scratch: &Scratch, name: &str, args: &[&str]) -> Server {
    let address = scratch.file(&format!("{name}.addr"));
    let child = Command::new(FIXTURE)
        .args(["--http", path(&address)])
        .args(["--ledger", path(&scratch.file("ledger.jsonl"))])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("fixture starts");
    let started = Instant::now();
    let url = loop {
        if let Ok(url) = std::fs::read_to_string(&address) {
            break url.trim().to_string();
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "fixture did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    Server { child, url }
}

/// A server file for an HTTP fixture that trusts its annotations.
fn http_file(scratch: &Scratch, server: &Server) -> PathBuf {
    scratch.write(
        "server.json",
        &json!({"url": server.url, "trust_annotations": true}).to_string(),
    )
}

fn codemode(args: &[&str]) -> Output {
    Command::new(CODEMODE)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("codemode starts")
}

fn ok(args: &[&str]) -> String {
    let output = codemode(args);
    assert!(output.status.success(), "codemode {args:?}: {output:?}");
    String::from_utf8(output.stdout).expect("UTF-8")
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// The applied effects the fixture recorded, as (tool, arguments).
fn ledger(scratch: &Scratch) -> Vec<(String, Value)> {
    std::fs::read_to_string(scratch.file("ledger.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let entry = json_of(line);
            (
                entry["tool"].as_str().expect("tool").to_string(),
                entry["arguments"].clone(),
            )
        })
        .collect()
}

fn classes(server_file: &Path) -> Vec<(String, String)> {
    let listing = json_of(&ok(&[
        "mcp",
        "tools",
        "--server",
        path(server_file),
        "--json",
    ]));
    listing["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("name").to_string(),
                tool["class"].as_str().expect("class").to_string(),
            )
        })
        .collect()
}

const PROGRAM: &str = r#"
const { notes } = await tools.call("notes.list", {});
return {
  summary: `seen ${notes.length} notes`,
  operations: [
    { id: "put", tool: "notes.put", arguments: { id: "a", text: "one" } },
    { id: "append", tool: "notes.append", arguments: { id: "log", text: "line" } },
  ],
};
"#;

/// Investigate with `program`, declare, and approve into `journal.db`.
/// Returns the server file and the journal.
fn approve(scratch: &Scratch, server_file: &Path, program: &str, policy: &[&str]) -> PathBuf {
    let program = scratch.write("program.ts", program);
    let plan = scratch.file("plan.json");
    let investigated = json_of(&ok(&[
        "mcp",
        "investigate",
        "--server",
        path(server_file),
        "--program",
        path(&program),
        "--out",
        path(&plan),
    ]));
    let declaration = scratch.write(
        "declaration.json",
        &ok(&["mcp", "declare", "--server", path(server_file)]),
    );
    let journal = scratch.file("journal.db");
    let mut args = vec![
        "plan",
        "approve",
        path(&plan),
        "--journal",
        path(&journal),
        "--provider",
        path(&declaration),
        "--digest",
        investigated["digest"].as_str().expect("digest"),
        "--approver",
        "tester",
    ];
    args.extend_from_slice(policy);
    ok(&args);
    journal
}

fn session(kind: &str, server_file: &Path, journal: &Path, extra: &[&str]) -> (i32, Value) {
    let mut args = vec![
        "mcp",
        kind,
        "--server",
        path(server_file),
        "--journal",
        path(journal),
    ];
    args.extend_from_slice(extra);
    let output = codemode(&args);
    let code = output.status.code().expect("exit code");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!text.trim().is_empty(), "no outcome: {output:?}");
    (code, json_of(&text))
}

/// The conclusion and next step of operation `id` in a session outcome.
fn standing(outcome: &Value, id: &str) -> (String, String) {
    let operation = outcome["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .find(|operation| operation["operation"] == id)
        .unwrap_or_else(|| panic!("no operation {id}: {outcome}"));
    let text = |key: &str| operation[key].as_str().expect("string").to_string();
    (text("conclusion"), text("next"))
}

#[test]
fn tools_are_writes_until_a_person_trusts_the_server() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--page-size", "2"]);
    let untrusted = scratch.write(
        "untrusted.json",
        &json!({"url": server.url, "read_tools": ["notes.get"]}).to_string(),
    );
    let found = classes(&untrusted);
    assert_eq!(found.len(), 9, "every page is read: {found:?}");
    for (name, class) in &found {
        let expected = if name == "notes.get" { "read" } else { "write" };
        assert_eq!(class, expected, "{name}");
    }

    let trusted = http_file(&scratch, &server);
    let class = |name: &str| {
        classes(&trusted)
            .into_iter()
            .find(|(tool, _)| tool == name)
            .expect("listed")
            .1
    };
    assert_eq!(class("notes.list"), "read");
    assert_eq!(class("notes.put"), "repeatable_write");
    assert_eq!(class("notes.delete"), "repeatable_write");
    assert_eq!(class("notes.append"), "write");
    assert_eq!(class("plain.unannotated"), "write");

    let unknown = scratch.write(
        "unknown.json",
        &json!({"url": server.url, "read_tools": ["nope"]}).to_string(),
    );
    let output = codemode(&["mcp", "tools", "--server", path(&unknown)]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not list"));
}

#[test]
fn each_era_and_transport_agrees_on_a_version() {
    let scratch = Scratch::new();
    for (era, expected) in [
        ("modern", "2026-07-28"),
        ("legacy", "2025-11-25"),
        ("dual", "2026-07-28"),
    ] {
        let server = serve(&scratch, era, &["--era", era]);
        let file = scratch.write(
            &format!("{era}.json"),
            &json!({"url": server.url}).to_string(),
        );
        let listing = json_of(&ok(&["mcp", "tools", "--server", path(&file), "--json"]));
        assert_eq!(listing["protocol_version"], expected, "http {era}");
        assert_eq!(listing["tools"].as_array().expect("tools").len(), 9);

        let stdio = scratch.write(
            &format!("{era}-stdio.json"),
            &json!({"command": FIXTURE, "args": ["--stdio", "--era", era]}).to_string(),
        );
        let listing = json_of(&ok(&["mcp", "tools", "--server", path(&stdio), "--json"]));
        assert_eq!(listing["protocol_version"], expected, "stdio {era}");
        assert_eq!(listing["tools"].as_array().expect("tools").len(), 9);
    }
}

#[test]
fn a_header_comes_from_the_environment_and_is_never_printed() {
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--require-header", "x-api-key=s3cret-value"],
    );
    let file = scratch.write(
        "server.json",
        &json!({"url": server.url, "headers": {"x-api-key": "${FIXTURE_KEY}"}}).to_string(),
    );
    let run = |key: Option<&str>| {
        let mut command = Command::new(CODEMODE);
        command
            .args(["mcp", "tools", "--server", path(&file)])
            .stdin(Stdio::null())
            .env_remove("FIXTURE_KEY");
        if let Some(key) = key {
            command.env("FIXTURE_KEY", key);
        }
        command.output().expect("starts")
    };
    let missing = run(None);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("FIXTURE_KEY is not set"));

    let wrong = run(Some("wrong-value"));
    assert_eq!(wrong.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&wrong.stderr);
    assert!(stderr.contains("401"), "{stderr}");
    assert!(!stderr.contains("wrong-value"));

    let right = run(Some("s3cret-value"));
    assert!(right.status.success(), "{right:?}");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&right.stdout),
        String::from_utf8_lossy(&right.stderr)
    );
    assert!(all.contains("notes.list") && !all.contains("s3cret-value"));
}

#[test]
fn a_program_reads_and_proposes_but_cannot_write() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--sse"]);
    let file = http_file(&scratch, &server);
    let program = scratch.write(
        "program.ts",
        r#"
const text = await tools.call("raw.text", {});
const parsed = await tools.call("raw.json_text", {});
const echoed = await tools.call("region.echo", { region: "Zürich 北", query: "q" });
let denied = "allowed";
try { await tools.call("notes.append", { id: "x", text: "sneaky" }); } catch (error) { denied = "denied"; }
let missing = "found";
try { await tools.call("notes.get", { id: "absent" }); } catch (error) { missing = error.code; }
return { operations: [{ id: "record", tool: "notes.put", arguments: {
  id: "seen", text: [text, parsed.answer, echoed.region, denied, missing, input.who].join("|") } }] };
"#,
    );
    let input = scratch.write("input.json", r#"{"who": "me", "__proto__": {"x": 1}}"#);
    let plan_file = scratch.file("plan.json");
    let trace = scratch.file("trace.json");
    let result = json_of(&ok(&[
        "mcp",
        "investigate",
        "--server",
        path(&file),
        "--program",
        path(&program),
        "--out",
        path(&plan_file),
        "--input",
        path(&input),
        "--trace",
        path(&trace),
    ]));
    assert_eq!(result["operations"], 1);
    let plan = json_of(&std::fs::read_to_string(&plan_file).expect("plan"));
    let operation = &plan["operations"][0];
    assert_eq!(
        operation["arguments"]["text"],
        "plain words, not JSON|42|Zürich 北|denied|tool_error|me"
    );
    assert_eq!(
        operation["scope"],
        json!({"provider": "mcp", "account": server.url})
    );
    assert!(ledger(&scratch).is_empty(), "investigation applied nothing");
    assert!(trace.exists());
}

#[test]
fn a_proposal_outside_the_servers_contract_is_refused_before_review() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    let file = http_file(&scratch, &server);
    let cases = [
        (
            r#"return { operations: [{ id: "a", tool: "notes.list", arguments: {} }] };"#,
            "a read tool",
        ),
        (
            r#"return { operations: [{ id: "a", tool: "nope", arguments: {} }] };"#,
            "does not list",
        ),
        (
            r#"return { operations: [{ id: "a", tool: "notes.put", arguments: { id: "a" } }] };"#,
            "\"text\" is missing",
        ),
        (
            r#"return { operations: [{ id: "a", tool: "notes.put", arguments: { id: "a", text: "t", extra: 1 } }] };"#,
            "not in the tool's schema",
        ),
        (
            r#"return { operations: [{ id: "a", tool: "notes.put", arguments: { id: "a", text: "t" }, preconditions: [{}] }] };"#,
            "unknown key \"preconditions\"",
        ),
        (r#"return [];"#, "the program must return"),
    ];
    for (program, expected) in cases {
        let program = scratch.write("program.ts", program);
        let out = scratch.file("plan.json");
        let output = codemode(&[
            "mcp",
            "investigate",
            "--server",
            path(&file),
            "--program",
            path(&program),
            "--out",
            path(&out),
        ]);
        assert_eq!(output.status.code(), Some(2), "{expected}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "{stderr} should mention {expected}"
        );
        assert!(!out.exists(), "no plan is written for {expected}");
    }
}

#[test]
fn an_approved_plan_runs_once_on_each_transport_and_era() {
    for era in ["modern", "legacy"] {
        let scratch = Scratch::new();
        let server = serve(&scratch, "s", &["--era", era, "--sse"]);
        let file = http_file(&scratch, &server);
        let journal = approve(&scratch, &file, PROGRAM, &[]);
        let checked = ok(&[
            "plan",
            "check",
            path(&scratch.file("plan.json")),
            "--provider",
            path(&scratch.file("declaration.json")),
        ]);
        assert!(
            checked.contains("reconcile by none, repeat safe"),
            "{checked}"
        );
        assert!(checked.contains("reconcile by none, repeat unsafe"));
        assert!(checked.contains("tool definition sha256:"));

        let (code, outcome) = session("run", &file, &journal, &[]);
        assert_eq!(
            (code, outcome["status"].as_str()),
            (0, Some("completed")),
            "{outcome}"
        );
        assert_eq!(
            ledger(&scratch).tap_sorted(),
            [
                (
                    "notes.append".to_string(),
                    json!({"id": "log", "text": "line"})
                ),
                ("notes.put".to_string(), json!({"id": "a", "text": "one"})),
            ],
            "{era}"
        );
        // A second run of the same journal is refused before the server is asked.
        let again = codemode(&[
            "mcp",
            "run",
            "--server",
            path(&file),
            "--journal",
            path(&journal),
        ]);
        assert_eq!(again.status.code(), Some(2));
        assert_eq!(ledger(&scratch).len(), 2);
    }

    // stdio: every session starts the server again, so its state is a file.
    let scratch = Scratch::new();
    let file = scratch.write(
        "server.json",
        &json!({
            "command": FIXTURE,
            "args": ["--stdio", "--state", path(&scratch.file("state.json")),
                "--ledger", path(&scratch.file("ledger.jsonl"))],
            "trust_annotations": true,
        })
        .to_string(),
    );
    let journal = approve(&scratch, &file, PROGRAM, &[]);
    let (code, outcome) = session("run", &file, &journal, &[]);
    assert_eq!(
        (code, outcome["status"].as_str()),
        (0, Some("completed")),
        "{outcome}"
    );
    assert_eq!(ledger(&scratch).len(), 2);
}

/// Sort a ledger, because concurrent dispatch has no fixed order.
trait TapSorted {
    fn tap_sorted(self) -> Self;
}

impl TapSorted for Vec<(String, Value)> {
    fn tap_sorted(mut self) -> Self {
        self.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_string().cmp(&b.1.to_string())));
        self
    }
}

#[test]
fn a_lost_reply_repeats_only_the_write_the_server_calls_idempotent() {
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &[
            "--fault",
            "notes.put=drop-reply",
            "--fault",
            "notes.append=drop-reply",
        ],
    );
    let file = http_file(&scratch, &server);
    let journal = approve(&scratch, &file, PROGRAM, &["--concurrency", "1"]);

    // Both effects landed; codemode saw neither reply.
    let (code, first) = session("run", &file, &journal, &[]);
    assert_eq!(code, 3, "{first}");
    assert_eq!(
        standing(&first, "put"),
        ("unknown".into(), "dispatch".into())
    );
    assert_eq!(
        standing(&first, "append"),
        ("unknown".into(), "review".into())
    );
    let (code, second) = session("resume", &file, &journal, &[]);
    assert_eq!(code, 3, "{second}");
    assert_eq!(standing(&second, "put"), ("applied".into(), "done".into()));
    assert_eq!(
        standing(&second, "append"),
        ("unknown".into(), "review".into())
    );

    let applied = ledger(&scratch);
    let count = |tool: &str| applied.iter().filter(|(name, _)| name == tool).count();
    assert_eq!(count("notes.put"), 2, "the idempotent write was sent again");
    assert_eq!(
        count("notes.append"),
        1,
        "the other write was never repeated"
    );

    // A person settles the append; nothing more is sent.
    ok(&[
        "plan",
        "review",
        "--journal",
        path(&journal),
        "--operation",
        "append",
        "--as",
        "applied",
        "--reviewer",
        "tester",
    ]);
    let (code, done) = session("resume", &file, &journal, &[]);
    assert_eq!(
        (code, done["status"].as_str()),
        (0, Some("completed")),
        "{done}"
    );
    assert_eq!(ledger(&scratch).len(), 3);
}

#[test]
fn a_tool_error_or_a_server_error_leaves_a_write_for_a_person() {
    for fault in ["is-error", "rpc-error", "http-500", "input-required"] {
        let scratch = Scratch::new();
        let server = serve(
            &scratch,
            "s",
            &["--fault", &format!("notes.append={fault},sticky")],
        );
        let file = http_file(&scratch, &server);
        let journal = approve(&scratch, &file, PROGRAM, &[]);
        let (code, outcome) = session("run", &file, &journal, &[]);
        assert_eq!(code, 3, "{fault}: {outcome}");
        assert_eq!(standing(&outcome, "put"), ("applied".into(), "done".into()));
        assert_eq!(
            standing(&outcome, "append"),
            ("unknown".into(), "review".into()),
            "{fault}"
        );
        let (_, resumed) = session("resume", &file, &journal, &[]);
        assert_eq!(standing(&resumed, "append").1, "review", "{fault}");
        assert_eq!(
            ledger(&scratch),
            [("notes.put".to_string(), json!({"id": "a", "text": "one"}))],
            "{fault}: the failed write was sent once and never applied"
        );
    }
}

#[test]
fn a_deadline_does_not_prove_that_a_slow_write_did_nothing() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=delay-ms:1500"]);
    let file = http_file(&scratch, &server);
    let journal = approve(&scratch, &file, PROGRAM, &[]);
    let (code, outcome) = session("run", &file, &journal, &["--deadline-ms", "500"]);
    assert_eq!(code, 124, "{outcome}");
    assert_eq!(standing(&outcome, "append").0, "unknown");

    // The server ignored the cancellation: the effect lands after the deadline.
    let started = Instant::now();
    while ledger(&scratch).len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the late effect never landed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, resumed) = session("resume", &file, &journal, &[]);
    assert_eq!(
        standing(&resumed, "append"),
        ("unknown".into(), "review".into())
    );
    assert_eq!(ledger(&scratch).len(), 2, "resume did not send it again");
}

#[test]
fn an_abandoned_stdio_call_is_cancelled_and_stays_unknown() {
    let scratch = Scratch::new();
    let file = scratch.write(
        "server.json",
        &json!({
            "command": FIXTURE,
            "args": ["--stdio", "--state", path(&scratch.file("state.json")),
                "--ledger", path(&scratch.file("ledger.jsonl")),
                "--fault", "notes.append=delay-ms:1500,sticky"],
            "trust_annotations": true,
        })
        .to_string(),
    );
    let journal = approve(&scratch, &file, PROGRAM, &[]);
    let (code, outcome) = session("run", &file, &journal, &["--deadline-ms", "700"]);
    assert_eq!(code, 124, "{outcome}");
    assert_eq!(standing(&outcome, "put").0, "applied");
    assert_eq!(standing(&outcome, "append").0, "unknown");
    let appended = |scratch: &Scratch| {
        ledger(scratch)
            .iter()
            .filter(|(tool, _)| tool == "notes.append")
            .count()
    };
    // The fixture ignores the cancellation and applies the effect before it exits.
    let started = Instant::now();
    while appended(&scratch) < 1 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the late effect never landed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, resumed) = session("resume", &file, &journal, &[]);
    assert_eq!(
        standing(&resumed, "append"),
        ("unknown".into(), "review".into())
    );
    assert_eq!(appended(&scratch), 1, "resume did not send it again");
}

#[test]
fn another_server_or_a_changed_trust_setting_refuses_the_session() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    let file = http_file(&scratch, &server);
    let journal = approve(&scratch, &file, PROGRAM, &[]);

    let other = serve(&scratch, "other", &[]);
    let other_file = scratch.write(
        "other.json",
        &json!({"url": other.url, "trust_annotations": true}).to_string(),
    );
    let (code, outcome) = session("run", &other_file, &journal, &[]);
    assert_eq!((code, outcome["status"].as_str()), (4, Some("refused")));
    assert!(
        outcome["detail"]
            .as_str()
            .expect("detail")
            .contains("the provider acts as")
    );

    let untrusted = scratch.write("untrusted.json", &json!({"url": server.url}).to_string());
    let (code, outcome) = session("resume", &untrusted, &journal, &[]);
    assert_eq!(code, 4, "{outcome}");
    assert!(
        outcome["detail"]
            .as_str()
            .expect("detail")
            .contains("contracts changed")
    );
    assert!(
        ledger(&scratch).is_empty(),
        "a refused session sends nothing"
    );
}

#[test]
fn an_unreachable_server_is_a_startup_error() {
    let scratch = Scratch::new();
    let file = scratch.write("server.json", r#"{"url": "http://127.0.0.1:1/mcp"}"#);
    let output = codemode(&["mcp", "tools", "--server", path(&file)]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not sent"));

    let missing = scratch.write("missing.json", r#"{"command": "/nonexistent/mcp-server"}"#);
    let output = codemode(&["mcp", "tools", "--server", path(&missing)]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot start"));
}
