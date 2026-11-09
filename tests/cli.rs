//! Command-line contracts: removed surfaces, plan previews, the bridge over
//! process stdio, and signal and worker-process handling.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn removed_commands_and_options_are_unknown() {
    let cases: &[(&[&str], &str)] = &[
        (&["run", "--code", "-"], "unknown command 'run'"),
        (&["bridge", "--code", "x.ts"], "unknown option '--code'"),
        (&["bridge", "--trace", "x.json"], "unknown option '--trace'"),
        (&["bridge", "--deno", "x"], "unknown option '--deno'"),
        (
            &["bridge", "--backend", "embedded-v8"],
            "unknown option '--backend'",
        ),
        (&["plan", "run", "--deno", "x"], "unknown option '--deno'"),
        (
            &["plan", "resume", "--deno", "x"],
            "unknown option '--deno'",
        ),
        (
            &["plan", "run", "--fixture", "x"],
            "unknown option '--fixture'",
        ),
        (
            &["plan", "resume", "--fixture", "x"],
            "unknown option '--fixture'",
        ),
        (
            &["plan", "run", "--account", "x"],
            "unknown option '--account'",
        ),
        (
            &["plan", "resume", "--account", "x"],
            "unknown option '--account'",
        ),
        (
            &["plan", "inspect", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "approve", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "run", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "resume", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "review", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "trace", "--durability", "process"],
            "unknown option '--durability'",
        ),
        (
            &["plan", "approve", "--attempt-timeout-ms", "1"],
            "unknown option '--attempt-timeout-ms'",
        ),
        (
            &["plan", "approve", "--probe-timeout-ms", "1"],
            "unknown option '--probe-timeout-ms'",
        ),
        (
            &["plan", "approve", "--session-timeout-ms", "1"],
            "unknown option '--session-timeout-ms'",
        ),
        (
            &["plan", "run", "--bridge", "--deadline-ms", "0"],
            "--deadline-ms must be positive",
        ),
    ];
    for (args, expected) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .args(*args)
            .stdin(Stdio::null())
            .output()
            .expect("CLI starts");
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}

