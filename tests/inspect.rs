//! Inspection answers, their bounds, and what they leave out.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use codemode::{Evidence, InspectOptions, RunReport, Sections, inspect};
#[cfg(feature = "testkit")]
use codemode::{MockProvider, RunCancellation, RunConfig, RunStatus, run};
use serde_json::{Value, json};

const SECRET: &str = "codemode-inspect-secret-value";

#[cfg(feature = "testkit")]
fn config() -> RunConfig {
    RunConfig {
        deno: std::env::var_os("CODEMODE_TEST_DENO").map_or_else(|| "deno".into(), Into::into),
        timeout_ms: 5_000,
        ..MockProvider::config()
    }
}

/// A program run with the mock tools, saved as a host would save its trace.
#[cfg(feature = "testkit")]
async fn saved_run(name: &str, source: &str, config: RunConfig) -> (RunReport, std::path::PathBuf) {
    let report = run(source, config, MockProvider, RunCancellation::default())
        .await
        .expect("run completes");
    let trace = std::env::temp_dir().join(format!(
        "codemode-inspect-{name}-{}.json",
        std::process::id()
    ));
    std::fs::write(
        &trace,
        serde_json::to_vec(&report).expect("report serializes"),
    )
    .expect("trace written");
    (report, trace)
}

fn phase(report: &RunReport, name: &str) -> u64 {
    inspect(report, &InspectOptions::default())
        .timing
        .phases
        .iter()
        .find(|phase| phase.name == name)
        .map(|phase| phase.ms)
        .expect("phase exists")
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn why_slow_attributes_waiting_to_the_concurrency_cap() {
    let mut options = config();
    options.max_concurrency = 1;
    let report = run(
        "return await Promise.all([1,2,3].map(value => tools.call('mock.wait', {ms: 60, value})));",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    let inspection = inspect(&report, &InspectOptions::default());
    let total: u64 = inspection.timing.phases.iter().map(|phase| phase.ms).sum();
    assert_eq!(
        total, report.elapsed_ms,
        "phases are exclusive and complete"
    );
    // Two calls wait behind the single slot for about 120 ms.
    assert!(phase(&report, "queued_at_cap") >= 110, "{inspection:?}");
    assert!(phase(&report, "startup") > 0);
    assert_eq!(inspection.timing.slowest.len(), 3);
    let text = inspection.render(Sections::ALL);
    assert!(text.contains("concurrency cap of 1"), "{text}");
}

fn clock() -> Value {
    json!({"origin": "run_start", "source": "rust_monotonic", "unit": "ms_floor"})
}

fn bytes() -> Value {
    json!({"from_guest": 0, "to_guest": 0, "retained": 0, "omitted": 0})
}

/// A report with one call per evidence class, built as a host would see it.
fn evidence_report() -> RunReport {
    let call = |id: u64, outcome: Value, started: Option<u64>| {
        json!({"call": {"id": id, "tool": "store.write", "arguments": {"key": SECRET, "value": 1}},
               "guest_id": id - 1, "outcome": outcome, "admitted_ms": 10, "started_ms": started,
               "finished_ms": 300, "argument_bytes": 40})
    };
    let fact = |id: u64, kind: &str, at: u64| json!({"call_id": id, "kind": kind, "received_ms": at, "provider_ms": 1.5});
    serde_json::from_value(json!({
        "schema_version": 3, "backend": "deno", "code": "...", "status": "deadline",
        "limits": {"timeout_ms": 300, "max_calls": 32, "max_concurrency": 4, "max_output_bytes": 1000},
        "calls": [
            call(1, json!({"status": "unknown"}), Some(11)),
            call(2, json!({"status": "unknown"}), Some(11)),
            call(3, json!({"status": "unknown"}), Some(11)),
            call(4, json!({"status": "provider_failed", "error": {"code": "permission_denied", "message": SECRET}}), Some(11)),
            call(5, json!({"status": "cancelled_before_dispatch"}), None),
            call(6, json!({"status": "rejected", "error": {"code": "tool_denied", "message": "no"}}), None),
        ],
        "events": [
            {"id": 1, "call_id": null, "kind": "guest_spawned", "at_ms": 0},
            {"id": 2, "call_id": null, "kind": "deadline_reached", "at_ms": 300},
            {"id": 3, "call_id": 1, "kind": "unknown", "at_ms": 300, "reason": "deadline"},
        ],
        "provider_facts": [
            fact(1, "received", 12), fact(1, "executor_started", 20),
            fact(2, "received", 12),
            fact(4, "received", 12), fact(4, "permission_requested", 13), fact(4, "permission_denied", 40),
        ],
        "provider_facts_dropped": 0, "elapsed_ms": 302, "clock": clock(), "bytes": bytes(),
        "diagnostic": SECRET
    }))
    .expect("report deserializes")
}

#[test]
fn reconciliation_lists_only_uncertain_calls_with_their_strongest_evidence() {
    let inspection = inspect(&evidence_report(), &InspectOptions::default());
    let classes: Vec<Evidence> = inspection
        .calls
        .rows
        .iter()
        .map(|row| row.evidence)
        .collect();
    assert_eq!(
        classes,
        [
            Evidence::UnknownAfterExecutorStart,
            Evidence::UnknownAfterReceipt,
            Evidence::UnknownUnacknowledged,
            Evidence::DeniedByProvider,
            Evidence::NotDispatched,
            Evidence::Rejected,
        ]
    );
    let items: Vec<(u64, Evidence)> = inspection
        .reconcile
        .items
        .iter()
        .map(|item| (item.id, item.evidence))
        .collect();
    assert_eq!(
        items,
        [
            (1, Evidence::UnknownAfterExecutorStart),
            (2, Evidence::UnknownAfterReceipt),
            (3, Evidence::UnknownUnacknowledged),
        ]
    );
    assert_eq!(
        inspection.reconcile.items[0].reason.as_deref(),
        Some("deadline")
    );
    assert_eq!(inspection.timing.permission_wait_ms, 27);
    let text = inspection.render(Sections::ALL);
    assert!(
        text.contains("missing evidence is not proof it did not run"),
        "{text}"
    );
    assert!(!text.contains(SECRET), "{text}");
}

fn codemode(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI starts");
    let mut input = child.stdin.take().expect("stdin piped");
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).expect("stdin writable");
    }
    drop(input);
    child.wait_with_output().expect("CLI finishes")
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn inspection_keeps_values_and_messages_out_of_its_output() {
    let source = format!(
        "await tools.call('mock.echo', {{token: '{SECRET}'}});
         try {{ await tools.call('mock.fail', {{message: '{SECRET}'}}); }} catch {{}}
         try {{ await tools.call('evil\\u001b[31m', {{}}); }} catch {{}}
         throw new Error('{SECRET}');"
    );
    let (report, trace) = saved_run("values", &source, config()).await;
    assert_eq!(report.status, RunStatus::GuestError, "{report:?}");
    let saved = std::fs::read_to_string(&trace).expect("trace written");
    assert!(saved.contains(SECRET), "the full record keeps task data");

    let path = trace.to_str().expect("UTF-8 path");
    for args in [vec!["inspect", path], vec!["inspect", path, "--json"]] {
        let output = codemode(&args, None);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).expect("UTF-8");
        assert!(!text.contains(SECRET), "{args:?}: {text}");
        assert!(!text.contains('\u{1b}'), "control characters are replaced");
        assert!(
            text.contains("mock_failure") && text.contains("tool_denied"),
            "{text}"
        );
    }
    // Messages are opt-in.
    let output = codemode(&["inspect", path, "--messages"], None);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    let _ = std::fs::remove_file(&trace);
    assert!(text.contains(SECRET), "{text}");
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn inspection_output_is_bounded_and_counts_what_it_omits() {
    let (report, trace) = saved_run(
        "bound",
        "return (await Promise.all(Array.from({length: 30}, (_, i) => tools.call('mock.echo', i)))).length;",
        RunConfig {
            max_calls: 64,
            ..config()
        },
    )
    .await;
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    let output = codemode(
        &[
            "inspect",
            trace.to_str().expect("UTF-8 path"),
            "--calls",
            "--limit",
            "3",
        ],
        None,
    );
    let _ = std::fs::remove_file(&trace);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(text.contains("30 × ran: result observed"), "{text}");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("  #")).count(),
        3,
        "{text}"
    );
    assert!(text.contains("… 27 more calls"), "{text}");
    assert!(
        !text.contains("Why was this slow?"),
        "only the requested section"
    );
}

