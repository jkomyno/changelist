//! Native OpenCode integration checks against a real OpenCode process. They need
//! OpenCode 1.19.5 and are ignored by default; run them with `mise run opencode-check`.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
/// A host credential: present in OpenCode's environment, never in the guest's.
const TOKEN: &str = "codemode-opencode-secret-value";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-opencode-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create a fresh scratch directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.file(name)).expect("file exists")).expect("JSON file")
    }

    fn audit(&self) -> Vec<Value> {
        fs::read_to_string(self.file("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit lines are JSON"))
            .collect()
    }

    fn events(&self) -> Vec<Value> {
        self.audit()
            .into_iter()
            .map(|entry| entry["event"].clone())
            .collect()
    }

    /// Wait until the adapter has written a complete trace.
    fn await_trace(&self, deadline: Instant) -> Value {
        loop {
            if let Ok(bytes) = fs::read(self.file("trace.json")) {
                return serde_json::from_slice(&bytes).expect("the trace is written atomically");
            }
            assert!(
                Instant::now() < deadline,
                "no trace; audit: {:?}",
                self.audit()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until an audit entry matches.
    fn await_audit(&self, deadline: Instant, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            if let Some(entry) = self.audit().into_iter().find(&matches) {
                return entry;
            }
            assert!(Instant::now() < deadline, "audit: {:?}", self.audit());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Launch OpenCode with only the demo plugin, isolated from user configuration,
/// state, credentials, and the network.
fn opencode(scratch: &Scratch, program: &str, args: &[&str]) -> Command {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let demo = root.join("integrations/opencode/demo");
    let home = scratch.file("home");
    let config_dir = home.join(".config/opencode");
    fs::create_dir_all(config_dir.join("node_modules")).expect("config directory");
    // A seeded config directory stops OpenCode from installing its plugin package.
    fs::write(
        config_dir.join("package.json"),
        r#"{"dependencies":{"@opencode-ai/plugin":"1.19.5"}}"#,
    )
    .expect("package.json");
    fs::write(
        config_dir.join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"":{"dependencies":{"@opencode-ai/plugin":"1.19.5"}}}}"#,
    )
    .expect("package-lock.json");
    let project = scratch.file("project");
    fs::create_dir_all(&project).expect("project directory");

    let mut config: Value =
        serde_json::from_slice(&fs::read(demo.join("opencode.json")).expect("demo config"))
            .expect("demo config is JSON");
    config.as_object_mut().expect("object").remove("$comment");
    config["plugin"] = json!([demo.join("host.ts")]);

    let mut command =
        Command::new(std::env::var_os("CODEMODE_OPENCODE").unwrap_or_else(|| "opencode".into()));
    command
        .current_dir(&project)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("OPENCODE_CONFIG_CONTENT", config.to_string())
        .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "1")
        .env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1")
        .env("OPENCODE_DISABLE_CLAUDE_CODE", "1")
        .env("OPENCODE_AUTH_CONTENT", "{}")
        .env("OPENCODE_DB", ":memory:")
        // Any non-loopback request fails instead of reaching the network.
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("CODEMODE_OC_PROGRAM", root.join(program))
        .env("CODEMODE_OC_AUDIT", scratch.file("audit.jsonl"))
        .env("CODEMODE_OC_TRACE", scratch.file("trace.json"))
        .env("CODEMODE_OC_BIN", env!("CARGO_BIN_EXE_codemode"))
        .env("CODEMODE_DEMO_TOKEN", TOKEN)
        .args(args);
    if let Some(deno) = std::env::var_os("CODEMODE_TEST_DENO") {
        command.env("CODEMODE_OC_DENO", deno);
    }
    command
}

fn outcomes(trace: &Value) -> Vec<(Value, Value, Value)> {
    trace["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            (
                call["call"]["tool"].clone(),
                call["outcome"]["status"].clone(),
                call["outcome"]["error"]["code"].clone(),
            )
        })
        .collect()
}

/// Host-reported facts as (call id, kind), in arrival order.
fn facts(trace: &Value) -> Vec<(u64, String)> {
    trace["provider_facts"]
        .as_array()
        .expect("provider facts")
        .iter()
        .map(|fact| {
            (
                fact["call_id"].as_u64().expect("call id"),
                fact["kind"].as_str().expect("kind").to_owned(),
            )
        })
        .collect()
}

