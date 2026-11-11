//! What the command-line tests share: a scratch directory, the offline MCP
//! server on HTTP or stdio, and the `codemode` binary.
#![allow(
    dead_code,
    reason = "each test file uses a different part of this module"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
pub const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
pub const FIXTURE: &str = env!("CARGO_BIN_EXE_codemode-mcp-fixture");

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-cli-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("create scratch directory");
        Self(path)
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A test may have made the directory read-only.
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.0)
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// An HTTP fixture server, killed when dropped.
pub struct Server {
    child: Child,
    pub url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the fixture on a free loopback port with its ledger and call log
/// in `scratch`.
pub fn serve(scratch: &Scratch, name: &str, args: &[&str]) -> Server {
    let address = scratch.file(&format!("{name}.addr"));
    let child = Command::new(FIXTURE)
        .args(["--http", path(&address)])
        .args(["--ledger", path(&scratch.file("ledger.jsonl"))])
        .args(["--calls", path(&scratch.file("calls.jsonl"))])
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
pub fn http_file(scratch: &Scratch, server: &Server) -> PathBuf {
    scratch.write(
        "server.json",
        &json!({"url": server.url, "trust_annotations": true}).to_string(),
    )
}

/// A server file for a stdio fixture that trusts its annotations. Every
/// command starts the server again, so its notes are a file.
pub fn stdio_file(scratch: &Scratch, args: &[&str]) -> PathBuf {
    let state = scratch.file("state.json");
    let ledger = scratch.file("ledger.jsonl");
    let mut all = vec![
        "--stdio",
        "--state",
        path(&state),
        "--ledger",
        path(&ledger),
    ];
    all.extend_from_slice(args);
    scratch.write(
        "server.json",
        &json!({"command": FIXTURE, "args": all, "trust_annotations": true}).to_string(),
    )
}

/// `codemode` with `args`, run in `scratch`, without a terminal.
pub fn command(scratch: &Scratch, args: &[&str]) -> Command {
    let mut command = Command::new(CODEMODE);
    command
        .args(args)
        .current_dir(&scratch.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub fn codemode(scratch: &Scratch, args: &[&str]) -> Output {
    command(scratch, args).output().expect("codemode starts")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8")
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Run `codemode` and require exit code `code`.
pub fn exits(scratch: &Scratch, code: i32, args: &[&str]) -> Output {
    let output = codemode(scratch, args);
    assert_eq!(
        output.status.code(),
        Some(code),
        "codemode {args:?}\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    output
}

/// Run `codemode`, require success, and return what it printed.
pub fn ok(scratch: &Scratch, args: &[&str]) -> String {
    stdout(&exits(scratch, 0, args))
}

pub fn json_of(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// The applied effects the fixture recorded, as (tool, arguments).
pub fn ledger(scratch: &Scratch) -> Vec<(String, Value)> {
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

/// How the fixture answered each call of `tool` it received, in order.
pub fn calls(scratch: &Scratch, tool: &str) -> Vec<String> {
    std::fs::read_to_string(scratch.file("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(json_of)
        .filter(|entry| entry["tool"] == tool)
        .map(|entry| entry["outcome"].as_str().expect("outcome").to_string())
        .collect()
}

/// A program that proposes `count` calls of `tool`, named `<prefix>-<index>`.
pub fn proposing(count: usize, prefix: &str, tool: &str) -> String {
    format!(
        "return {{ operations: Array.from({{ length: {count} }}, (_, i) => ({{ \
         id: `{prefix}-${{i}}`, tool: \"{tool}\", \
         arguments: {{ id: `note-${{i}}`, text: `text ${{i}}` }} }})) }};"
    )
}

/// One write to `notes.put` and one to `notes.append`, after one read.
pub const PROGRAM: &str = r#"
const { notes } = await tools.call("notes.list", {});
return {
  summary: `seen ${notes.length} notes`,
  operations: [
    { id: "put", tool: "notes.put", arguments: { id: "a", text: "one" } },
    { id: "append", tool: "notes.append", arguments: { id: "log", text: "line" } },
  ],
};
"#;

/// `codemode plan` from `program` into `<name>.plan.json`. Returns the plan
/// file's name.
pub fn plan(scratch: &Scratch, server_file: &Path, name: &str, program: &str) -> String {
    let program_file = scratch.write(&format!("{name}.ts"), program);
    ok(
        scratch,
        &[
            "plan",
            "--server",
            path(server_file),
            "--program",
            path(&program_file),
        ],
    );
    format!("{name}.plan.json")
}

/// The `--json` status of a plan file.
pub fn status(scratch: &Scratch, plan: &str) -> Value {
    json_of(&ok(scratch, &["status", plan, "--json"]))
}

/// The plan's full digest.
pub fn digest(scratch: &Scratch, plan: &str) -> String {
    status(scratch, plan)["digest"]
        .as_str()
        .expect("digest")
        .to_string()
}

/// `codemode apply <plan> --approve <its digest> --json` with `extra`
/// options. Returns the exit code and the status document.
pub fn approve(scratch: &Scratch, plan: &str, extra: &[&str]) -> (i32, Value) {
    let digest = digest(scratch, plan);
    let mut args = vec!["apply", plan, "--approve", &digest, "--approver", "tester"];
    args.extend_from_slice(extra);
    apply_with(scratch, &args)
}

/// `codemode apply <plan> --json` with `extra` options, for a plan that is
/// approved already. Returns the exit code and the status document.
pub fn apply(scratch: &Scratch, plan: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["apply", plan];
    args.extend_from_slice(extra);
    apply_with(scratch, &args)
}

fn apply_with(scratch: &Scratch, args: &[&str]) -> (i32, Value) {
    let mut args = args.to_vec();
    args.push("--json");
    let output = codemode(scratch, &args);
    let code = output.status.code().expect("exit code");
    let text = stdout(&output);
    assert!(
        !text.trim().is_empty(),
        "codemode {args:?} printed no status: {}",
        stderr(&output)
    );
    (code, json_of(&text))
}

/// Write `id` of a status document.
pub fn write<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["writes"]
        .as_array()
        .expect("writes")
        .iter()
        .find(|write| write["id"] == id)
        .unwrap_or_else(|| panic!("no write {id}: {status}"))
}

/// The state and next step of write `id` in a status document.
pub fn standing(status: &Value, id: &str) -> (String, String) {
    let write = write(status, id);
    let text = |key: &str| write[key].as_str().expect("string").to_string();
    (text("state"), text("next"))
}

/// The states of every write of a status document, in plan order.
pub fn states(status: &Value) -> Vec<(String, String)> {
    status["writes"]
        .as_array()
        .expect("writes")
        .iter()
        .map(|write| {
            (
                write["id"].as_str().expect("id").to_string(),
                write["state"].as_str().expect("state").to_string(),
            )
        })
        .collect()
}
