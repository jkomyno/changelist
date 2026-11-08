//! Offline checks of the optional Composio host adapter. They run the adapter
//! under Deno with recorded fake Composio results; no account or network is used.
//! The live, read-only check is `mise run composio-live-check`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const SECRET: &str = "codemode-composio-secret-value";
const TOOLS: &str = "GITHUB_GET_THE_AUTHENTICATED_USER,SLACK_TEST_AUTH,GITHUB_GET_A_REPOSITORY,CODEMODE_DEMO_SLOW,CODEMODE_DEMO_CRASH";

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// An absolute Deno path: the host resolves executables itself.
fn deno() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEMODE_TEST_DENO") {
        return path.into();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("deno"))
        .find(|candidate| candidate.is_file())
        .expect("deno is on PATH")
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "codemode-composio-{}-{name}.json",
        std::process::id()
    ))
}

/// The offline host, with a host credential in its own environment only.
fn host(program: &str, trace: &Path) -> Command {
    let mut command = Command::new(deno());
    command
        .current_dir(root())
        .env("COMPOSIO_API_KEY", SECRET)
        .args([
            "run",
            "--allow-run",
            "--allow-read",
            "--allow-write",
            "--allow-env",
        ])
        .arg("integrations/composio/host.ts")
        .args(["--offline", "--program", program, "--tools", TOOLS])
        .args(["--bin", env!("CARGO_BIN_EXE_codemode")])
        .arg("--deno")
        .arg(deno())
        .arg("--trace")
        .arg(trace)
        .stdin(Stdio::null());
    command
}

fn summary(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "host prints one JSON summary; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn outcomes(trace: &Value) -> Vec<String> {
    trace["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            let outcome = &call["outcome"];
            let code = outcome["error"]["code"].as_str().unwrap_or("");
            format!(
                "{} {} {code}",
                call["call"]["tool"].as_str().unwrap_or(""),
                outcome["status"].as_str().unwrap_or("")
            )
            .trim_end()
            .to_owned()
        })
        .collect()
}

#[test]
fn composio_results_failures_and_denials_keep_their_origin_without_credentials() {
    let trace_path = scratch("workflow");
    let output = host("integrations/composio/demo/workflow.ts", &trace_path)
        .output()
        .expect("Deno runs the host");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = summary(&output);
    assert_eq!(summary["status"], "success");
    assert_eq!(
        summary["output"],
        json!({"environment":"denied","github":true,"refused":["composio_tool_failed","tool_denied"],"slack":true})
    );
    let text = std::fs::read_to_string(&trace_path).expect("trace written");
    let _ = std::fs::remove_file(&trace_path);
    assert!(
        !text.contains(SECRET),
        "the host credential never reaches the trace"
    );
    let trace: Value = serde_json::from_str(&text).expect("trace is JSON");
    assert_eq!(
        outcomes(&trace),
        [
            "GITHUB_GET_THE_AUTHENTICATED_USER succeeded",
            "SLACK_TEST_AUTH succeeded",
            "GITHUB_GET_A_REPOSITORY provider_failed composio_tool_failed",
            "TELEGRAM_GET_ME rejected tool_denied",
        ]
    );
    // Calls the host received carry its acknowledgment; the slug Rust rejected
    // never reached the host. The adapter cannot attest executor entry.
    let facts: Vec<(u64, &str)> = trace["provider_facts"]
        .as_array()
        .expect("provider facts")
        .iter()
        .map(|fact| {
            (
                fact["call_id"].as_u64().expect("id"),
                fact["kind"].as_str().expect("kind"),
            )
        })
        .collect();
    for id in 1..=3 {
        let kinds: Vec<&str> = facts
            .iter()
            .filter(|(call, _)| *call == id)
            .map(|(_, kind)| *kind)
            .collect();
        assert_eq!(kinds, ["received"], "call {id}");
    }
    assert!(facts.iter().all(|(call, _)| *call != 4));
    // The fake helper answers neither identity read until both have started, so
    // success already proves they overlapped; the trace must agree.
    let span = |index: usize, field: &str| trace["calls"][index][field].as_u64().expect(field);
    assert!(span(0, "started_ms") <= span(1, "finished_ms"));
    assert!(span(1, "started_ms") <= span(0, "finished_ms"));
}

