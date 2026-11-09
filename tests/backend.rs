//! Explicit program backend selection through the library, CLI, and bridge.

use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(not(feature = "embedded-v8"))]
#[test]
fn selecting_embedded_without_the_build_feature_explains_the_requirement() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["run", "--code", "-", "--backend", "embedded-v8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"return 42;")
        .expect("source");
    let output = child.wait_with_output().expect("CLI exits");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("embedded-v8 build feature"),
        "{output:?}"
    );
}

#[cfg(feature = "embedded-v8")]
mod enabled {
    use super::*;
    use codemode::{
        CallOutcome, ExecutionBackend, GuestBackend, MockProvider, PreparedGuest, RunCancellation,
        RunConfig, RunStatus, run, run_prepared,
    };
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    fn config() -> RunConfig {
        RunConfig {
            deno: "/codemode-test-no-deno/deno".into(),
            backend: GuestBackend::EmbeddedV8 {
                executable: env!("CARGO_BIN_EXE_codemode").into(),
            },
            ..RunConfig::default()
        }
    }

    #[tokio::test]
    async fn explicit_library_worker_runs_typed_composition_with_truthful_evidence() {
        let report = run(r#"
            const values: number[] = await Promise.all([1,2,3].map(n => tools.call('mock.echo', n)));
            const denied = await tools.call('private.write').catch(error => error.code);
            return {sum: values.reduce((a,b) => a+b, 0), denied, deno: typeof Deno};
        "#, config(), MockProvider, RunCancellation::default()).await.expect("embedded run");
        assert_eq!(report.status, RunStatus::Success, "{report:?}");
        assert_eq!(
            report.output,
            Some(json!({"sum":6,"denied":"tool_denied","deno":"undefined"}))
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
    async fn prewarmed_embedded_guest_keeps_its_actual_backend_and_one_run_lifecycle() {
        let guest =
            PreparedGuest::start_embedded(std::path::Path::new(env!("CARGO_BIN_EXE_codemode")))
                .expect("embedded prewarm");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let report = run_prepared(
            guest,
            "return await tools.call('mock.echo', 42);",
            config(),
            MockProvider,
            RunCancellation::default(),
        )
        .await
        .expect("prepared run");
        assert_eq!(report.status, RunStatus::Success);
        assert_eq!(report.output, Some(json!(42)));
        assert_eq!(report.backend, ExecutionBackend::EmbeddedV8);
        assert!(report.prewarmed_ms.is_some());
    }

    #[tokio::test]
    async fn parent_deadline_terminates_cpu_bound_embedded_code() {
        let started = Instant::now();
        let report = run(
            "for (;;) {}",
            RunConfig {
                timeout_ms: 150,
                ..config()
            },
            MockProvider,
            RunCancellation::default(),
        )
        .await
        .expect("deadline report");
        assert_eq!(report.status, RunStatus::Deadline);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn same_turn_submission_and_output_budget_use_parent_controls() {
        let report = run(
            "return await Promise.all([60,0,0,0].map(ms => tools.call('mock.wait',{ms})));",
            RunConfig {
                max_concurrency: 2,
                ..config()
            },
            MockProvider,
            RunCancellation::default(),
        )
        .await
        .expect("rolling report");
        assert_eq!(report.status, RunStatus::Success, "{report:?}");
        let event = |id, kind: &str| {
            report
                .events
                .iter()
                .position(|event| event.call_id == Some(id) && event.kind == kind)
                .expect("event")
        };
        // A freed slot is refilled while the slow call still runs.
        assert!(event(2, "dispatched") < event(1, "succeeded"));
        assert!(event(3, "dispatched") < event(1, "succeeded"));
        let limited = run(
            "return await tools.call('mock.echo', 'x'.repeat(100));",
            RunConfig {
                max_output_bytes: 50,
                ..config()
            },
            MockProvider,
            RunCancellation::default(),
        )
        .await
        .expect("limited report");
        assert_eq!(limited.status, RunStatus::OutputLimit);
        assert!(matches!(
            limited.calls[0].outcome,
            CallOutcome::ResultTooLarge
        ));
    }

    struct CancellingProvider;

    impl codemode::ToolProvider for CancellingProvider {
        async fn execute(
            &self,
            _: codemode::ToolCall,
            context: codemode::CallContext,
        ) -> Result<Value, codemode::ProviderError> {
            context.cancellation.cancel();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn aborting_an_embedded_run_preserves_unknown_provider_outcomes() {
        let report = run(
            "return await tools.call('mock.echo');",
            config(),
            CancellingProvider,
            RunCancellation::default(),
        )
        .await
        .expect("cancelled report");
        assert_eq!(report.status, RunStatus::Cancelled);
        assert!(matches!(report.calls[0].outcome, CallOutcome::Unknown));
    }

    #[cfg(unix)]
    #[test]
    fn killing_the_bridge_stops_its_embedded_worker() {
        let mut bridge = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .args([
                "bridge",
                "--backend",
                "embedded-v8",
                "--timeout-ms",
                "20000",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("bridge starts");
        let mut input = bridge.stdin.take().expect("bridge stdin");
        let mut output = BufReader::new(bridge.stdout.take().expect("bridge stdout"));
        writeln!(
            input,
            "{}",
            json!({"type":"start","version":2,"tools":["ready"],
            "code":"await tools.call('ready'); for (;;) {}"})
        )
        .expect("start");
        let mut line = String::new();
        output.read_line(&mut line).expect("ready call");
        let call: Value = serde_json::from_str(&line).expect("call");
        assert_eq!(call["type"], "call");
        let children = Command::new("pgrep")
            .args(["-P", &bridge.id().to_string()])
            .output()
            .expect("find worker");
        assert!(
            children.status.success(),
            "worker must exist before killing its parent"
        );
        let pid: u32 = String::from_utf8(children.stdout)
            .expect("worker pid")
            .trim()
            .parse()
            .expect("one direct worker child");
        writeln!(
            input,
            "{}",
            json!({"type":"result","id":call["id"],"ok":true,"value":null})
        )
        .expect("reply");
        bridge.kill().expect("SIGKILL bridge");
        bridge.wait().expect("reap bridge");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("worker status");
            let state = String::from_utf8_lossy(&status.stdout);
            if !status.status.success() || state.trim().is_empty() || state.trim().starts_with('Z')
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker {pid} outlived its killed bridge"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn cli_and_bridge_select_embedded_with_an_empty_environment() {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .args(["run", "--code", "-", "--backend", "embedded-v8"])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("CLI starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"const n: number = 42; return n;")
            .expect("source");
        let output = child.wait_with_output().expect("CLI exits");
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).expect("report");
        assert_eq!(report["output"], 42);
        assert_eq!(report["backend"], "embedded_v8");

        let mut bridge = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .args(["bridge", "--backend", "embedded-v8"])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("bridge starts");
        let mut input = bridge.stdin.take().expect("bridge stdin");
        let mut output = BufReader::new(bridge.stdout.take().expect("bridge stdout"));
        writeln!(input,"{}",json!({"type":"start","version":2,"tools":["host.echo"],"code":"return await tools.call('host.echo', 42);"})).expect("start");
        let mut line = String::new();
        output.read_line(&mut line).expect("call");
        let call: Value = serde_json::from_str(&line).expect("call JSON");
        assert_eq!(call["tool"], "host.echo");
        writeln!(
            input,
            "{}",
            json!({"type":"result","id":call["id"],"ok":true,"value":42})
        )
        .expect("reply");
        line.clear();
        output.read_line(&mut line).expect("report");
        let report: Value = serde_json::from_str(&line).expect("report JSON");
        assert_eq!(report["report"]["backend"], "embedded_v8");
        assert_eq!(report["report"]["output"], 42);
        assert!(bridge.wait().expect("bridge exits").success());
    }
}
