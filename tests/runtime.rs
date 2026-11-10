//! Public runtime contracts exercised through the real worker subprocess.
#![cfg(feature = "testkit")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use codemode::{
    CallContext, CallOutcome, ExecutionBackend, MockProvider, ProviderError, RunCancellation,
    RunConfig, RunStatus, ToolCall, ToolProvider, run,
};
use serde_json::{Value, json};

fn config() -> RunConfig {
    RunConfig {
        worker: Some(env!("CARGO_BIN_EXE_codemode").into()),
        timeout_ms: 5_000,
        ..MockProvider::config()
    }
}

/// A stand-in worker that writes the stdout frame named by its program, then
/// waits for input to close, as a compromised guest could.
#[cfg(unix)]
fn forging_worker() -> RunConfig {
    static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
    let script = SCRIPT.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "codemode-forging-worker-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("script directory");
        let draft = directory.join("worker.sh.draft");
        // Builtins only: the worker starts with an empty environment.
        std::fs::write(
            &draft,
            r#"#!/bin/sh
read -r frame
case "$frame" in
  *forged-event*) printf '%s\n' '{"type":"event","id":77,"kind":"succeeded"}' ;;
  *duplicate-token*)
    line='{"type":"call","id":44,"tool":"mock.wait","arguments":{"ms":1000}}'
    printf '%s\n%s\n' "$line" "$line" ;;
  *unsafe-token*) printf '%s\n' '{"type":"call","id":9007199254740993,"tool":"mock.echo","arguments":null}' ;;
  *oversized-line*) printf '%70000s' x ;;
esac
while read -r frame; do :; done
"#,
        )
        .expect("script written");
        std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o755))
            .expect("script is executable");
        // Copied into place by a child process so no fork on another test
        // thread can still hold a write descriptor on the file it later execs
        // (Linux refuses with ETXTBSY).
        let path = directory.join("worker.sh");
        let copied = std::process::Command::new("/bin/cp")
            .arg("-p")
            .arg(&draft)
            .arg(&path)
            .status()
            .expect("cp starts");
        assert!(copied.success(), "script copied into place");
        path
    });
    RunConfig {
        worker: Some(script.clone()),
        ..config()
    }
}

/// A real worker started through a link unique to `name`, so a test can find its
/// process by command line while other tests run their own workers.
#[cfg(unix)]
fn traceable_worker(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("codemode-worker-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("link directory");
    let link = directory.join("codemode");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_codemode"), &link).expect("worker link");
    link
}

/// Process ids whose command line starts with `worker`.
#[cfg(unix)]
fn worker_pids(worker: &Path) -> Vec<u32> {
    let output = std::process::Command::new("pgrep")
        .args(["-f", &format!("^{} ", worker.display())])
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[tokio::test]
async fn typescript_and_concurrent_tools_return_json() {
    let report = run(
        "const n: number = 20; return await tools.call('mock.echo', {answer: n + 22});",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("the worker executes TypeScript");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!({"answer":42})));
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].call.id, 1);
    assert!(matches!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { .. }
    ));
    assert!(
        report
            .events
            .iter()
            .any(|event| event.kind == "dispatched" && event.call_id == Some(1))
    );
}

#[tokio::test]
async fn concurrency_refills_before_slowest_call_finishes() {
    let mut options = config();
    options.max_concurrency = 2;
    let release = Arc::new(tokio::sync::Notify::new());
    let report = run(
        "return await Promise.all([0,1,2,3].map(value => tools.call('mock.echo', value)));",
        options,
        RollingProvider(release),
        RunCancellation::default(),
    )
    .await
    .expect("concurrent run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!([0, 1, 2, 3])));
    let third_dispatch = report
        .events
        .iter()
        .position(|e| e.call_id == Some(3) && e.kind == "dispatched")
        .expect("third call dispatched");
    let first_success = report
        .events
        .iter()
        .position(|e| e.call_id == Some(1) && e.kind == "succeeded")
        .expect("first call completed");
    assert!(
        third_dispatch < first_success,
        "a free slot must refill before the held call completes"
    );
    let mut active = 0;
    let mut peak = 0;
    for event in &report.events {
        match event.kind.as_str() {
            "dispatched" => {
                active += 1;
                peak = peak.max(active);
            }
            "succeeded" | "provider_failed" => active -= 1,
            _ => {}
        }
    }
    assert_eq!(peak, 2, "host events prove the concurrency cap");
    assert_eq!(active, 0);
}

struct RollingProvider(Arc<tokio::sync::Notify>);
impl ToolProvider for RollingProvider {
    async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        if call.arguments == json!(0) {
            self.0.notified().await;
        }
        if call.arguments == json!(2) {
            self.0.notify_one();
        }
        Ok(call.arguments)
    }
}

