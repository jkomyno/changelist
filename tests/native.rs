//! Host-owned requests exercise the same control contract without a guest.

use codemode::{
    CallOutcome, ExecutionBackend, MockProvider, NativeCall, RunCancellation, RunConfig, RunStatus,
    run_calls,
};
use serde_json::json;

fn config() -> RunConfig {
    RunConfig {
        deno: "/nonexistent/codemode-native-must-not-start-deno".into(),
        ..RunConfig::default()
    }
}

fn echo(value: u64) -> NativeCall {
    NativeCall {
        tool: "mock.echo".into(),
        arguments: json!(value),
    }
}

#[tokio::test]
async fn finite_input_drains_and_reports_only_native_evidence() {
    let report = run_calls(
        &[echo(1), echo(2), echo(3)],
        RunConfig {
            max_concurrency: 1,
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("native calls complete");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.backend, ExecutionBackend::Native);
    assert_eq!(report.calls.len(), 3);
    assert!(report.code.is_empty());
    assert!(report.output.is_none());
    assert_eq!(report.bytes.to_guest, 0);
    assert_eq!(report.bytes.from_guest, 0);
    assert!(report.prewarmed_ms.is_none());
    for (index, record) in report.calls.iter().enumerate() {
        assert_eq!(record.call.id, index as u64 + 1);
        assert_eq!(
            record.outcome,
            CallOutcome::Succeeded {
                value: json!(index + 1)
            }
        );
        assert!(record.guest_id.is_none());
        assert!(record.delivered_ms.is_none());
        assert!(record.reply_bytes.is_none());
    }
    assert!(
        !report
            .events
            .iter()
            .any(|event| event.kind.starts_with("guest_") || event.kind == "reply_written")
    );
    assert_eq!(report.bytes.retained, 6);
}

#[tokio::test]
async fn empty_input_succeeds_without_a_guest() {
    let report = run_calls(&[], config(), MockProvider, RunCancellation::default())
        .await
        .expect("empty native input completes");
    assert_eq!(report.status, RunStatus::Success);
    assert!(report.calls.is_empty());
}

fn position(report: &codemode::RunReport, id: u64, kind: &str) -> usize {
    report
        .events
        .iter()
        .position(|event| event.call_id == Some(id) && event.kind == kind)
        .unwrap_or_else(|| panic!("call {id} has a {kind} event"))
}

struct RollingProvider(std::sync::Arc<tokio::sync::Notify>);
impl codemode::ToolProvider for RollingProvider {
    async fn execute(
        &self,
        call: codemode::ToolCall,
        _: codemode::CallContext,
    ) -> Result<serde_json::Value, codemode::ProviderError> {
        if call.arguments == json!(1) {
            self.0.notified().await;
        }
        if call.arguments == json!(3) {
            self.0.notify_one();
        }
        Ok(call.arguments)
    }
}

#[tokio::test]
async fn rolling_refills_before_a_held_call_and_never_exceeds_the_cap() {
    let report = run_calls(
        &[echo(1), echo(2), echo(3), echo(4)],
        RunConfig {
            max_concurrency: 2,
            ..config()
        },
        RollingProvider(std::sync::Arc::new(tokio::sync::Notify::new())),
        RunCancellation::default(),
    )
    .await
    .expect("rolling execution completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert!(position(&report, 3, "dispatched") < position(&report, 1, "succeeded"));
    let (mut active, mut peak) = (0, 0);
    for event in &report.events {
        match event.kind.as_str() {
            "dispatched" => {
                active += 1;
                peak = peak.max(active);
            }
            "succeeded" => active -= 1,
            _ => {}
        }
    }
    assert_eq!(peak, 2);
    assert_eq!(active, 0);
}

#[tokio::test]
async fn batch_waits_for_all_active_calls_then_drains_the_remaining_input() {
    let calls = [80, 0, 0, 0].map(|ms| NativeCall {
        tool: "mock.wait".into(),
        arguments: json!({"ms": ms}),
    });
    let report = run_calls(
        &calls,
        RunConfig {
            max_concurrency: 2,
            dispatch: codemode::DispatchPolicy::Batch,
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("batch execution completes");
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert!(position(&report, 2, "dispatched") < position(&report, 1, "succeeded"));
    assert!(position(&report, 3, "dispatched") > position(&report, 1, "succeeded"));
    assert!(position(&report, 4, "dispatched") < position(&report, 3, "succeeded"));
    assert!(
        report
            .calls
            .iter()
            .all(|call| matches!(call.outcome, CallOutcome::Succeeded { .. }))
    );
}

#[tokio::test]
async fn authorization_and_argument_rejections_consume_the_same_call_allowance() {
    let calls = [
        NativeCall {
            tool: "private.write".into(),
            arguments: json!({}),
        },
        NativeCall {
            tool: "mock.wait".into(),
            arguments: json!({"ms": -1}),
        },
        echo(3),
        echo(4),
    ];
    let report = run_calls(
        &calls,
        RunConfig {
            max_calls: 2,
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("rejections produce a report");
    assert_eq!(report.status, RunStatus::CallLimit);
    assert_eq!(report.calls.len(), 3);
    for (record, code) in
        report
            .calls
            .iter()
            .zip(["tool_denied", "invalid_arguments", "call_limit"])
    {
        assert!(matches!(&record.outcome, CallOutcome::Rejected { error } if error.code == code));
        assert!(record.started_ms.is_none());
    }
}

#[tokio::test]
async fn zero_allowance_records_only_the_first_excess_request() {
    let calls = [
        echo(1),
        NativeCall {
            tool: "mock.echo".into(),
            arguments: json!("x".repeat(100_000)),
        },
    ];
    let report = run_calls(
        &calls,
        RunConfig {
            max_calls: 0,
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("later oversized input is not inspected");
    assert_eq!(report.status, RunStatus::CallLimit);
    assert_eq!(report.calls.len(), 1);
    assert!(
        matches!(&report.calls[0].outcome, CallOutcome::Rejected { error } if error.code == "call_limit")
    );
}

#[tokio::test]
async fn result_budget_is_cumulative_without_a_fictitious_reply_envelope() {
    let report = run_calls(
        &[echo(7), echo(88)],
        RunConfig {
            max_concurrency: 1,
            max_output_bytes: 2,
            ..config()
        },
        MockProvider,
        RunCancellation::default(),
    )
    .await
    .expect("output limit produces a report");
    assert_eq!(report.status, RunStatus::OutputLimit);
    assert_eq!(
        report.calls[0].outcome,
        CallOutcome::Succeeded { value: json!(7) }
    );
    assert_eq!(report.calls[1].outcome, CallOutcome::ResultTooLarge);
    assert_eq!(report.calls[1].result_bytes, Some(2));
    assert_eq!(report.bytes.omitted, 2);
    assert_eq!(
        report.bytes.retained, 4,
        "three argument bytes and the one observed result byte"
    );
}

struct FactsProvider;
impl codemode::ToolProvider for FactsProvider {
    async fn execute(
        &self,
        call: codemode::ToolCall,
        context: codemode::CallContext,
    ) -> Result<serde_json::Value, codemode::ProviderError> {
        context.facts.report(
            call.id,
            codemode::ProviderFactKind::Received,
            Some(50_000.0),
        );
        context
            .facts
            .report(call.id, codemode::ProviderFactKind::PermissionDenied, None);
        for _ in 0..8 {
            context
                .facts
                .report(call.id, codemode::ProviderFactKind::Received, None);
        }
        Err(codemode::ProviderError {
            code: "permission_denied".into(),
            message: "refused".into(),
        })
    }
}

#[tokio::test]
async fn provider_errors_and_bounded_facts_remain_observed_without_reply_delivery() {
    let report = run_calls(
        &[echo(1)],
        config(),
        FactsProvider,
        RunCancellation::default(),
    )
    .await
    .expect("provider error is observed");
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        matches!(&report.calls[0].outcome, CallOutcome::ProviderFailed { error } if error.code == "permission_denied")
    );
    assert_eq!(report.provider_facts.len(), 8);
    assert_eq!(report.provider_facts_dropped, 2);
    assert_eq!(report.provider_facts[0].provider_ms, Some(50_000.0));
    assert!(
        report
            .provider_facts
            .iter()
            .all(|fact| Some(fact.received_ms) <= report.calls[0].finished_ms)
    );
    let inspection = codemode::inspect(&report, &codemode::InspectOptions::default());
    assert_eq!(
        inspection.calls.rows[0].evidence,
        codemode::Evidence::DeniedByProvider
    );
    assert!(inspection.timing.slowest[0].delivery_ms.is_none());
}

struct HoldingProvider(std::sync::Arc<tokio::sync::Notify>);
impl codemode::ToolProvider for HoldingProvider {
    async fn execute(
        &self,
        call: codemode::ToolCall,
        context: codemode::CallContext,
    ) -> Result<serde_json::Value, codemode::ProviderError> {
        context
            .facts
            .report(call.id, codemode::ProviderFactKind::ExecutorStarted, None);
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn deadline_and_cancellation_distinguish_running_and_queued_calls() {
    for cancelled in [false, true] {
        let cancellation = RunCancellation::default();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let controller = if cancelled {
            let cancellation = cancellation.clone();
            let entered = std::sync::Arc::clone(&entered);
            Some(tokio::spawn(async move {
                entered.notified().await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                cancellation.cancel();
            }))
        } else {
            None
        };
        let report = run_calls(
            &[echo(1), echo(2), echo(3)],
            RunConfig {
                max_concurrency: 1,
                timeout_ms: if cancelled { 5_000 } else { 40 },
                ..config()
            },
            HoldingProvider(entered),
            cancellation.clone(),
        )
        .await
        .expect("interrupted execution produces a report");
        if let Some(controller) = controller {
            controller.await.expect("cancellation task completes");
        }
        assert_eq!(
            report.status,
            if cancelled {
                RunStatus::Cancelled
            } else {
                RunStatus::Deadline
            }
        );
        assert_eq!(report.calls.len(), 3);
        assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
        assert!(report.calls[1..].iter().all(|record| record.outcome
            == CallOutcome::CancelledBeforeDispatch
            && record.started_ms.is_none()));
        assert!(cancellation.is_cancelled());
        let inspection = codemode::inspect(&report, &codemode::InspectOptions::default());
        assert_eq!(inspection.reconcile.items.len(), 1);
        assert_eq!(
            inspection.reconcile.items[0].evidence,
            codemode::Evidence::UnknownAfterExecutorStart
        );
    }
}

struct PanicProvider;
impl codemode::ToolProvider for PanicProvider {
    async fn execute(
        &self,
        _: codemode::ToolCall,
        _: codemode::CallContext,
    ) -> Result<serde_json::Value, codemode::ProviderError> {
        panic!("test provider terminates without returning an outcome")
    }
}

#[tokio::test]
async fn provider_panic_does_not_invent_a_known_failure_outcome() {
    let report = run_calls(
        &[echo(1), echo(2), echo(3)],
        RunConfig {
            max_concurrency: 1,
            dispatch: codemode::DispatchPolicy::Batch,
            ..config()
        },
        PanicProvider,
        RunCancellation::default(),
    )
    .await
    .expect("provider panic produces a report");
    assert_eq!(report.status, RunStatus::ProviderTaskError);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    assert!(
        report.calls[1..]
            .iter()
            .all(|record| record.outcome == CallOutcome::CancelledBeforeDispatch)
    );
}

#[tokio::test]
async fn oversized_requests_are_refused_before_any_provider_effect() {
    let calls = [
        echo(1),
        NativeCall {
            tool: "mock.echo".into(),
            arguments: json!("x".repeat(65_536)),
        },
    ];
    let error = run_calls(&calls, config(), PanicProvider, RunCancellation::default())
        .await
        .expect_err("oversized request is invalid configuration");
    assert!(matches!(error, codemode::RunError::Config(message) if message.contains("64 KiB")));
}

#[tokio::test]
async fn cancelled_input_and_invalid_limits_never_dispatch() {
    let cancellation = RunCancellation::default();
    cancellation.cancel();
    let report = run_calls(&[echo(1)], config(), PanicProvider, cancellation)
        .await
        .expect("pre-cancelled run reports cancellation");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(report.calls.is_empty());
    for options in [
        RunConfig {
            timeout_ms: 0,
            ..config()
        },
        RunConfig {
            max_concurrency: 0,
            ..config()
        },
        RunConfig {
            max_output_bytes: 0,
            ..config()
        },
    ] {
        assert!(matches!(
            run_calls(&[], options, PanicProvider, RunCancellation::default()).await,
            Err(codemode::RunError::Config(_))
        ));
    }
}
