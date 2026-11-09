//! Memory exhaustion and worker death end a run with an explicit status, through
//! real worker processes; the controller (this test process) survives each case.
#![cfg(all(feature = "testkit", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use codemode::{
    CallOutcome, MockProvider, RunCancellation, RunConfig, RunReport, RunStatus, WorkerExit, run,
};
use serde_json::json;

const MIB: u64 = 1024 * 1024;
/// Retains ever more heap objects after a slow call is dispatched.
const RETAIN_ARRAYS: &str = "
  const slow = tools.call('mock.wait', {ms: WAIT_MS, value: 'late'});
  await tools.call('mock.echo', 1);
  const keep = [];
  for (;;) keep.push(new Array(1e5).fill({x: keep.length}));
";

fn config(heap_mb: u64) -> RunConfig {
    RunConfig {
        worker: Some(env!("CARGO_BIN_EXE_codemode").into()),
        timeout_ms: 20_000,
        heap_mb,
        ..MockProvider::config()
    }
}

async fn execute(code: &str, config: RunConfig) -> RunReport {
    run(code, config, MockProvider, RunCancellation::default())
        .await
        .expect("the run produces a report")
}

fn finished_because(report: &RunReport, reason: &str) -> bool {
    report
        .events
        .iter()
        .any(|event| event.kind == "guest_finished" && event.reason.as_deref() == Some(reason))
}

/// A private directory for this test's scripts and markers.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("codemode-memory-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

/// An executable shell script standing in for a worker; it starts with an empty
/// environment, so it uses builtins and absolute paths only.
fn script(directory: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("worker.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("script is executable");
    path
}

async fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        assert!(Instant::now() < deadline, "{} appears", path.display());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn retained_arrays_past_the_heap_budget_end_with_memory_limit_and_keep_the_call_observed() {
    let report = execute(&RETAIN_ARRAYS.replace("WAIT_MS", "300"), config(64)).await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert!(finished_because(&report, "memory_limit"), "{report:?}");
    assert_eq!(report.limits.heap_bytes, Some(64 * MIB));
    assert!(report.diagnostic.is_some());
    // The controller keeps collecting the in-flight call after the worker is gone.
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded {
            value: json!("late")
        },
        "{report:?}"
    );
}

#[tokio::test]
async fn a_deadline_while_collecting_calls_keeps_memory_limit_and_leaves_the_call_unknown() {
    let report = execute(
        &RETAIN_ARRAYS.replace("WAIT_MS", "60000"),
        RunConfig {
            timeout_ms: 3_000,
            ..config(64)
        },
    )
    .await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert!(report.events.iter().any(|e| e.kind == "deadline_reached"));
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown, "{report:?}");
    let unknown = report
        .events
        .iter()
        .find(|event| event.kind == "unknown")
        .expect("an unknown event");
    assert_eq!(unknown.reason.as_deref(), Some("memory_limit"));
}

#[tokio::test]
async fn strings_grown_past_the_heap_budget_end_with_memory_limit() {
    let report = execute(
        "const keep = []; for (let i = 0;; i++) keep.push(String(i).repeat(1 << 22));",
        config(64),
    )
    .await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert_eq!(report.worker_exit, None);
}

#[tokio::test]
async fn the_report_states_the_enforced_limits() {
    let defaults = RunConfig {
        worker: Some(env!("CARGO_BIN_EXE_codemode").into()),
        ..MockProvider::config()
    };
    let report = execute("return 1;", defaults).await;
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.limits.heap_bytes, Some(256 * MIB));
    if cfg!(target_os = "linux") {
        assert_eq!(report.limits.process_data_bytes, Some(512 * MIB));
    } else {
        assert_eq!(report.limits.process_data_bytes, None);
    }
    let saved = serde_json::to_value(&report).expect("report serializes");
    assert_eq!(saved["schema_version"], 5);
    assert!(saved["limits"].get("process_data_bytes").is_some());
}