#[test]
fn inspect_reads_a_record_from_stdin_and_refuses_other_json() {
    let report = serde_json::to_string(&evidence_report()).expect("serializes");
    let output = codemode(&["inspect", "-", "--reconcile"], Some(&report));
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("#3 store.write"));
    let refused = codemode(&["inspect", "-"], Some("{\"type\":\"report\"}"));
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not an execution record"));
}

fn minimal(status: &str, elapsed: u64, calls: Value, events: Value) -> RunReport {
    serde_json::from_value(json!({
        "schema_version": 3, "backend": "deno", "code": "", "status": status,
        "limits": {"timeout_ms": 10, "max_calls": 32, "max_concurrency": 4, "max_output_bytes": 1000},
        "calls": calls, "events": events, "provider_facts": [], "provider_facts_dropped": 0,
        "elapsed_ms": elapsed, "clock": clock(), "bytes": bytes(), "diagnostic": null
    }))
    .expect("report deserializes")
}

#[test]
fn a_run_stopped_before_any_guest_frame_counts_the_rest_as_shutdown() {
    let report = minimal(
        "deadline",
        12,
        json!([]),
        json!([{"id": 1, "call_id": null, "kind": "deadline_reached", "at_ms": 10}]),
    );
    assert_eq!(phase(&report, "startup"), 10);
    assert_eq!(phase(&report, "shutdown"), 2);
}