/// Run a command to completion, killing it if it outlives `timeout`.
fn output_within(command: &mut Command, timeout: Duration) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("OpenCode starts");
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("stdout is piped")));
    let stderr = drain(Box::new(child.stderr.take().expect("stderr is piped")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("OpenCode status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "OpenCode did not finish in {timeout:?}: {}",
                String::from_utf8_lossy(&stderr.join().unwrap_or_default())
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: stderr.join().expect("stderr reader"),
    }
}

#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check`"]
fn opencode_permissions_gate_nested_calls_under_rust_scheduling() {
    let scratch = Scratch::new();
    let output = output_within(
        opencode(
            &scratch,
            "integrations/opencode/demo/workflow.ts",
            &["run", "--model", "codemode-demo/scripted"],
        )
        .arg("Run the codemode demo workflow.")
        .stdin(Stdio::null()),
        Duration::from_secs(30),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains(r#""status":"success""#), "{output:?}");

    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "success");
    assert_eq!(
        trace["output"],
        json!({"stock": [3, 0], "environment": "denied",
               "refused": ["permission_denied", "tool_denied"]})
    );
    assert_eq!(
        outcomes(&trace),
        [
            (json!("lookup"), json!("succeeded"), Value::Null),
            (json!("lookup"), json!("succeeded"), Value::Null),
            (
                json!("purge"),
                json!("provider_failed"),
                json!("permission_denied")
            ),
            (json!("bash"), json!("rejected"), json!("tool_denied")),
        ]
    );
    assert!(
        trace["calls"][2]["outcome"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("specified a rule")),
        "OpenCode's deny rule applied: {trace}"
    );
    // OpenCode's permission decisions reach the trace as host claims; the call Rust
    // rejected never reached the host.
    let granted = [
        "received",
        "permission_requested",
        "permission_granted",
        "executor_started",
    ];
    let mut expected: Vec<(u64, String)> = Vec::new();
    for id in [1, 2] {
        expected.extend(granted.iter().map(|kind| (id, (*kind).to_owned())));
    }
    expected.extend(
        ["received", "permission_requested", "permission_denied"]
            .iter()
            .map(|kind| (3, (*kind).to_owned())),
    );
    let mut actual = facts(&trace);
    actual.sort_by_key(|(id, _)| *id);
    assert_eq!(actual, expected, "{trace}");
    // A rule denial needs no approval, the denied tool never ran, and the turn continued.
    assert_eq!(
        scratch.events(),
        [json!("model"), json!("model")],
        "{:?}",
        scratch.audit()
    );
    let everything = format!("{stdout}{trace}");
    assert!(!everything.contains(TOKEN), "OpenCode's environment leaked");
}

#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check`"]
fn opencode_rejected_approval_refuses_later_calls_and_stops_the_turn() {
    let scratch = Scratch::new();
    let output = output_within(
        opencode(
            &scratch,
            "integrations/opencode/demo/approval.ts",
            &["run", "--model", "codemode-demo/scripted"],
        )
        .arg("Run the approval program.")
        .stdin(Stdio::null()),
        Duration::from_secs(30),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rejected permission"),
        "OpenCode reports its own rejection: {output:?}"
    );

    // `opencode run` refuses every approval, as a user rejecting it would. The guest
    // catches the rejection and tries again; the adapter refuses without asking.
    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "success", "{trace}");
    assert_eq!(
        trace["output"],
        json!({"published": "permission_rejected", "stock": "permission_rejected"})
    );
    assert_eq!(
        outcomes(&trace),
        [
            (
                json!("publish"),
                json!("provider_failed"),
                json!("permission_rejected")
            ),
            (
                json!("lookup"),
                json!("provider_failed"),
                json!("permission_rejected")
            ),
        ],
        "nothing runs after a rejected approval"
    );
    assert_eq!(
        scratch.events(),
        [
            json!("model"),
            json!("permission.asked"),
            json!("permission.replied")
        ],
        "the turn stopped without a further model request: {:?}",
        scratch.audit()
    );
    assert_eq!(scratch.audit()[1]["permission"], "codemode_publish");
    assert_eq!(scratch.audit()[2]["reply"], "reject");
}

/// An `opencode serve` process, killed when dropped.
struct Server {
    child: Child,
    address: String,
}

impl Server {
    fn start(command: &mut Command, scratch: &Scratch) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(fs::File::create(scratch.file("stderr.log")).expect("stderr log"))
            .spawn()
            .expect("OpenCode server starts");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (sender, receiver) = mpsc::channel();
        // Read the address, then keep draining stdout so the server never blocks.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Some((_, url)) = line.split_once("listening on http://") {
                    let _ = sender.send(url.trim().trim_end_matches('/').to_owned());
                }
            }
        });
        let mut server = Self {
            child,
            address: String::new(),
        };
        server.address = receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|_| {
                panic!(
                    "OpenCode did not print its address: {}",
                    fs::read_to_string(scratch.file("stderr.log")).unwrap_or_default()
                )
            });
        server
    }

    /// A minimal HTTP/1.1 exchange with the server.
    fn http(&self, method: &str, path: &str, body: Value) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.address).expect("OpenCode accepts connections");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let body = body.to_string();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.address,
            body.len()
        )
        .expect("request is writable");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("response arrives in time");
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_owned()
        };
        (status, body)
    }

    /// Create a session and prompt it without waiting for the turn to finish.
    fn prompt(&self) -> String {
        let (status, session) = self.http("POST", "/session", json!({}));
        assert_eq!(status, 200, "{session}");
        let session: Value = serde_json::from_str(&session).expect("session JSON");
        let session = session["id"].as_str().expect("session id").to_owned();
        let (status, body) = self.http(
            "POST",
            &format!("/session/{session}/prompt_async"),
            json!({"model": {"providerID": "codemode-demo", "modelID": "scripted"},
                   "parts": [{"type": "text", "text": "Run the program."}]}),
        );
        assert!((200..300).contains(&status), "{status} {body}");
        session
    }

    fn reply(&self, request: &Value, reply: &str) {
        let request = request["id"].as_str().expect("permission request id");
        let (status, body) = self.http(
            "POST",
            &format!("/permission/{request}/reply"),
            json!({"reply": reply}),
        );
        assert_eq!(status, 200, "{body}");
    }

    fn abort(&self, session: &str) {
        let (status, body) = self.http("POST", &format!("/session/{session}/abort"), json!({}));
        assert_eq!(status, 200, "{body}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dechunk(mut body: &str) -> String {
    let mut decoded = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).expect("chunk size");
        if size == 0 {
            break;
        }
        decoded.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    decoded
}

