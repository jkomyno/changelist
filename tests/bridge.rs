//! Host bridge contracts exercised with a real guest and an in-process fake host.

use std::collections::HashSet;
use std::time::Duration;

use codemode::{
    BridgeError, CallOutcome, ProviderFactKind, RunCancellation, RunConfig, RunReport, RunStatus,
    serve_bridge,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::task::JoinHandle;

fn config() -> RunConfig {
    RunConfig {
        deno: std::env::var_os("CODEMODE_TEST_DENO").map_or_else(|| "deno".into(), Into::into),
        timeout_ms: 5_000,
        ..RunConfig::default()
    }
}

#[tokio::test]
async fn a_host_that_stops_reading_cannot_hold_bridge_shutdown_open() {
    let (mut input, bridge_input) = tokio::io::duplex(4096);
    let (bridge_output, _unread_output) = tokio::io::duplex(64);
    let bridge = tokio::spawn(serve_bridge(
        bridge_input,
        bridge_output,
        config(),
        RunCancellation::default(),
    ));
    let start = json!({"type":"start","version":2,"code":"return 42;","tools":[]});
    input
        .write_all(format!("{start}\n").as_bytes())
        .await
        .expect("start");
    let error = tokio::time::timeout(Duration::from_secs(5), bridge)
        .await
        .expect("bounded shutdown")
        .expect("bridge joined")
        .expect_err("output stalled");
    assert!(
        matches!(error, BridgeError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut),
        "{error}"
    );
}

#[tokio::test]
async fn literal_json_keys_survive_host_replies() {
    let expected = json!({"$serde_json::private::RawValue":"not JSON"});
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.echo');",
        &["host.echo"],
    )
    .await;
    let call = host.next().await;
    host.reply(&call, expected.clone()).await;
    let (_, report) = host.report().await;
    assert_eq!(report.output, Some(expected));
}

/// The parent-host end of a bridge: it reads bridge frames and writes host frames.
struct Host {
    frames: Lines<BufReader<DuplexStream>>,
    input: DuplexStream,
    bridge: JoinHandle<Result<RunReport, BridgeError>>,
}

impl Host {
    fn start(config: RunConfig, cancellation: RunCancellation) -> Self {
        let (input, bridge_input) = tokio::io::duplex(1 << 20);
        let (bridge_output, output) = tokio::io::duplex(1 << 20);
        let bridge = tokio::spawn(serve_bridge(
            bridge_input,
            bridge_output,
            config,
            cancellation,
        ));
        Self {
            frames: BufReader::new(output).lines(),
            input,
            bridge,
        }
    }

    async fn send(&mut self, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).expect("frame serializes");
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .expect("bridge input is open");
    }

    async fn begin(config: RunConfig, code: &str, tools: &[&str]) -> Self {
        let mut host = Self::start(config, RunCancellation::default());
        host.send(json!({"type":"start","version":1,"code":code,"tools":tools}))
            .await;
        host
    }

    async fn next(&mut self) -> Value {
        let line = self
            .frames
            .next_line()
            .await
            .expect("bridge output is readable")
            .expect("bridge wrote a frame");
        codemode::parse_json(&line).expect("bridge frames are JSON")
    }

    async fn reply(&mut self, call: &Value, value: Value) {
        let id = call["id"].clone();
        self.send(json!({"type":"result","id":id,"ok":true,"value":value}))
            .await;
    }

    /// Await the report frame, checking it matches the value returned by the bridge.
    async fn report(mut self) -> (Value, RunReport) {
        let frame = self.next().await;
        assert_eq!(frame["type"], "report", "{frame}");
        let report = self
            .bridge
            .await
            .expect("bridge task completes")
            .expect("bridge returns a report");
        assert_eq!(frame["report"]["status"], json!(report.status));
        (frame["report"].clone(), report)
    }
}