#[test]
fn hostile_reports_cannot_panic_or_inject_terminal_control() {
    let call = json!([{"call": {"id": 1, "tool": "t", "arguments": {}}, "guest_id": 0,
        "outcome": {"status": "unknown"}, "admitted_ms": 0, "started_ms": 1, "finished_ms": u64::MAX,
        "argument_bytes": 2}]);
    let events = json!([{"id": 1, "call_id": 1, "kind": "unknown", "at_ms": 1,
        "reason": format!("\u{1b}]52;c;{}\u{7}", "z".repeat(10_000))}]);
    let report = minimal("deadline", u64::MAX, call, events);
    let text = inspect(&report, &InspectOptions::default()).render(Sections::ALL);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text}"
    );
    assert!(text.len() < 4_000, "reasons are truncated");
}

#[test]
fn a_zero_limit_still_says_reconciliation_is_needed() {
    let options = InspectOptions {
        limit: 0,
        messages: false,
    };
    let text = inspect(&evidence_report(), &options).render(Sections {
        timing: false,
        calls: false,
        reconcile: true,
    });
    assert!(!text.contains("none:"), "{text}");
    assert!(text.contains("… 3 more uncertain calls"), "{text}");
}

#[test]
fn an_omitted_provider_error_is_not_reported_as_a_run_tool() {
    let call = json!([{"call": {"id": 1, "tool": "t", "arguments": {}}, "guest_id": 0,
        "outcome": {"status": "result_too_large"}, "admitted_ms": 0, "started_ms": 1, "finished_ms": 2,
        "argument_bytes": 2}]);
    let events = json!([{"id": 1, "call_id": 1, "kind": "provider_failed", "at_ms": 2}]);
    let inspection = inspect(
        &minimal("output_limit", 3, call, events),
        &InspectOptions::default(),
    );
    assert_eq!(inspection.calls.rows[0].evidence, Evidence::ErrorOmitted);
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn native_inspection_has_exclusive_host_phases_and_roundtrips_without_guest_evidence() {
    let calls = [20, 20].map(|ms| codemode::NativeCall {
        tool: "mock.wait".into(),
        arguments: json!({"ms": ms}),
    });
    let report = codemode::run_calls(
        &calls,
        RunConfig {
            max_concurrency: 1,
            deno: "/nonexistent/native-inspect-deno".into(),
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("native run completes");
    let saved = serde_json::to_value(&report).expect("native report serializes");
    assert_eq!(saved["schema_version"], 3);
    assert_eq!(saved["backend"], "native");
    assert!(saved.get("output").is_none());
    assert!(
        saved["calls"]
            .as_array()
            .expect("calls array")
            .iter()
            .all(|call| call.get("guest_id").is_none())
    );
    let restored: RunReport = serde_json::from_value(saved).expect("native report deserializes");
    let inspection = inspect(&restored, &InspectOptions::default());
    assert_eq!(inspection.backend, codemode::ExecutionBackend::Native);
    assert_eq!(
        inspection
            .timing
            .phases
            .iter()
            .map(|phase| phase.ms)
            .sum::<u64>(),
        report.elapsed_ms
    );
    assert!(phase(&report, "queued_at_cap") > 0);
    assert!(
        inspection
            .timing
            .phases
            .iter()
            .all(|phase| !phase.meaning.contains("guest")
                && phase.name != "startup"
                && phase.name != "draining")
    );
    let text = inspection.render(Sections::ALL);
    assert!(text.contains("backend native"), "{text}");
    let output = codemode(
        &["inspect", "-", "--json"],
        Some(&serde_json::to_string(&restored).expect("report serializes")),
    );
    assert!(output.status.success(), "{output:?}");
    let inspected: Value = serde_json::from_slice(&output.stdout).expect("inspection JSON");
    assert_eq!(inspected["backend"], "native");
}

#[test]
fn saved_reports_preserve_numeric_guest_tokens() {
    let report = evidence_report();
    assert_eq!(report.calls[0].guest_id, Some(0));
    let saved = serde_json::to_value(&report).expect("report serializes");
    assert_eq!(saved["calls"][0]["guest_id"], 0);
    assert!(
        inspect(&report, &InspectOptions::default())
            .timing
            .phases
            .iter()
            .any(|phase| phase.name == "guest")
    );
}