fn serve(scratch: &Scratch, program: &str) -> Server {
    Server::start(
        &mut opencode(
            scratch,
            program,
            &["serve", "--hostname", "127.0.0.1", "--port", "0"],
        ),
        scratch,
    )
}

fn has(audit: &[Value], expected: Value) -> bool {
    audit.iter().any(|entry| {
        expected
            .as_object()
            .expect("object")
            .iter()
            .all(|(key, value)| &entry[key] == value)
    })
}

#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check`"]
fn opencode_approval_reply_and_abort_reach_the_rust_run() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "integrations/opencode/demo/approve-and-cancel.ts");
    let deadline = Instant::now() + Duration::from_secs(20);
    let session = server.prompt();

    // Approve `publish` the way an OpenCode client does.
    let asked = scratch.await_audit(deadline, |entry| entry["event"] == "permission.asked");
    assert_eq!(asked["permission"], "codemode_publish");
    server.reply(&asked, "once");

    // Abort the session while the slow host call runs.
    scratch.await_audit(deadline, |entry| {
        entry["event"] == "started" && entry["tool"] == "wait"
    });
    let aborted_at = Instant::now();
    server.abort(&session);
    let trace = scratch.await_trace(deadline);
    let latency = aborted_at.elapsed();

    assert_eq!(trace["status"], "cancelled", "{trace}");
    assert_eq!(
        outcomes(&trace),
        [
            (json!("publish"), json!("succeeded"), Value::Null),
            (json!("wait"), json!("unknown"), Value::Null),
        ],
        "a dispatched call without an observed result stays unknown"
    );
    let audit = scratch.audit();
    for expected in [
        json!({"event": "permission.replied", "reply": "once"}),
        json!({"event": "executed", "tool": "publish"}),
        json!({"event": "aborted", "tool": "wait"}),
    ] {
        assert!(
            has(&audit, expected.clone()),
            "missing {expected} in {audit:?}"
        );
    }
    assert!(latency < Duration::from_secs(5), "abort took {latency:?}");
}

