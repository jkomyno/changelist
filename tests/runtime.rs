//! Public runtime contracts exercised through the real guest subprocess.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use codemode::{
    CallContext, CallOutcome, DispatchPolicy, MockProvider, ProviderError, RunCancellation,
    RunConfig, RunStatus, ToolCall, ToolProvider, run,
};
use serde_json::{Value, json};

fn config() -> RunConfig {
    RunConfig {
        deno: std::env::var_os("CODEMODE_TEST_DENO").map_or_else(|| "deno".into(), Into::into),
        timeout_ms: 5_000,
        ..RunConfig::default()
    }
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
    .expect("Deno executes TypeScript");
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

fn position(report: &codemode::RunReport, call_id: u64, kind: &str) -> usize {
    report
        .events
        .iter()
        .position(|e| e.call_id == Some(call_id) && e.kind == kind)
        .unwrap_or_else(|| panic!("call {call_id} has a {kind} event"))
}

#[tokio::test]
async fn batch_dispatch_waits_for_the_whole_batch_and_groups_submitted_calls() {
    let mut options = config();
    options.max_concurrency = 2;
    options.dispatch = DispatchPolicy::Batch;
    let report = run(
        "return await Promise.all([{ms:150,value:0},{ms:0,value:1},{ms:0,value:2},{ms:0,value:3}].map(a => tools.call('mock.wait', a)));",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("batch run completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!([0, 1, 2, 3])));
    assert_eq!(report.limits.dispatch, DispatchPolicy::Batch);
    // Both calls submitted together form the first batch.
    assert!(position(&report, 2, "dispatched") < position(&report, 2, "succeeded"));
    assert!(position(&report, 2, "dispatched") < position(&report, 1, "succeeded"));
    // The idle slot is not refilled until the slow call finishes the batch.
    assert!(position(&report, 3, "dispatched") > position(&report, 1, "succeeded"));
    assert!(position(&report, 4, "dispatched") < position(&report, 3, "succeeded"));
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
async fn an_incomplete_stdout_frame_cannot_hold_back_a_batch() {
    let mut options = config();
    options.timeout_ms = 1_500;
    options.dispatch = DispatchPolicy::Batch;
    let report = run(
        r#"Deno.stdout.writeSync(new TextEncoder().encode('{"type":"call","id":1000,"tool":"mock.echo","arguments":1}\n{')); await new Promise(() => {});"#,
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("run produces a report");
    assert_eq!(report.status, RunStatus::Deadline, "{report:?}");
    assert!(
        matches!(report.calls[0].outcome, CallOutcome::Succeeded { .. }),
        "{report:?}"
    );
}

#[tokio::test]
async fn exhausted_call_budget_distinguishes_queued_and_running_calls() {
    let mut options = config();
    options.max_calls = 2;
    options.max_concurrency = 1;
    let report = run(
        "return await Promise.all([1,2,3].map(value => tools.call('mock.wait', {ms:1000,value})));",
        options,
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("call limit produces a report");
    assert_eq!(report.status, RunStatus::CallLimit, "{report:?}");
    assert_eq!(report.calls.len(), 2);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    assert_eq!(
        report.calls[1].outcome,
        CallOutcome::CancelledBeforeDispatch
    );
}

#[tokio::test]
async fn deadline_terminates_a_cpu_loop() {
    let mut options = config();
    options.timeout_ms = 300;
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
async fn denial_and_invalid_arguments_never_reach_provider() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut options = config();
    options.allowed_tools = vec!["mock.wait".into()];
    let report = run(
        "const errors=[]; for (const fn of [() => tools.call('mock.echo', 1), () => tools.call('mock.wait', {ms:-1})]) { try { await fn(); } catch (e) { errors.push(e.code); } } return errors;",
        options, CountDispatches(Arc::clone(&dispatches)), RunCancellation::default(),
    ).await.expect("caught host validation errors return normally");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(
        report.output,
        Some(json!(["tool_denied", "invalid_arguments"]))
    );
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

#[tokio::test]
async fn forged_trace_frames_are_rejected_without_dispatch() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let report = run(
        "Deno.stdout.writeSync(new TextEncoder().encode(JSON.stringify({type:'event',id:77,kind:'succeeded'})+'\\n')); await new Promise(() => {});",
        config(), CountDispatches(Arc::clone(&dispatches)), RunCancellation::default(),
    ).await.expect("invalid protocol produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert!(report.calls.is_empty());
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert!(!report.events.iter().any(|event| event.kind == "succeeded"));
}

#[tokio::test]
async fn duplicate_guest_tokens_cannot_reuse_host_identity() {
    let report = run(
        "const frame=JSON.stringify({type:'call',id:44,tool:'mock.wait',arguments:{ms:1000}})+'\\n'; Deno.stdout.writeSync(new TextEncoder().encode(frame+frame)); await new Promise(() => {});",
        config(), MockProvider, RunCancellation::default(),
    ).await.expect("duplicate correlation produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].call.id, 1);
    assert_eq!(report.calls[0].guest_id, 44);
}

#[tokio::test]
async fn unsafe_javascript_tokens_never_reach_provider() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let report = run(
        "Deno.stdout.writeSync(new TextEncoder().encode('{\"type\":\"call\",\"id\":9007199254740993,\"tool\":\"mock.echo\",\"arguments\":null}\\n')); await new Promise(() => {});",
        config(), CountDispatches(Arc::clone(&dispatches)), RunCancellation::default(),
    ).await.expect("invalid correlation produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
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

#[cfg(unix)]
struct KillGuest;
#[cfg(unix)]
impl ToolProvider for KillGuest {
    async fn execute(&self, call: ToolCall, _: CallContext) -> Result<Value, ProviderError> {
        let pid = call.arguments["pid"]
            .as_u64()
            .expect("guest supplied its process id");
        let status = tokio::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .await
            .expect("terminate this test's guest");
        assert!(status.success());
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(json!("observed after guest exit"))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn abrupt_guest_exit_preserves_later_observable_provider_result() {
    let report = run(
        "return await tools.call('mock.echo', {pid:Deno.pid});",
        config(),
        KillGuest,
        RunCancellation::default(),
    )
    .await
    .expect("guest exit produces a report");
    assert_eq!(report.status, RunStatus::GuestError, "{report:?}");
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded {
            value: json!("observed after guest exit")
        }
    );
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

#[tokio::test]
async fn stdout_frame_size_is_bounded_without_a_newline() {
    let report = run(
        "Deno.stdout.writeSync(new TextEncoder().encode('x'.repeat(70000))); await new Promise(() => {});",
        config(), MockProvider, RunCancellation::default(),
    ).await.expect("oversized frame produces a report");
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
}

#[tokio::test]
async fn guest_cannot_read_host_environment_or_files() {
    let report = run(
        "const denied=[]; for (const fn of [() => Deno.env.get('HOME'), () => Deno.readTextFileSync('/etc/hosts'), () => new Deno.Command('/bin/echo').spawn()]) { try { fn(); denied.push(false); } catch { denied.push(true); } } return denied;",
        config(), MockProvider, RunCancellation::default(),
    ).await.expect("permission errors can be caught");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!([true, true, true])));
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
