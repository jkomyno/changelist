//! The restricted embedded worker is tested through its real process protocol.

#![cfg(feature = "embedded-v8")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<String>,
    diagnostic: Receiver<String>,
}

impl Worker {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codemode"))
            .arg("--internal-embedded-guest")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("embedded worker starts");
        let stdout = child.stdout.take().expect("worker stdout");
        let stderr = child.stderr.take().expect("worker stderr");
        let (send_diagnostic, diagnostic) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if send_diagnostic
                    .send(line.expect("worker stderr is readable"))
                    .is_err()
                {
                    return;
                }
            }
        });
        let (send, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.expect("worker stdout is readable")).is_err() {
                    return;
                }
            }
        });
        Self {
            input: child.stdin.take(),
            output,
            diagnostic,
            child,
        }
    }

    fn send(&mut self, frame: Value) {
        writeln!(self.input.as_mut().expect("worker input open"), "{frame}")
            .expect("write host frame");
    }

    fn receive(&mut self) -> Value {
        let line = self
            .output
            .recv_timeout(Duration::from_secs(10))
            .expect("worker responds within 10 seconds");
        serde_json::from_str(&line).expect("worker emits a JSON frame")
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("worker status") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "worker exits independently of JavaScript progress"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn worker_executes_typescript_without_deno() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run", "code":"const n: number = 20; return await tools.call('mock.echo', {answer:n+22});"}));
    assert_eq!(
        worker.receive(),
        json!({"type":"call", "id":0,"tool":"mock.echo","arguments":{"answer":42}})
    );
    worker.send(json!({"type":"result","id":0,"ok":true,"value":{"answer":42}}));
    assert_eq!(
        worker.receive(),
        json!({"type":"output","value":{"answer":42}})
    );
}

#[test]
fn same_turn_calls_snapshot_arguments_and_provider_errors_are_catchable() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run", "code":r#"
      const args = {n:1};
      const first = tools.call('echo', args);
      args.n = 2;
      const second = tools.call('fail').catch(error => [error.code, error.message]);
      return await Promise.all([first, second]);
    "#}));
    assert_eq!(
        worker.receive(),
        json!({"type":"call","id":0,"tool":"echo","arguments":{"n":1}})
    );
    assert_eq!(
        worker.receive(),
        json!({"type":"call","id":1,"tool":"fail","arguments":null})
    );
    worker.send(json!({"type":"result","id":1,"ok":false,"error":{"code":"denied","message":"approval required"}}));
    worker.send(json!({"type":"result","id":0,"ok":true,"value":{"n":1}}));
    assert_eq!(
        worker.receive(),
        json!({"type":"output","value":[{"n":1},["denied","approval required"]]})
    );
}

#[test]
fn unawaited_calls_precede_final_output_and_uncloneable_arguments_reject_locally() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run","code":r#"
      const rejected = await tools.call('bad', () => 1).then(() => false, () => true);
      queueMicrotask(() => { void tools.call('echo', 'late'); });
      return rejected;
    "#}));
    assert_eq!(
        worker.receive(),
        json!({"type":"call","id":1,"tool":"echo","arguments":"late"})
    );
    assert_eq!(worker.receive(), json!({"type":"output","value":true}));
}

#[test]
fn forbidden_capabilities_and_imports_are_unavailable() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run","code":r#"
      const globals = [typeof Deno, typeof process, typeof require, typeof fetch,
        typeof WebSocket, typeof Worker];
      const imports = await Promise.all(['file:///etc/passwd', 'https://example.com/x.js',
        'node:fs', 'data:text/javascript,export default 1', 'ext:core/ops', 'ext:core/mod.js'].map(async name => {
          try { await import(name); return 'allowed'; } catch { return 'denied'; }
        }));
      return {globals, imports};
    "#}));
    assert_eq!(
        worker.receive(),
        json!({"type":"output","value":{
            "globals":["undefined","undefined","undefined","undefined","undefined","undefined"],
        "imports":["denied","denied","denied","denied","denied","denied"]
          }})
    );
}

#[test]
fn fresh_workers_do_not_share_global_state() {
    for expected in [0, 0] {
        let mut worker = Worker::start();
        worker.send(json!({"type":"run","code":"const previous = globalThis.marker ?? 0; globalThis.marker = 42; return previous;"}));
        assert_eq!(worker.receive(), json!({"type":"output","value":expected}));
    }
}

#[test]
fn stdin_eof_terminates_a_spinning_guest() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run","code":"console.log('spinning'); for (;;) {}"}));
    assert_eq!(
        worker
            .diagnostic
            .recv_timeout(Duration::from_secs(10))
            .expect("guest starts spinning"),
        "spinning"
    );
    drop(worker.input.take());
    assert!(!worker.wait_for_exit().success());
}

#[test]
fn overlong_source_input_and_output_are_rejected() {
    for code in [
        " ".repeat(64 * 1024 + 1),
        "return 'x'.repeat(64 * 1024);".into(),
        "return await tools.call('echo', 'x'.repeat(64 * 1024));".into(),
    ] {
        let mut worker = Worker::start();
        worker.send(json!({"type":"run","code":code}));
        assert_eq!(worker.receive()["type"], "error");
        assert!(!worker.wait_for_exit().success());
    }
    let mut worker = Worker::start();
    worker
        .input
        .as_mut()
        .expect("input open")
        .write_all(&vec![b' '; 1024 * 1024 + 1])
        .expect("overlong input written");
    assert!(!worker.wait_for_exit().success());
}

#[test]
fn a_large_reply_and_a_burst_of_small_replies_are_delivered() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run","code":"const values = await Promise.all(Array.from({length:64}, () => tools.call('echo'))); return [values[0].length, values.length];"}));
    for id in 0..64 {
        assert_eq!(worker.receive()["id"], id);
    }
    for id in 0..64 {
        worker.send(json!({"type":"result","id":id,"ok":true,"value":if id == 0 { "x".repeat(900 * 1024) } else { String::new() }}));
    }
    assert_eq!(
        worker.receive(),
        json!({"type":"output","value":[900 * 1024,64]})
    );
}

#[test]
fn malformed_host_replies_fail_the_worker() {
    let mut worker = Worker::start();
    worker.send(json!({"type":"run","code":"return await tools.call('echo');"}));
    assert_eq!(worker.receive()["type"], "call");
    worker.send(json!({"type":"result","id":0,"ok":false,"error":{"message":"missing code"}}));
    assert_eq!(worker.receive()["type"], "error");
    assert!(!worker.wait_for_exit().success());
}
