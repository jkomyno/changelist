//! Machine-readable CLI output and trace-file contracts.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

static TRACE_ID: AtomicU64 = AtomicU64::new(0);

fn invoke(source: &str, extra: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["run", "--code", "-"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(source.as_bytes())
        .expect("source is writable");
    child.wait_with_output().expect("CLI finishes")
}

#[test]
fn stdin_execution_returns_one_json_record_and_a_failure_exit_status() {
    let output = invoke(
        "await tools.call('mock.echo', {done:true}); await tools.call('mock.fail', {message:'expected'});",
        &[],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout is one JSON object");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "guest_error");
    assert!(report.get("output").is_none());
    assert_eq!(report["calls"].as_array().expect("call records").len(), 2);
    assert_eq!(report["calls"][0]["outcome"]["status"], "succeeded");
    assert_eq!(report["calls"][1]["outcome"]["status"], "provider_failed");
}

struct TraceFile(PathBuf);

impl Drop for TraceFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn trace_file_keeps_execution_evidence_out_of_the_compact_stdout_result() {
    let path = std::env::temp_dir().join(format!(
        "codemode-cli-{}-{}.json",
        std::process::id(),
        TRACE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("reserve a new test-owned trace path");
    let trace = TraceFile(path);
    let source = "const n: number = 42; return await tools.call('mock.echo', {answer:n});";
    let output = invoke(
        source,
        &["--trace", trace.0.to_str().expect("test path is UTF-8")],
    );
    assert!(output.status.success(), "{output:?}");
    let summary: Value = serde_json::from_slice(&output.stdout).expect("compact JSON stdout");
    assert_eq!(summary["status"], "success");
    assert_eq!(summary["output"], json!({"answer":42}));
    assert!(summary.get("code").is_none());
    assert!(summary.get("calls").is_none());
    let report: Value = serde_json::from_slice(&fs::read(&trace.0).expect("trace exists"))
        .expect("full trace is JSON");
    assert_eq!(report["code"], source);
    assert_eq!(report["output"], summary["output"]);
    assert_eq!(report["calls"][0]["call"]["tool"], "mock.echo");

    for (source, expected) in [
        ("return null;", Some(Value::Null)),
        ("throw new Error('no output');", None),
    ] {
        let output = invoke(
            source,
            &["--trace", trace.0.to_str().expect("test path is UTF-8")],
        );
        let summary: Value = serde_json::from_slice(&output.stdout).expect("compact JSON stdout");
        let report: Value =
            serde_json::from_slice(&fs::read(&trace.0).expect("trace exists")).expect("JSON trace");
        assert_eq!(summary.get("output"), expected.as_ref());
        assert_eq!(report.get("output"), expected.as_ref());
    }
}

#[test]
fn bridge_subcommand_speaks_the_host_protocol_over_process_stdio() {
    use std::io::{BufRead, BufReader};

    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--timeout-ms", "5000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI starts");
    let mut input = child.stdin.take().expect("stdin is piped");
    let mut output = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    let start = json!({"type":"start","version":1,"tools":["host.add"],
        "code":"return await tools.call('host.add', {a: 2, b: 40});"});
    writeln!(input, "{start}").expect("start frame is writable");
    output.read_line(&mut line).expect("call frame is readable");
    let call: Value = serde_json::from_str(&line).expect("call frame is JSON");
    assert_eq!(
        call,
        json!({"type":"call","id":1,"tool":"host.add","arguments":{"a":2,"b":40}})
    );
    writeln!(
        input,
        "{}",
        json!({"type":"result","id":1,"ok":true,"value":42})
    )
    .expect("result is writable");
    line.clear();
    output
        .read_line(&mut line)
        .expect("report frame is readable");
    let frame: Value = serde_json::from_str(&line).expect("report frame is JSON");
    assert_eq!(frame["type"], "report");
    assert_eq!(frame["report"]["output"], 42);
    // The bridge exits after its report even though the host keeps stdin open.
    let status = child.wait().expect("bridge exits");
    assert!(status.success());
    drop(input);

    let output = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--code", "x.ts"])
        .output()
        .expect("CLI runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not accepted by bridge"));
}