/// Without concurrency the rendezvous cannot complete, so the proof above is not vacuous.
#[test]
fn sequential_identity_reads_fail_the_concurrency_rendezvous() {
    let trace_path = scratch("sequential");
    let output = host("integrations/composio/demo/workflow.ts", &trace_path)
        .args(["--concurrency", "1"])
        .output()
        .expect("Deno runs the host");
    let _ = std::fs::remove_file(&trace_path);
    assert_eq!(output.status.code(), Some(1));
    let summary = summary(&output);
    assert_eq!(summary["status"], "guest_error", "{summary}");
    assert!(
        summary["diagnostic"]
            .as_str()
            .is_some_and(|text| text.contains("without its partner")),
        "{summary}"
    );
}

/// Write a one-call program to a scratch file.
fn program(name: &str, tool: &str) -> PathBuf {
    let path = scratch(name).with_extension("ts");
    std::fs::write(&path, format!("return await tools.call('{tool}', {{}});"))
        .expect("program written");
    path
}

/// SIGTERM to the host's whole process group, as `composio run` forwards it,
/// still yields a report in which the unabortable call is unknown.
#[cfg(unix)]
#[test]
fn terminating_the_process_group_leaves_an_unabortable_composio_call_unknown() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;

    let trace_path = scratch("cancel");
    let program = program("cancel-program", "CODEMODE_DEMO_SLOW");
    let mut child = host(program.to_str().expect("UTF-8 path"), &trace_path)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Deno runs the host");
    // The fake helper reports when the call is in flight.
    let stderr = child.stderr.take().expect("stderr piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line == "started CODEMODE_DEMO_SLOW" {
                let _ = sender.send(());
            }
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the call starts");
    let started = Instant::now();
    let status = Command::new("kill")
        .args(["-TERM", &format!("-{}", child.id())])
        .status()
        .expect("kill runs");
    assert!(status.success());
    let output = child.wait_with_output().expect("host exits");
    let _ = std::fs::remove_file(&program);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation does not wait for the call"
    );
    assert_eq!(summary(&output)["status"], "cancelled");
    let trace: Value =
        serde_json::from_slice(&std::fs::read(&trace_path).expect("trace written")).expect("JSON");
    let _ = std::fs::remove_file(&trace_path);
    assert_eq!(outcomes(&trace), ["CODEMODE_DEMO_SLOW unknown"]);
}

#[test]
fn a_cli_that_ends_without_a_result_leaves_the_call_unknown() {
    let trace_path = scratch("crash");
    let program = program("crash-program", "CODEMODE_DEMO_CRASH");
    let output = host(program.to_str().expect("UTF-8 path"), &trace_path)
        .output()
        .expect("Deno runs the host");
    let _ = std::fs::remove_file(&program);
    assert_eq!(output.status.code(), Some(1));
    let summary = summary(&output);
    assert_eq!(summary["status"], "cancelled");
    assert!(
        summary["host_diagnostic"]
            .as_str()
            .is_some_and(|text| text.contains("without a result")),
        "{summary}"
    );
    let trace: Value =
        serde_json::from_slice(&std::fs::read(&trace_path).expect("trace written")).expect("JSON");
    let _ = std::fs::remove_file(&trace_path);
    assert_eq!(outcomes(&trace), ["CODEMODE_DEMO_CRASH unknown"]);
}

#[test]
fn sdk_executor_forwards_abort_signals_and_maps_results() {
    let output = Command::new(deno())
        .current_dir(root())
        .args(["run", "--allow-run", "--allow-read", "--allow-env"])
        .arg("integrations/composio/demo/sdk-check.ts")
        .arg(env!("CARGO_BIN_EXE_codemode"))
        .arg(deno())
        .stdin(Stdio::null())
        .output()
        .expect("Deno runs the check");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = summary(&output);
    assert_eq!(result["status"], "success");
    assert_eq!(
        result["output"],
        json!([{"login":"codemode-demo"}, "composio_tool_failed", "composio_sdk_refused", "invalid_arguments"])
    );
    let requests = result["requests"].as_array().expect("requests");
    assert_eq!(
        requests.len(),
        3,
        "non-object arguments never reach Composio"
    );
    assert_eq!(
        result["unobserved"],
        json!({"status":"cancelled","outcomes":["unknown"],"diagnostic":"call 1 (GITHUB_GET_THE_AUTHENTICATED_USER): Composio request ended without a result: socket hang up; run cancelled"})
    );
    assert_eq!(
        requests[0],
        json!({"slug":"GITHUB_GET_THE_AUTHENTICATED_USER","body":{"userId":"user-1","arguments":{},"version":"20261104_00"},"signal":true})
    );
}