#[tokio::test]
async fn uncloneable_arguments_reject_only_their_own_call() {
    let report = run(
        "const results = await Promise.allSettled([tools.call('mock.echo', 1), tools.call('mock.echo', () => 2), tools.call(() => 3, null), tools.call(4, null)]); return results.map(r => r.status);",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(
        report.output,
        Some(json!(["fulfilled", "rejected", "rejected", "rejected"]))
    );
    assert_eq!(report.calls.len(), 1, "rejected calls never reach the host");
}

#[tokio::test]
async fn a_call_submitted_as_the_program_settles_reaches_the_host() {
    let report = run(
        "queueMicrotask(() => { void tools.call('mock.echo', 'late'); }); return 'done';",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.calls.len(), 1, "{report:?}");
    assert!(matches!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { .. }
    ));
}

#[tokio::test]
async fn exhausted_call_budget_distinguishes_queued_and_running_calls() {
    let mut options = config();
    options.max_calls = 2;
    options.max_concurrency = 1;
    let report = run(
        "return await Promise.all([1,2,3,4,5].map(value => tools.call('mock.wait', {ms:1000,value})));",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("call limit produces a report");
    assert_eq!(report.status, RunStatus::CallLimit, "{report:?}");
    // The first request beyond the allowance is recorded; later ones are not, so
    // a report holds at most max_calls + 1 calls.
    assert_eq!(report.calls.len(), 3, "{report:?}");
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    assert_eq!(
        report.calls[1].outcome,
        CallOutcome::CancelledBeforeDispatch
    );
    let CallOutcome::Rejected { error } = &report.calls[2].outcome else {
        panic!("over-allowance call is rejected: {:?}", report.calls[2]);
    };
    assert_eq!(error.code, "call_limit");
    assert_eq!(report.calls[2].call.arguments, json!({"ms":1000,"value":3}));
    assert_eq!(report.calls[2].started_ms, None);
    let kinds: Vec<&str> = report
        .events
        .iter()
        .filter(|event| event.call_id == Some(3))
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(kinds, ["admitted", "rejected"]);
}

#[cfg(unix)]
#[tokio::test]
async fn deadline_terminates_a_cpu_loop_and_its_worker() {
    let worker = traceable_worker("cpu-loop");
    let mut options = config();
    options.timeout_ms = 300;
    options.worker = Some(worker.clone());
    let report = tokio::time::timeout(
        Duration::from_secs(3),
        run(
            "while (true) {}",
            options,
            MockProvider,
            RunCancellation::default(),
        ),
    )
    .await
    .expect("host deadline remains responsive")
    .expect("deadline produces a report");
    assert_eq!(report.status, RunStatus::Deadline, "{report:?}");
    assert!(
        report
            .events
            .iter()
            .any(|event| event.kind == "guest_terminated")
    );
    assert!(
        worker_pids(&worker).is_empty(),
        "the worker outlived its run"
    );
}

#[derive(Clone)]
struct ObserveStart {
    started: Arc<tokio::sync::Notify>,
}
impl ToolProvider for ObserveStart {
    async fn execute(&self, _: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        self.started.notify_one();
        context.cancellation.cancelled().await;
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn cancellation_preserves_uncertain_dispatched_effects() {
    let cancellation = RunCancellation::default();
    let started = Arc::new(tokio::sync::Notify::new());
    let provider = ObserveStart {
        started: Arc::clone(&started),
    };
    let signal = cancellation.clone();
    let cancel = async move {
        started.notified().await;
        signal.cancel();
    };
    let (report, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            run(
                "return await tools.call('mock.echo', 1);",
                config(),
                provider,
                cancellation,
            ),
            cancel
        )
    })
    .await
    .expect("provider starts and cancellation completes");
    let report = report.expect("cancellation produces a report");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
}

#[derive(Clone)]
struct CountDispatches(Arc<AtomicUsize>);
impl ToolProvider for CountDispatches {
    async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(call.arguments)
    }
}