#[tokio::test]
async fn host_executes_only_authorized_calls_while_rust_owns_identity_and_trace() {
    let code = r#"
        const [a, b] = await Promise.all([
            tools.call("host.lookup", { key: "a" }),
            tools.call("host.lookup", { key: "b" }),
        ]);
        let denied = "";
        try { await tools.call("host.delete", { key: "a" }); }
        catch (error) { denied = error.code; }
        return { a, b, denied };
    "#;
    let mut host = Host::begin(config(), code, &["host.lookup"]).await;
    let mut seen = HashSet::new();
    for _ in 0..2 {
        let call = host.next().await;
        assert_eq!(call["type"], "call");
        assert_eq!(
            call["tool"], "host.lookup",
            "only authorized tools reach the host"
        );
        seen.insert(call["id"].as_u64().expect("host-visible id"));
        let value = if call["arguments"]["key"] == "a" {
            Value::Null
        } else {
            json!("B")
        };
        host.reply(&call, value).await;
    }
    assert_eq!(seen, HashSet::from([1, 2]));
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(
        report.output,
        Some(json!({"a": null, "b": "B", "denied": "tool_denied"})),
        "explicit null survives the bridge"
    );
    assert!(matches!(
        report.calls[2].outcome,
        CallOutcome::Rejected { .. }
    ));
}

#[tokio::test]
async fn bridge_dispatch_respects_the_rust_concurrency_cap() {
    let mut options = config();
    options.max_concurrency = 1;
    let code = "return await Promise.all([1,2,3].map(n => tools.call('host.id', n)));";
    let mut host = Host::begin(options, code, &["host.id"]).await;
    for _ in 0..3 {
        let call = host.next().await;
        // With one slot, no other call may arrive until this one is answered.
        let early = tokio::time::timeout(Duration::from_millis(200), host.frames.next_line()).await;
        assert!(
            early.is_err(),
            "a second call was dispatched early: {early:?}"
        );
        host.reply(&call, call["arguments"].clone()).await;
    }
    let (_, report) = host.report().await;
    assert_eq!(report.output, Some(json!([1, 2, 3])));
}

