//! Native Pi integration checks against a real Pi process. They need Pi 1.4.0 and
//! are ignored by default; run them with `mise run pi-check`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static SCRATCH: AtomicU64 = AtomicU64::new(0);
const SECRET: &str = "codemode-pi-secret-value";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-pi-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Launch Pi with only the bridge and demo extensions, isolated from user configuration.
fn pi(scratch: &Scratch, program: &str, mode: &[&str]) -> Command {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new(std::env::var_os("CODEMODE_PI").unwrap_or_else(|| "pi".into()));
    command
        .current_dir(root)
        .env("PI_CODING_AGENT_DIR", scratch.file("agent"))
        .env("CODEMODE_PI_PROGRAM", root.join(program))
        .env("CODEMODE_PI_AUDIT", scratch.file("audit.jsonl"))
        // Present in Pi's environment; it must never reach the bridge or guest.
        .env("CODEMODE_DEMO_SECRET", SECRET)
        .args(mode)
        .args([
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
            "integrations/pi/demo/offline-host.ts",
            "--extension",
            "integrations/pi/demo/approval-gate.ts",
            "--model",
            "codemode-demo/scripted",
            "--tools",
            "rust_codemode,read,ls,bash,write,demo_wait",
            "--codemode-tools",
            "read,ls,bash,demo_wait",
            "--codemode-bin",
            env!("CARGO_BIN_EXE_codemode"),
            "--codemode-trace",
        ])
        .arg(scratch.file("trace.json"));
    if let Some(deno) = std::env::var_os("CODEMODE_TEST_DENO") {
        command.arg("--codemode-deno").arg(deno);
    }
    command
}

#[test]
#[ignore = "requires Pi 1.4.0; run `mise run pi-check`"]
fn pi_nested_calls_keep_pi_permissions_and_rust_scheduling() {
    let scratch = Scratch::new();
    let output = pi(&scratch, "integrations/pi/demo/workflow.ts", &["--print"])
        .arg("Run the codemode demo workflow.")
        .stdin(Stdio::null())
        .output()
        .expect("Pi starts");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains(r#""status":"success""#), "{stdout}");

    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "success");
    assert_eq!(
        trace["output"],
        json!({"title": "codemode Pi bridge fixture", "entries": 5,
               "refused": ["tool_error", "tool_denied"], "environment": "denied"})
    );
    let outcomes: Vec<_> = trace["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            (
                call["call"]["tool"].clone(),
                call["outcome"]["status"].clone(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            (json!("read"), json!("succeeded")),
            (json!("ls"), json!("succeeded")),
            (json!("bash"), json!("provider_failed")),
            (json!("write"), json!("rejected")),
        ]
    );
    assert!(
        trace["calls"][2]["outcome"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("requires approval")),
        "Pi's gate reason reaches the trace: {trace}"
    );

    // Pi's own gate saw the nested calls; the unauthorized write never reached Pi.
    let nested: Vec<_> = scratch
        .audit()
        .into_iter()
        .filter(|entry| entry["nested"] == true)
        .map(|entry| (entry["tool"].clone(), entry["decision"].clone()))
        .collect();
    assert_eq!(
        nested,
        [
            (json!("read"), json!("allowed")),
            (json!("ls"), json!("allowed")),
            (json!("bash"), json!("blocked")),
        ]
    );
    let everything = format!("{stdout}{trace}");
    assert!(!everything.contains(SECRET), "Pi's environment leaked");
}

fn read_until(
    lines: &mpsc::Receiver<Value>,
    deadline: Instant,
    mut done: impl FnMut(&Value) -> bool,
) -> Value {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = lines
            .recv_timeout(remaining)
            .expect("Pi emits the expected RPC event in time");
        if done(&event) {
            return event;
        }
    }
}

fn rpc_events(child: &mut Child) -> mpsc::Receiver<Value> {
    let stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if let Ok(event) = serde_json::from_str(&line) {
                if sender.send(event).is_err() {
                    return;
                }
            }
        }
    });
    receiver
}

#[test]
#[ignore = "requires Pi 1.4.0; run `mise run pi-check`"]
fn pi_abort_cancels_the_rust_run_and_the_nested_tool() {
    let scratch = Scratch::new();
    let mut child = pi(
        &scratch,
        "integrations/pi/demo/cancel.ts",
        &["--mode", "rpc"],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("Pi starts in RPC mode");
    let events = rpc_events(&mut child);
    let mut input = child.stdin.take().expect("stdin is piped");
    let deadline = Instant::now() + Duration::from_secs(20);
    writeln!(
        input,
        "{}",
        json!({"type":"prompt","message":"Run the cancel program."})
    )
    .expect("prompt is writable");
    read_until(&events, deadline, |event| {
        event["type"] == "tool_execution_start" && event["toolName"] == "demo_wait"
    });
    let aborted_at = Instant::now();
    writeln!(input, "{}", json!({"type":"abort"})).expect("abort is writable");
    read_until(&events, deadline, |event| {
        event["type"] == "tool_execution_end" && event["toolName"] == "rust_codemode"
    });
    let latency = aborted_at.elapsed();
    drop(input);
    let _ = child.kill();
    let _ = child.wait();

    let trace = scratch.json("trace.json");
    assert_eq!(trace["status"], "cancelled", "{trace}");
    assert_eq!(trace["calls"][0]["call"]["tool"], "demo_wait");
    assert_eq!(
        trace["calls"][0]["outcome"]["status"], "unknown",
        "a dispatched call without an observed result stays unknown"
    );
    assert!(
        scratch
            .audit()
            .iter()
            .any(|entry| entry["event"] == "aborted" && entry["tool"] == "demo_wait"),
        "Pi aborted the nested tool"
    );
    assert!(latency < Duration::from_secs(5), "abort took {latency:?}");
}