#[tokio::test]
async fn denied_tools_never_reach_provider() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut options = config();
    options.allowed_tools = vec!["mock.wait".into()];
    let report = run(
        "const errors=[]; for (const fn of [() => tools.call('mock.echo', 1), () => tools.call('mock.fail', {})]) { try { await fn(); } catch (e) { errors.push(e.code); } } return errors;",
        options, CountDispatches(Arc::clone(&dispatches)), RunCancellation::default(),
    ).await.expect("caught host validation errors return normally");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!(["tool_denied", "tool_denied"])));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert!(
        report
            .calls
            .iter()
            .all(|call| matches!(call.outcome, CallOutcome::Rejected { .. }))
    );
}

#[tokio::test]
async fn provider_failure_is_catchable_and_retains_origin() {
    let report = run(
        "try { await tools.call('mock.fail', {message:'expected'}); } catch (e) { return {code:e.code,message:e.message}; }",
        config(), MockProvider, RunCancellation::default(),
    ).await.expect("caught provider failure returns normally");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(
        report.output,
        Some(json!({"code":"mock_failure","message":"expected"}))
    );
    assert!(
        matches!(&report.calls[0].outcome, CallOutcome::ProviderFailed { error } if error.code == "mock_failure")
    );
}

#[tokio::test]
async fn final_output_and_provider_response_share_a_byte_budget() {
    let mut options = config();
    options.max_output_bytes = 80;
    let report = run(
        "await tools.call('mock.echo', 'ok'); return 'x'.repeat(50);",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("output limit produces a report");
    assert_eq!(report.status, RunStatus::OutputLimit, "{report:?}");
    assert_eq!(report.output, None);
    assert!(matches!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { .. }
    ));
}