#[tokio::test]
async fn host_failures_are_catchable_and_keep_their_code() {
    let code = "try { await tools.call('host.write', {}); } catch (e) { return e.code + ':' + e.message; }";
    let mut host = Host::begin(config(), code, &["host.write"]).await;
    let call = host.next().await;
    host.send(json!({"type":"result","id":call["id"],"ok":false,
        "error":{"code":"blocked","message":"not approved"}}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.output, Some(json!("blocked:not approved")));
    assert!(matches!(
        &report.calls[0].outcome,
        CallOutcome::ProviderFailed { error } if error.code == "blocked"
    ));
}

#[tokio::test]
async fn oversized_host_results_stop_the_run_instead_of_becoming_catchable_errors() {
    let code = "try { await tools.call('host.dump', {}); } catch { await tools.call('host.write', {}); } return 'continued';";
    for ok in [true, false] {
        let mut host = Host::begin(config(), code, &["host.dump", "host.write"]).await;
        let call = host.next().await;
        host.send(
            json!({"type":"result","id":call["id"],"ok":ok,"too_large":true,"bytes":5_000_000}),
        )
        .await;
        let (_, report) = host.report().await;
        assert_eq!(report.status, RunStatus::OutputLimit);
        assert_eq!(report.calls.len(), 1, "the guest never reached host.write");
        assert_eq!(report.calls[0].outcome, CallOutcome::ResultTooLarge);
        // The host's measurement, not the size of Rust's internal placeholder.
        assert_eq!(report.calls[0].result_bytes, Some(5_000_000));
        assert_eq!(report.bytes.omitted, 5_000_000);
    }
    // Without a host measurement the size is unknown, not invented.
    let mut host = Host::begin(config(), code, &["host.dump", "host.write"]).await;
    let call = host.next().await;
    host.send(json!({"type":"result","id":call["id"],"ok":true,"too_large":true}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.calls[0].result_bytes, None);
    assert_eq!(report.bytes.omitted, 0);
}

#[tokio::test]
async fn host_cancel_marks_dispatched_calls_unknown() {
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.slow', {});",
        &["host.slow"],
    )
    .await;
    let call = host.next().await;
    assert_eq!(call["tool"], "host.slow");
    host.send(json!({"type":"cancel"})).await;
    let (frame, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(frame["calls"][0]["outcome"]["status"], "unknown");
    assert!(report.diagnostic.is_none());
}

#[tokio::test]
async fn closed_host_input_cancels_the_run_and_says_why() {
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.slow', {});",
        &["host.slow"],
    )
    .await;
    host.next().await;
    host.input.shutdown().await.expect("host closes its output");
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    assert_eq!(
        report.diagnostic.as_deref(),
        Some("host closed the bridge input")
    );
}

#[tokio::test]
async fn malformed_host_results_end_the_run_without_inventing_an_outcome() {
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.x', {});",
        &["host.x"],
    )
    .await;
    let call = host.next().await;
    host.send(json!({"type":"result","id":call["id"],"ok":true}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::ProtocolError);
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
}

#[tokio::test]
async fn deadline_records_unanswered_host_calls_as_unknown() {
    let mut options = config();
    options.timeout_ms = 1_000;
    let mut host = Host::begin(
        options,
        "return await tools.call('host.slow', {});",
        &["host.slow"],
    )
    .await;
    host.next().await;
    let (frame, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Deadline);
    assert_eq!(frame["calls"][0]["outcome"]["status"], "unknown");
}

#[tokio::test]
async fn unsupported_start_frames_are_refused_before_any_guest_runs() {
    for (start, expected) in [
        (
            json!({"type":"start","version":3,"code":"return 1;","tools":[]}),
            "unsupported protocol version",
        ),
        (
            json!({"type":"start","version":1,"code":"return 1;","tools":["a","a"]}),
            "duplicate tool name",
        ),
        (json!({"type":"cancel"}), "first frame must be start"),
    ] {
        let mut host = Host::start(config(), RunCancellation::default());
        host.send(start).await;
        let frame = host.next().await;
        assert_eq!(frame["type"], "error");
        assert!(
            frame["message"]
                .as_str()
                .is_some_and(|m| m.contains(expected)),
            "{frame}"
        );
        let result = host.bridge.await.expect("bridge task completes");
        assert!(matches!(result, Err(BridgeError::Protocol(_))));
    }
}

/// Kinds of the call's events, in order.
fn call_events(report: &RunReport, call_id: u64) -> Vec<&str> {
    report
        .events
        .iter()
        .filter(|event| event.call_id == Some(call_id))
        .map(|event| event.kind.as_str())
        .collect()
}

#[tokio::test]
async fn host_facts_are_recorded_as_host_claims_beside_rust_observations() {
    let mut host = Host::start(config(), RunCancellation::default());
    host.send(json!({"type":"start","version":2,"code":"const a = await tools.call('host.read', {key:'k'}); await tools.call('host.read', {key:'z'}); return a;","tools":["host.read"]}))
        .await;
    let call = host.next().await;
    let id = call["id"].clone();
    for (kind, host_ms) in [
        ("received", 10.25),
        ("permission_requested", 10.5),
        ("permission_granted", 31.0),
        ("executor_started", 31.5),
    ] {
        host.send(json!({"type":"fact","id":id,"kind":kind,"host_ms":host_ms}))
            .await;
    }
    host.reply(&call, json!("value")).await;
    // A fact for a finished call is counted as dropped rather than attached to it.
    let second = host.next().await;
    host.send(json!({"type":"fact","id":id,"kind":"executor_started"}))
        .await;
    host.reply(&second, json!("other")).await;
    let (_, report) = host.report().await;
    assert_eq!(report.provider_facts_dropped, 1);
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    let facts: Vec<(ProviderFactKind, Option<f64>)> = report
        .provider_facts
        .iter()
        .map(|fact| (fact.kind, fact.provider_ms))
        .collect();
    assert_eq!(
        facts,
        [
            (ProviderFactKind::Received, Some(10.25)),
            (ProviderFactKind::PermissionRequested, Some(10.5)),
            (ProviderFactKind::PermissionGranted, Some(31.0)),
            (ProviderFactKind::ExecutorStarted, Some(31.5)),
        ]
    );
    let record = &report.calls[0];
    assert!(report.provider_facts.iter().all(|fact| fact.call_id == 1
        && record.started_ms <= Some(fact.received_ms)
        && Some(fact.received_ms) <= record.finished_ms));
    assert_eq!(
        call_events(&report, 1),
        ["admitted", "dispatched", "succeeded", "reply_written"]
    );
    assert_eq!(report.output, Some(json!("value")));
    let bridge = report.bridge.expect("bridge totals");
    assert!(bridge.from_host > 0 && bridge.to_host > 0, "{bridge:?}");
}

#[tokio::test]
async fn host_acknowledgment_without_a_result_leaves_the_call_unknown_with_its_evidence() {
    let mut options = config();
    options.timeout_ms = 1_000;
    let mut host = Host::start(options, RunCancellation::default());
    host.send(json!({"type":"start","version":2,"code":"await Promise.all([tools.call('host.write', {}), tools.call('host.write', {})]);","tools":["host.write"]}))
        .await;
    let first = host.next().await;
    let _second = host.next().await;
    host.send(json!({"type":"fact","id":first["id"],"kind":"executor_started"}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Deadline);
    for call_id in [1, 2] {
        let unknown = report
            .events
            .iter()
            .find(|event| event.call_id == Some(call_id) && event.kind == "unknown")
            .expect("unknown event");
        assert_eq!(unknown.reason.as_deref(), Some("deadline"));
    }
    // Only the first call has host evidence; the second has none, which is not
    // evidence that it did not run.
    assert_eq!(report.provider_facts.len(), 1);
    assert_eq!(report.provider_facts[0].call_id, 1);
    assert_eq!(
        report.provider_facts[0].kind,
        ProviderFactKind::ExecutorStarted
    );
    assert_eq!(report.provider_facts[0].provider_ms, None);
}

#[tokio::test]
async fn facts_beyond_the_per_call_limit_are_counted_not_kept() {
    let mut host = Host::start(config(), RunCancellation::default());
    host.send(json!({"type":"start","version":2,"code":"return await tools.call('host.read', {});","tools":["host.read"]}))
        .await;
    let call = host.next().await;
    for _ in 0..10 {
        host.send(json!({"type":"fact","id":call["id"],"kind":"received"}))
            .await;
    }
    host.reply(&call, json!(1)).await;
    let (_, report) = host.report().await;
    assert_eq!(report.provider_facts.len(), 8);
    assert_eq!(report.provider_facts_dropped, 2);
}

#[tokio::test]
async fn a_size_without_too_large_is_a_protocol_error() {
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.read', {});",
        &["host.read"],
    )
    .await;
    let call = host.next().await;
    host.send(json!({"type":"result","id":call["id"],"ok":true,"value":1,"bytes":1}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::ProtocolError, "{report:?}");
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
}

#[tokio::test]
async fn version_one_hosts_cannot_send_facts() {
    let mut host = Host::begin(
        config(),
        "return await tools.call('host.read', {});",
        &["host.read"],
    )
    .await;
    let call = host.next().await;
    host.send(json!({"type":"fact","id":call["id"],"kind":"received"}))
        .await;
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::ProtocolError);
    assert_eq!(
        report.diagnostic.as_deref(),
        Some("fact frames need protocol version 2")
    );
    assert_eq!(report.calls[0].outcome, CallOutcome::Unknown);
    // The call ended because of the protocol error, not a host cancellation.
    let unknown = report
        .events
        .iter()
        .find(|event| event.kind == "unknown")
        .expect("unknown event");
    assert_eq!(unknown.reason.as_deref(), Some("protocol_error"));
}

#[tokio::test]
async fn a_bridge_started_ahead_of_its_program_waits_and_times_the_run_from_start() {
    let mut options = config();
    options.timeout_ms = 300;
    let mut host = Host::start(options, RunCancellation::default());
    // Idle for longer than the run's deadline before sending the program.
    tokio::time::sleep(Duration::from_millis(400)).await;
    host.send(json!({"type":"start","version":2,"code":"return await tools.call('host.read', {});","tools":["host.read"]}))
        .await;
    let call = host.next().await;
    host.reply(&call, json!("ready")).await;
    let (_, report) = host.report().await;
    assert_eq!(report.status, RunStatus::Success, "{report:?}");
    assert_eq!(report.output, Some(json!("ready")));
    assert!(
        report.prewarmed_ms.is_some_and(|ms| ms >= 350),
        "{report:?}"
    );
}