/// Lives here, not beside the in-process bridge tests: a spawned child briefly
/// inherits every descriptor the test process has open, including other tests'
/// journal locks.
#[test]
fn the_session_deadline_ends_an_unanswered_authorization_with_exit_status_124() {
    use std::io::BufRead;

    use codemode::{Approval, Durability, Journal, Plan, ProviderDeclaration, RecoveryPolicy};

    let path =
        std::env::temp_dir().join(format!("codemode-cli-deadline-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let declaration = json!({"provider": "test", "account": "bot",
                             "contracts": {"label": {"repeat": "safe", "reconcile": "none"}}});
    let plan = Plan::parse(
        &json!({"version": 1, "operations": [{"id": "a", "tool": "label",
                "scope": {"provider": "test", "account": "bot"}, "arguments": {}}]})
        .to_string(),
    )
    .expect("plan");
    let parsed: ProviderDeclaration =
        serde_json::from_value(declaration.clone()).expect("declaration");
    let policy = RecoveryPolicy::default();
    Journal::create(&path, &plan, Durability::Process)
        .expect("journal")
        .approve(&Approval {
            binding: codemode::approval_binding(plan.digest(), &parsed, &policy),
            plan: plan.digest().clone(),
            declaration: parsed,
            policy,
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("approve");

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["plan", "run", "--journal"])
        .arg(&path)
        .args(["--bridge", "--deadline-ms", "300"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("CLI starts");
    // The input stays open: closing it would cancel the session instead.
    let mut input = bridge.stdin.take().expect("stdin");
    writeln!(
        input,
        "{}",
        json!({"type": "start", "version": 3, "declaration": declaration})
    )
    .expect("start frame");
    let mut frames = std::io::BufReader::new(bridge.stdout.take().expect("stdout")).lines();
    let mut next = || -> Value {
        serde_json::from_str(&frames.next().expect("a frame").expect("readable")).expect("JSON")
    };
    assert_eq!(next()["type"], "authorize");
    let outcome = next();
    assert_eq!(outcome["type"], "outcome", "{outcome}");
    assert_eq!(outcome["outcome"]["status"], "interrupted", "{outcome}");
    assert_eq!(outcome["outcome"]["dispatched"], 0, "{outcome}");
    assert_eq!(bridge.wait().expect("exits").code(), Some(124));
    drop(input);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn plan_preview_escapes_terminal_text_without_changing_the_approved_data() {
    let directory = std::env::temp_dir().join(format!("codemode-cli-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir(&directory).expect("create scratch directory");
    let summary = "safe\u{1b}[2J\u{1b}[HAPPROVED\nforged line";
    let key = "x\u{1b}[31m";
    let plan = json!({"version":1,"summary":summary,"operations":[{
        "id":"op","tool":"test.action",
        "scope":{"provider":"test","account":"bot","destination":"repo"},
        "arguments":{key:1}
    }]})
    .to_string();
    let path = directory.join("plan.json");
    std::fs::write(&path, &plan).expect("write plan");
    let path = path.to_str().expect("UTF-8 path");
    let expected = codemode::Plan::parse(&plan).expect("valid plan");
    let check = |extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .args(["plan", "check", path])
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .expect("CLI starts");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("UTF-8")
    };
    let preview = check(&[]);
    assert!(!preview.chars().any(|c| c.is_control() && c != '\n'));
    assert!(!preview.contains("\nforged line"));
    assert!(preview.contains(expected.digest().as_str()));
    let decoded: Value = serde_json::from_str(&check(&["--json"])).expect("JSON preview");
    let _ = std::fs::remove_dir_all(&directory);
    assert_eq!(decoded["digest"], expected.digest().as_str());
    assert_eq!(decoded["plan"]["summary"], summary);
    assert_eq!(decoded["plan"]["operations"][0]["arguments"][key], 1);
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
    let start = json!({"type":"start","version":2,"tools":["host.add"],
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
}

/// SIGTERM, like Ctrl-C, cancels a running program and still reports it.
#[cfg(unix)]
#[test]
fn sigterm_cancels_the_run_and_still_reports() {
    use std::io::{BufRead, BufReader};

    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--timeout-ms", "20000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("CLI starts");
    let mut input = child.stdin.take().expect("stdin is piped");
    let mut output = BufReader::new(child.stdout.take().expect("stdout is piped"));
    writeln!(
        input,
        "{}",
        json!({"type":"start","version":2,"tools":["host.wait"],
        "code":"return await tools.call('host.wait');"})
    )
    .expect("start frame is writable");
    let mut line = String::new();
    output.read_line(&mut line).expect("call frame is readable");
    assert_eq!(
        serde_json::from_str::<Value>(&line).expect("call frame")["type"],
        "call"
    );
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
    line.clear();
    output
        .read_line(&mut line)
        .expect("report frame is readable");
    let frame: Value = serde_json::from_str(&line).expect("report frame is JSON");
    assert_eq!(frame["type"], "report");
    assert_eq!(frame["report"]["status"], "cancelled");
    assert_eq!(frame["report"]["calls"][0]["outcome"]["status"], "unknown");
    let status = child.wait().expect("CLI finishes");
    assert_eq!(status.code(), Some(130));
    drop(input);
}

/// A bridge waiting for its program still stops on SIGTERM.
#[cfg(unix)]
#[test]
fn an_idle_bridge_stops_on_sigterm() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .arg("bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bridge starts");
    // Keep the input open, so only the signal can stop the bridge.
    let _input = child.stdin.take();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
    let started = std::time::Instant::now();
    let output = child.wait_with_output().expect("bridge exits");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("cancelled before the start frame"));
}

/// The bridge starts its worker from its own executable, without a PATH.
#[test]
fn a_bridge_runs_its_program_with_an_empty_environment() {
    use std::io::{BufRead, BufReader};

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .arg("bridge")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("bridge starts");
    let mut input = bridge.stdin.take().expect("bridge stdin");
    let mut output = BufReader::new(bridge.stdout.take().expect("bridge stdout"));
    writeln!(
        input,
        "{}",
        json!({"type":"start","version":2,"tools":["host.echo"],
        "code":"const n: number = 42; return await tools.call('host.echo', n);"})
    )
    .expect("start");
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

/// Killing a program host leaves no worker running, even one spinning on CPU.
#[cfg(unix)]
#[test]
fn killing_a_program_host_does_not_leave_its_worker_running() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--timeout-ms", "20000"])
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
        if !status.status.success() || state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker {pid} outlived its killed bridge"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