#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check`"]
fn opencode_abort_during_approval_prevents_a_late_approved_call() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "integrations/opencode/demo/approval.ts");
    let deadline = Instant::now() + Duration::from_secs(20);
    let session = server.prompt();

    let asked = scratch.await_audit(deadline, |entry| entry["event"] == "permission.asked");
    server.abort(&session);
    let trace = scratch.await_trace(deadline);
    // OpenCode keeps the request pending after the abort; approving it now must not run the tool.
    let (status, pending) = server.http("GET", "/permission", json!({}));
    assert_eq!(status, 200, "{pending}");
    let pending: Value = serde_json::from_str(&pending).expect("permission list");
    assert_eq!(pending[0]["id"], asked["id"], "{pending}");
    server.reply(&asked, "once");
    scratch.await_audit(deadline, |entry| {
        entry["event"] == "permission.replied" && entry["reply"] == "once"
    });
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(trace["status"], "cancelled", "{trace}");
    assert_eq!(
        outcomes(&trace),
        [(json!("publish"), json!("unknown"), Value::Null)],
        "{trace}"
    );
    // The trace shows the call was still waiting for approval: no executor entry.
    assert_eq!(
        facts(&trace),
        [(1, "received".into()), (1, "permission_requested".into())],
        "{trace}"
    );
    let audit = scratch.audit();
    assert!(
        !has(&audit, json!({"event": "executed"})),
        "a late approval ran the tool: {audit:?}"
    );
}

#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check`"]
fn opencode_refuses_nonobject_arguments_before_permission_or_execution() {
    let scratch = Scratch::new();
    let program = scratch.file("invalid-arguments.ts");
    fs::write(
        &program,
        r#"
const errors = [];
for (const args of [null, 42, []]) {
  try { await tools.call('publish', args); }
  catch (error) { errors.push(error.code); }
}
return errors;
"#,
    )
    .expect("program");
    let output = output_within(
        opencode(
            &scratch,
            program.to_str().expect("UTF-8 path"),
            &["run", "--model", "codemode-demo/scripted"],
        )
        .arg("Run the invalid arguments program.")
        .stdin(Stdio::null()),
        Duration::from_secs(30),
    );
    assert!(output.status.success(), "{output:?}");
    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "success", "{trace}");
    assert_eq!(
        trace["output"],
        json!([
            "invalid_arguments",
            "invalid_arguments",
            "invalid_arguments"
        ])
    );
    assert_eq!(
        scratch.events(),
        [json!("model"), json!("model")],
        "no permission or execution: {:?}",
        scratch.audit()
    );
    assert!(facts(&trace).iter().all(|(_, kind)| kind == "received"));
}

#[cfg(feature = "embedded-v8")]
#[test]
#[ignore = "requires OpenCode 1.19.5; run `mise run opencode-check` with embedded-v8"]
fn opencode_embedded_backend_does_not_resolve_deno() {
    let scratch = Scratch::new();
    let output = output_within(
        opencode(
            &scratch,
            "integrations/opencode/demo/workflow.ts",
            &["run", "--model", "codemode-demo/scripted"],
        )
        .env("CODEMODE_OC_BACKEND", "embedded-v8")
        .env("CODEMODE_OC_DENO", "/codemode-test-no-deno/deno")
        .arg("Run the embedded backend workflow.")
        .stdin(Stdio::null()),
        Duration::from_secs(30),
    );
    assert!(output.status.success(), "{output:?}");
    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "success", "{trace}");
    assert_eq!(trace["backend"], "embedded_v8");
    assert_eq!(trace["output"]["stock"], json!([3, 0]));
}