#[tokio::test]
async fn oversized_provider_results_are_omitted_from_report() {
    let mut options = config();
    options.max_output_bytes = 80;
    let report = run(
        "return await tools.call('mock.echo', 'x'.repeat(1000));",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("response limit produces a report");
    assert_eq!(report.status, RunStatus::OutputLimit, "{report:?}");
    assert_eq!(report.calls[0].outcome, CallOutcome::ResultTooLarge);
}

#[cfg(unix)]
#[tokio::test]
async fn forged_trace_frames_are_rejected_without_dispatch() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let report = run(
        "forged-event",
        forging_worker(),
        CountDispatches(Arc::clone(&dispatches)),
        RunCancellation::default(),
    )
    .await
    .expect("invalid protocol produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert!(report.calls.is_empty());
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert!(!report.events.iter().any(|event| event.kind == "succeeded"));
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_guest_tokens_cannot_reuse_host_identity() {
    let report = run(
        "duplicate-token",
        forging_worker(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("duplicate correlation produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert_eq!(
        report.diagnostic.as_deref(),
        Some("duplicate guest correlation token")
    );
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].call.id, 1);
    assert_eq!(report.calls[0].guest_id, Some(44));
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_javascript_tokens_never_reach_provider() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let report = run(
        "unsafe-token",
        forging_worker(),
        CountDispatches(Arc::clone(&dispatches)),
        RunCancellation::default(),
    )
    .await
    .expect("invalid correlation produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert_eq!(
        report.diagnostic.as_deref(),
        Some("guest correlation token exceeds the JavaScript safe integer range")
    );
    assert!(report.calls.is_empty());
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}

struct PanicProvider;
impl ToolProvider for PanicProvider {
    async fn execute(&self, _: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        panic!("test provider panic");
    }
}

#[tokio::test]
async fn provider_task_failure_keeps_origin_and_call_identity() {
    let report = run(
        "return await tools.call('mock.echo', null);",
        config(),
        PanicProvider,
        RunCancellation::default(),
    )
    .await
    .expect("provider panic produces a report");
    assert_eq!(
        serde_json::to_value(report.status).expect("status serializes"),
        "provider_task_error"
    );
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    assert!(
        report
            .events
            .iter()
            .any(|e| e.kind == "provider_task_failed" && e.call_id == Some(1))
    );
}

/// Kills the run's worker while its call runs, then answers or never does.
#[cfg(unix)]
struct KillWorker {
    worker: PathBuf,
    answer: bool,
}
#[cfg(unix)]
impl ToolProvider for KillWorker {
    async fn execute(&self, _: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        let pids = worker_pids(&self.worker);
        assert_eq!(pids.len(), 1, "one running worker: {pids:?}");
        let status = tokio::process::Command::new("/bin/kill")
            .args(["-KILL", &pids[0].to_string()])
            .status()
            .await
            .expect("terminate this test's worker");
        assert!(status.success());
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !self.answer {
            std::future::pending::<()>().await;
        }
        Ok(json!("observed after worker exit"))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn abrupt_worker_exit_keeps_a_later_result_and_otherwise_unknown() {
    let worker = traceable_worker("abrupt-exit-observed");
    let report = run(
        "return await tools.call('mock.echo', {});",
        RunConfig {
            worker: Some(worker.clone()),
            ..config()
        },
        KillWorker {
            worker,
            answer: true,
        },
        RunCancellation::default(),
    )
    .await
    .expect("worker exit produces a report");
    assert_eq!(report.status, RunStatus::WorkerCrashed, "{report:?}");
    assert_eq!(
        report.worker_exit,
        Some(codemode::WorkerExit {
            code: None,
            signal: Some(9)
        })
    );
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded {
            value: json!("observed after worker exit")
        }
    );

    let worker = traceable_worker("abrupt-exit-unknown");
    let report = run(
        "return await tools.call('mock.echo', {});",
        RunConfig {
            worker: Some(worker.clone()),
            timeout_ms: 1_000,
            ..config()
        },
        KillWorker {
            worker,
            answer: false,
        },
        RunCancellation::default(),
    )
    .await
    .expect("worker exit produces a report");
    // The deadline that cut collection short does not hide why the worker ended.
    assert_eq!(report.status, RunStatus::WorkerCrashed, "{report:?}");
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
}

#[tokio::test]
async fn returned_null_is_distinct_from_absent_output_in_json() {
    let returned = run(
        "return null;",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("null return succeeds");
    let absent = run(
        "throw new Error('no output');",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("guest error returns report");
    let returned = serde_json::to_value(returned).expect("report serializes");
    let absent = serde_json::to_value(absent).expect("report serializes");
    assert_eq!(returned.get("output"), Some(&Value::Null));
    assert!(absent.get("output").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn stdout_frame_size_is_bounded_without_a_newline() {
    let report = run(
        "oversized-line",
        forging_worker(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("oversized frame produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert_eq!(
        report.diagnostic.as_deref(),
        Some("frame exceeds 65536 bytes")
    );
}

#[tokio::test]
async fn typed_composition_runs_without_host_capabilities_or_imports() {
    let report = run(
        r#"
        const values: number[] = await Promise.all([1, 2, 3].map(n => tools.call('mock.echo', n)));
        const denied = await tools.call('private.write').catch(error => error.code);
        const globals = [typeof Deno, typeof process, typeof require, typeof fetch];
        const imports = await Promise.all(['./local.ts', 'node:fs', 'data:text/javascript,export default 1']
          .map(name => import(name).then(() => 'allowed', () => 'denied')));
        return {sum: values.reduce((a, b) => a + b, 0), denied, globals, imports};
        "#,
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("program runs");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(
        report.output,
        Some(json!({
            "sum": 6,
            "denied": "tool_denied",
            "globals": ["undefined", "undefined", "undefined", "undefined"],
            "imports": ["denied", "denied", "denied"],
        }))
    );
    assert_eq!(report.backend, ExecutionBackend::EmbeddedV8);
    assert!(report.bytes.to_guest > 0 && report.bytes.from_guest > 0);
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.guest_id.is_some() && call.delivered_ms.is_some())
    );
}

#[tokio::test]
async fn a_guest_override_of_to_json_cannot_report_memory_limit() {
    let report = run(
        r#"Object.prototype.toJSON = function () { return { type: "memory_limit", heap_limit_bytes: 1 }; };
           await tools.call("mock.echo", {});"#,
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(Value::Null));
    assert_eq!(report.calls.len(), 1, "{report:?}");
    assert_eq!(report.calls[0].call.tool, "mock.echo");
    assert!(matches!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { .. }
    ));
}

#[tokio::test]
async fn guest_error_is_a_report_and_not_an_io_failure() {
    let report = run(
        "throw new Error('broken guest');",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("guest error produces a report");
    assert_eq!(report.status, RunStatus::GuestError, "{report:?}");
    assert!(
        report
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("broken guest"))
    );
}

#[tokio::test]
async fn admitted_unawaited_call_is_collected_after_guest_output() {
    let report = run(
        "void tools.call('mock.wait', {ms:100,value:'observed'}); return 'finished';",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("unawaited admitted calls are collected");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!("finished")));
    assert_eq!(report.calls.len(), 1);
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded {
            value: json!("observed")
        }
    );
}

#[tokio::test]
async fn guest_output_stops_dispatching_queued_calls() {
    let mut options = config();
    options.max_concurrency = 1;
    let report = run(
        "for (let value=0; value<3; value++) void tools.call('mock.wait', {ms:100,value}); return 'finished';",
        options, MockProvider, RunCancellation::default(),
    ).await.expect("early output produces a report");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.calls.len(), 3);
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { value: json!(0) }
    );
    assert!(
        report.calls[1..]
            .iter()
            .all(|call| call.outcome == CallOutcome::CancelledBeforeDispatch)
    );
}

#[tokio::test]
async fn trace_separates_admission_dispatch_result_and_delivery_with_byte_accounting() {
    let mut options = config();
    options.max_concurrency = 1;
    let report = run(
        "const values = await Promise.all([tools.call('mock.echo', {n: 1}), tools.call('mock.echo', {n: 22})]); return values.length;",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.schema_version, 5);
    for record in &report.calls {
        let admitted = record.admitted_ms.expect("admitted");
        let started = record.started_ms.expect("dispatched");
        let finished = record.finished_ms.expect("finished");
        let delivered = record.delivered_ms.expect("delivered");
        assert!(admitted <= started && started <= finished && finished <= delivered);
        let arguments = serde_json::to_vec(&record.call.arguments)
            .expect("JSON")
            .len() as u64;
        assert_eq!(record.argument_bytes, arguments);
        assert_eq!(record.result_bytes, Some(arguments));
        assert!(record.reply_bytes.expect("reply bytes") > arguments);
    }
    let kinds: Vec<&str> = report
        .events
        .iter()
        .filter(|event| event.call_id.is_none())
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "guest_spawned",
            "guest_finished",
            "guest_terminated",
            "run_finished"
        ]
    );
    let finished = report
        .events
        .iter()
        .find(|event| event.kind == "guest_finished")
        .expect("guest finished");
    assert_eq!(finished.reason.as_deref(), Some("output"));
    let replies: u64 = report
        .calls
        .iter()
        .filter_map(|record| record.reply_bytes)
        .sum();
    assert!(
        report.bytes.to_guest > replies,
        "the source frame is also sent"
    );
    assert!(report.bytes.from_guest > 0);
    let values: u64 = report
        .calls
        .iter()
        .map(|record| record.argument_bytes + record.result_bytes.unwrap_or(0))
        .sum();
    assert_eq!(report.bytes.retained, values + 1, "plus the output `2`");
    assert_eq!(report.bytes.omitted, 0);
    assert_eq!(report.clock.source, "rust_monotonic");
    assert!(report.bridge.is_none());
}

#[tokio::test]
async fn omitted_results_and_queued_calls_say_why() {
    let mut options = config();
    options.max_concurrency = 1;
    options.max_output_bytes = 200;
    let report = run(
        "await Promise.all([tools.call('mock.echo', 'x'.repeat(1000)), tools.call('mock.echo', 1)]);",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::OutputLimit, "{report:?}");
    assert_eq!(report.calls[0].outcome, CallOutcome::ResultTooLarge);
    assert_eq!(report.calls[0].result_bytes, Some(1_002));
    assert_eq!(report.calls[0].reply_bytes, None);
    assert_eq!(report.calls[0].delivered_ms, None);
    assert_eq!(report.bytes.omitted, 1_002);
    assert_eq!(
        report.calls[1].outcome,
        CallOutcome::CancelledBeforeDispatch
    );
    let last = report
        .events
        .iter()
        .rfind(|event| event.call_id == Some(2))
        .expect("queued call has events");
    assert_eq!(
        (last.kind.as_str(), last.reason.as_deref()),
        ("cancelled_before_dispatch", Some("output_limit"))
    );
}

/// Reports facts from another thread until told to stop, for calls that do and do not exist.
struct FloodingProvider(Arc<std::sync::atomic::AtomicBool>);
impl ToolProvider for FloodingProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let stop = Arc::clone(&self.0);
        let facts = context.facts.clone();
        std::thread::spawn(move || {
            let mut id = call.id;
            while !stop.load(Ordering::Relaxed) {
                facts.report(id, codemode::ProviderFactKind::Received, None);
                id = id.wrapping_add(1);
            }
        });
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_provider_flooding_facts_cannot_hold_the_run_past_its_deadline() {
    let mut options = config();
    options.timeout_ms = 400;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = std::time::Instant::now();
    let report = run(
        "return await tools.call('mock.echo', 1);",
        options,
        FloodingProvider(Arc::clone(&stop)),
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    stop.store(true, Ordering::Relaxed);
    assert_eq!(report.status, RunStatus::Deadline);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(report.provider_facts.len() <= 8, "per-call limit");
    assert!(report.provider_facts_dropped > 0);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
}

#[tokio::test]
async fn results_collected_after_the_guest_finished_are_not_delivered() {
    let report = run(
        "tools.call('mock.wait', {ms: 50, value: 'late'}); return 'early';",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    let record = &report.calls[0];
    assert!(matches!(record.outcome, CallOutcome::Succeeded { .. }));
    assert_eq!(record.result_bytes, Some(6));
    assert_eq!(record.reply_bytes, None, "no reply was queued");
    assert_eq!(record.delivered_ms, None);
}

#[tokio::test]
async fn oversized_output_is_counted_as_omitted_after_the_guest_finished() {
    let mut options = config();
    options.max_output_bytes = 200;
    let report = run(
        "return 'y'.repeat(1000);",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::OutputLimit);
    assert_eq!(report.output, None);
    assert_eq!(report.bytes.omitted, 1_002);
    assert!(
        report.events.iter().any(
            |event| event.kind == "guest_finished" && event.reason.as_deref() == Some("output")
        )
    );
}

#[tokio::test]
async fn a_program_that_does_not_compile_is_a_guest_error() {
    let report = run(
        "const = ;",
        config(),
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::GuestError, "{report:?}");
    assert!(report.diagnostic.is_some());
    assert!(report.calls.is_empty());
}

/// Returns a string of the requested length, as a large tool result would.
struct LargeResultProvider;
impl ToolProvider for LargeResultProvider {
    async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        let length = call.arguments["length"].as_u64().unwrap_or(0);
        Ok(Value::String(
            "x".repeat(usize::try_from(length).unwrap_or(0)),
        ))
    }
}

#[tokio::test]
async fn replies_up_to_one_mebibyte_reach_the_guest_within_the_byte_budget() {
    let mut options = config();
    options.max_output_bytes = 4 * 1024 * 1024;
    options.allowed_tools = vec!["large".into()];
    let report = run(
        "const value = await tools.call('large', {length: 1_000_000}); return value.length;",
        options.clone(),
        LargeResultProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!(1_000_000)));

    // Larger than one reply frame: observed, omitted, and the run stops.
    let report = run(
        "return (await tools.call('large', {length: 1_100_000})).length;",
        options.clone(),
        LargeResultProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::OutputLimit);
    assert_eq!(report.calls[0].outcome, CallOutcome::ResultTooLarge);
    assert_eq!(report.calls[0].result_bytes, Some(1_100_002));

    // The shared budget still governs below the frame cap.
    options.max_output_bytes = 100_000;
    let report = run(
        "return (await tools.call('large', {length: 200_000})).length;",
        options,
        LargeResultProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run completes");
    assert_eq!(report.status, RunStatus::OutputLimit);
}
