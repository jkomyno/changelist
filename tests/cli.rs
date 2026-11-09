//! Command-line contracts: removed surfaces, plan previews, the bridge over
//! process stdio, and signal and guest-process handling.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn removed_commands_and_options_are_unknown() {
    let cases: &[(&[&str], &str)] = &[
        (&["run", "--code", "-"], "unknown command 'run'"),
        (&["bridge", "--code", "x.ts"], "unknown option '--code'"),
        (&["bridge", "--trace", "x.json"], "unknown option '--trace'"),
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
    assert_eq!(decoded["summary"], summary);
    assert_eq!(decoded["operations"][0]["argument_keys"][0], key);
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

/// A bridge waiting for its program, as a prewarmed one does, still stops on SIGTERM.
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

/// A bridge whose guest dies while waiting reports it instead of waiting forever.
#[cfg(unix)]
#[test]
fn a_bridge_whose_guest_exits_before_its_program_reports_it() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--deno", "/usr/bin/true"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bridge starts");
    // Keep the input open, so only the guest's exit can stop the bridge.
    let _input = child.stdin.take();
    let output = child.wait_with_output().expect("bridge exits");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("guest exited before the start frame"),
        "{output:?}"
    );
}

/// Guest cleanup still applies to programs after approved plans stop using guests.
#[cfg(unix)]
#[test]
fn killing_a_program_host_does_not_leave_its_deno_guest_running() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(["bridge", "--timeout-ms", "20000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("program host starts");
    let mut input = child.stdin.take().expect("host stdin");
    let mut output = BufReader::new(child.stdout.take().expect("host stdout"));
    writeln!(
        input,
        "{}",
        json!({"type":"start","version":2,"tools":["host.wait"],
        "code":"return await tools.call('host.wait');"})
    )
    .expect("send program");
    let mut line = String::new();
    output.read_line(&mut line).expect("guest submitted a call");
    let call: Value = serde_json::from_str(&line).expect("call frame");
    assert_eq!(call["type"], "call");

    let pattern = format!("codemode-{}-[0-9]+\\.ts", child.id());
    let guest_running = || {
        Command::new("pgrep")
            .args(["-f", &pattern])
            .status()
            .expect("find this host's guest")
            .success()
    };
    assert!(
        guest_running(),
        "guest must exist before the host is killed"
    );
    child.kill().expect("SIGKILL program host");
    child.wait().expect("reap program host");
    let deadline = Instant::now() + Duration::from_secs(10);
    while guest_running() {
        assert!(Instant::now() < deadline, "guest outlived its killed host");
        std::thread::sleep(Duration::from_millis(20));
    }
}