#[tokio::test]
async fn a_heap_budget_too_small_to_start_v8_is_refused() {
    let error = run(
        "return 1;",
        config(8),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect_err("V8 cannot start under 8 MiB reliably");
    assert!(
        matches!(&error, codemode::RunError::Config(message) if message.contains("16 MiB")),
        "{error}"
    );
}

#[tokio::test]
async fn reserved_exit_code_alone_is_memory_limit() {
    let directory = scratch("exit-code");
    let worker = script(&directory, "read -r frame\nexit 75");
    let report = execute(
        "return 1;",
        RunConfig {
            worker: Some(worker),
            ..config(64)
        },
    )
    .await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert_eq!(
        report.worker_exit,
        Some(WorkerExit {
            code: Some(75),
            signal: None
        })
    );
}

#[tokio::test]
async fn a_memory_limit_frame_racing_a_controller_stop_is_still_memory_limit() {
    let directory = scratch("frame-race");
    let marker = directory.join("written");
    let worker = script(
        &directory,
        &format!(
            "read -r frame\nprintf '%s\\n' '{{\"type\":\"memory_limit\",\"heap_limit_bytes\":1}}'\nprintf 'x\\n' > '{}'\nwhile read -r frame; do :; done",
            marker.display()
        ),
    );
    let cancellation = RunCancellation::default();
    let mut running = tokio::spawn(run(
        "return 1;",
        RunConfig {
            worker: Some(worker),
            ..config(64)
        },
        MockProvider,
        cancellation.clone(),
    ));
    // The controller may already have read the frame and stopped the worker;
    // otherwise it is cancelled with the frame unread.
    let report = tokio::select! {
        report = &mut running => report,
        _ = wait_for(&marker) => {
            cancellation.cancel();
            running.await
        }
    }
    .expect("run task joins")
    .expect("the run produces a report");
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
}

#[tokio::test]
async fn a_memory_limit_frame_unread_when_the_controller_stops_still_classifies_the_run() {
    let directory = scratch("frame-unread");
    // The controller stops at the excess call, before reading the frame after it.
    let worker = script(
        &directory,
        "read -r frame\nprintf '%s\\n%s\\n' '{\"type\":\"call\",\"id\":0,\"tool\":\"mock.echo\",\"arguments\":null}' '{\"type\":\"memory_limit\",\"heap_limit_bytes\":1}'\nwhile read -r frame; do :; done",
    );
    let report = execute(
        "return 1;",
        RunConfig {
            worker: Some(worker),
            max_calls: 0,
            ..config(64)
        },
    )
    .await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert!(matches!(
        report.calls[0].outcome,
        CallOutcome::Rejected { .. }
    ));
}

#[tokio::test]
async fn a_worker_killed_by_an_external_signal_is_worker_crashed() {
    let directory = scratch("external-kill");
    let pid_file = directory.join("pid");
    let worker = script(
        &directory,
        &format!(
            "printf '%s\\n' \"$$\" > '{}'\nexec '{}' --internal-embedded-guest",
            pid_file.display(),
            env!("CARGO_BIN_EXE_codemode")
        ),
    );
    let running = tokio::spawn(execute(
        "for (;;) {}",
        RunConfig {
            worker: Some(worker),
            ..config(64)
        },
    ));
    let pid = wait_for(&pid_file).await;
    // Let the exec complete before signalling the worker itself.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let killed = Command::new("/bin/kill")
        .args(["-KILL", pid.trim()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let report = running.await.expect("run task joins");
    assert_eq!(report.status, RunStatus::WorkerCrashed, "{report:?}");
    assert_eq!(
        report.worker_exit,
        Some(WorkerExit {
            code: None,
            signal: Some(9)
        })
    );
    assert!(finished_because(&report, "worker_crashed"), "{report:?}");
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn a_process_memory_limit_is_refused_before_a_worker_starts() {
    let directory = scratch("refused");
    let marker = directory.join("started");
    let worker = script(
        &directory,
        &format!("printf 'x\\n' > '{}'", marker.display()),
    );
    let error = run(
        "return 1;",
        RunConfig {
            worker: Some(worker),
            process_memory_mb: Some(512),
            ..config(64)
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect_err("this platform cannot enforce a process memory limit");
    assert!(
        matches!(&error, codemode::RunError::Config(message) if message.contains("Linux")),
        "{error}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!marker.exists(), "no worker started");

    let output = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--process-memory-mb", "512"])
        .stdin(Stdio::null())
        .output()
        .expect("codemode runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Linux"));
}

#[test]
fn the_bridge_reports_memory_limit_through_the_cli() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--heap-mb", "64", "--timeout-ms", "20000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bridge starts");
    let start = json!({"type":"start","version":2,"tools":[],
        "code":"const keep = []; for (;;) keep.push(new Array(1e5).fill(0));"});
    // Keep the input open: closing it cancels the run.
    let mut input = child.stdin.take().expect("bridge input");
    writeln!(input, "{start}").expect("start frame");
    let mut text = String::new();
    BufReader::new(child.stdout.take().expect("bridge output"))
        .read_line(&mut text)
        .expect("report frame");
    let report: serde_json::Value = serde_json::from_str(&text).expect("JSON report");
    assert_eq!(report["report"]["status"], "memory_limit", "{text}");
    assert_eq!(report["report"]["limits"]["heap_bytes"], 64 * MIB);
    drop(input);
    assert_eq!(child.wait().expect("bridge exits").code(), Some(1));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn array_buffers_past_the_linux_data_limit_end_with_memory_limit() {
    let code = "const keep = []; for (let i = 0; i < 40; i++) { const b = new ArrayBuffer(64 * 1024 * 1024); new Uint8Array(b).fill(1); keep.push(b); } return keep.length;";
    let report = execute(code, config(64)).await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert_eq!(report.limits.process_data_bytes, Some((64 + 256) * MIB));

    let report = execute(
        code,
        RunConfig {
            process_memory_mb: Some(512),
            ..config(64)
        },
    )
    .await;
    assert_eq!(report.status, RunStatus::MemoryLimit, "{report:?}");
    assert_eq!(report.limits.process_data_bytes, Some(512 * MIB));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_data_limit_cases_are_skipped_here() {
    eprintln!(
        "skipped: RLIMIT_DATA cases run only on Linux; this platform applies no process data limit"
    );
}
