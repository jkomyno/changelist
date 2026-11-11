//! Process-level contracts of the `codemode` binary, against the offline MCP
//! fixture server: signals, the worker's lifetime and environment, and a
//! memory limit reported by the CLI.
#![cfg(all(unix, feature = "testkit"))]

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use support::*;

const MIB: u64 = 1024 * 1024;

/// `codemode plan` of `program`, with a trace and `extra` options. An HTTP
/// server keeps the worker the only child process of `codemode`.
fn plan_command(scratch: &Scratch, program: &str, extra: &[&str]) -> Command {
    scratch.write("program.ts", program);
    let mut command = command(
        scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "program.ts",
            "--trace",
            "trace.json",
        ],
    );
    command.args(extra);
    command
}

fn signal(name: &str, pid: u32) {
    let status = Command::new("kill")
        .args([name, &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
}

/// SIGTERM, like Ctrl-C, cancels a running apply, which still records and
/// prints where every write stands.
#[test]
fn sigterm_cancels_an_apply_and_still_reports() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=delay-ms:30000"]);
    http_file(&scratch, &server);
    scratch.write(
        "append.json",
        &json!({"operations": [{
            "id": "append", "tool": "notes.append",
            "arguments": {"id": "log", "text": "line"},
        }]})
        .to_string(),
    );
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "append.json",
        ],
    );
    let plan = "append.plan.json";
    let digest = digest(&scratch, plan);

    let child = command(&scratch, &["apply", plan, "--approve", &digest, "--json"])
        .spawn()
        .expect("apply starts");
    // The attempt's intent is committed before the server is asked.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = status(&scratch, plan);
        if status["state"] == "running" && write(&status, "append")["attempts"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the attempt was never recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    signal("-TERM", child.id());
    let output = child.wait_with_output().expect("apply exits");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report = json_of(&stdout(&output));
    assert_eq!(report["detail"], "cancelled", "{report}");
    assert_eq!(
        standing(&report, "append"),
        ("no_reply".into(), "resolve".into()),
        "{report}"
    );
    assert_eq!(write(&report, "append")["error_code"], "cancelled");
    assert_eq!(status(&scratch, plan), report, "the end was recorded");
}

/// The CLI starts its worker from its own executable, without a PATH.
#[test]
fn a_program_runs_with_an_empty_environment() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let output = plan_command(
        &scratch,
        r#"const n: number = 42;
const { notes } = await tools.call("notes.list", {});
return { operations: [{ id: "put", tool: "notes.put",
  arguments: { id: "a", text: String(n + notes.length) } }] };"#,
        &[],
    )
    .env_clear()
    .output()
    .expect("plan starts");
    assert!(output.status.success(), "{output:?}");
    let trace = json_of(&scratch.read("trace.json"));
    assert_eq!(trace["backend"], "embedded_v8");
    assert_eq!(trace["status"], "success", "{trace}");
    let plan = json_of(&scratch.read("program.plan.json"));
    assert_eq!(plan["operations"][0]["arguments"]["text"], "42");
}

/// Killing `codemode` leaves no worker running, even one spinning on CPU.
#[test]
fn killing_codemode_does_not_leave_its_worker_running() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let mut child = plan_command(
        &scratch,
        r#"await tools.call("notes.list", {}); for (;;) {}"#,
        &["--timeout-ms", "20000"],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("plan starts");
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid: u32 = loop {
        let children = Command::new("pgrep")
            .args(["-P", &child.id().to_string()])
            .output()
            .expect("find worker");
        if children.status.success() {
            break String::from_utf8(children.stdout)
                .expect("worker pid")
                .trim()
                .parse()
                .expect("one direct worker child");
        }
        assert!(
            Instant::now() < deadline,
            "worker must exist before killing its parent"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    // Give the program time to make its call and start spinning.
    std::thread::sleep(Duration::from_millis(500));
    child.kill().expect("SIGKILL codemode");
    child.wait().expect("reap codemode");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("worker status");
        let state = String::from_utf8_lossy(&status.stdout);
        if !status.status.success() || state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker {pid} outlived its killed parent"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A program that fails writes no plan, and the command exits with 2.
#[test]
fn the_cli_reports_memory_limit() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let output = plan_command(
        &scratch,
        "const keep = []; for (;;) keep.push(new Array(1e5).fill(0));",
        &["--heap-mb", "64", "--timeout-ms", "20000"],
    )
    .output()
    .expect("plan starts");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("the program ended with status memory_limit"),
        "{output:?}"
    );
    let trace = json_of(&scratch.read("trace.json"));
    assert_eq!(trace["status"], "memory_limit", "{trace}");
    assert_eq!(trace["limits"]["heap_bytes"], 64 * MIB);
    assert!(
        !scratch.file("program.plan.json").exists(),
        "no plan is written"
    );
}
