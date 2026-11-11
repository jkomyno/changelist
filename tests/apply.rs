//! The command surface through the real binaries, against the offline MCP
//! fixture server: the plan file, the one digest, approval without a
//! terminal, continuing, an edited plan, status at every stage, resolve, a
//! killed apply, and text that must not reach a terminal as it is.
#![cfg(all(unix, feature = "testkit"))]

mod support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use codemode::plan_file::PlanFile;
use codemode::render::unsafe_to_print;
use serde_json::{Value, json};
use support::*;

/// A checksum of `file`, computed in another process.
fn checksum(file: &Path) -> Option<String> {
    if !file.exists() {
        return None;
    }
    let output = Command::new("cksum").arg(file).output().expect("cksum");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    Some(
        text.split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn sidecar(journal: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", journal.display()))
}

/// The journal's database bytes, its write-ahead log bytes, and the WAL
/// index's checkpoint position (`nBackfill`, the first field of the
/// checkpoint info after the two 48-byte index headers).
fn storage_state(journal: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let shm = sidecar(journal, "-shm");
    let backfill = shm.exists().then(|| {
        let output = Command::new("od")
            .args(["-An", "-tx1", "-j96", "-N4"])
            .arg(&shm)
            .output()
            .expect("od");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .expect("UTF-8")
            .trim()
            .to_string()
    });
    (
        checksum(journal),
        checksum(&sidecar(journal, "-wal")),
        backfill,
    )
}

/// Poll the status of `plan` until `ready` holds.
fn wait_for(scratch: &Scratch, plan: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = codemode(scratch, &["status", plan, "--json"]);
        if output.status.success() {
            let status = json_of(&stdout(&output));
            if ready(&status) {
                return status;
            }
        }
        assert!(Instant::now() < deadline, "never saw: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn operations(list: &[(&str, &str, Value)]) -> Value {
    let operations: Vec<Value> = list
        .iter()
        .map(|(id, tool, arguments)| json!({"id": id, "tool": tool, "arguments": arguments}))
        .collect();
    json!({ "operations": operations })
}

/// `codemode plan --operations` from `list` into `<name>.plan.json`.
fn plan_list(scratch: &Scratch, name: &str, list: &Value) -> String {
    scratch.write(&format!("{name}.json"), &list.to_string());
    ok(
        scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            &format!("{name}.json"),
        ],
    );
    format!("{name}.plan.json")
}

#[test]
fn plan_writes_one_stable_file_at_a_default_path() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let list = json!({"summary": "Two notes", "operations": [
        {"id": "second", "tool": "notes.put", "arguments": {"text": "two", "id": "b"}},
        {"id": "first", "tool": "notes.append", "arguments": {"id": "a", "text": "one"}},
    ]});
    scratch.write("notes.json", &list.to_string());
    scratch.write("notes-program.ts", &format!("return {list};"));

    // The file is named after its source, and the review ends with the next command.
    let review = ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "notes.json",
        ],
    );
    assert!(review.starts_with("notes.plan.json  sha256:"), "{review}");
    assert!(
        review.ends_with("Next: codemode apply notes.plan.json\n"),
        "{review}"
    );
    let text = scratch.read("notes.plan.json");
    let definition = |tool: &str| {
        json_of(&text)["tools"][tool]["definition"]
            .as_str()
            .expect("definition")
            .to_string()
    };
    assert_eq!(
        text,
        format!(
            r#"{{
  "codemode_plan": 1,
  "summary": "Two notes",
  "server": {{
    "identity": "{url}",
    "file": "server.json",
    "config": "{config}",
    "protocol": "2026-07-28",
    "trust_annotations": true
  }},
  "tools": {{
    "notes.append": {{"class": "write", "definition": "{append}", "destructive": false, "annotated": true}},
    "notes.put": {{"class": "repeatable_write", "definition": "{put}", "destructive": false, "annotated": true}}
  }},
  "operations": [
    {{"id": "second", "tool": "notes.put", "arguments": {{"id": "b", "text": "two"}}}},
    {{"id": "first", "tool": "notes.append", "arguments": {{"id": "a", "text": "one"}}}}
  ]
}}
"#,
            url = server.url,
            config = codemode::Digest::of(scratch.read("server.json").as_bytes()),
            append = definition("notes.append"),
            put = definition("notes.put"),
        ),
        "pretty-printed, keys in a fixed order, operations in the order given"
    );

    // The same input gives the same bytes, from a file, a program, or stdin.
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "notes.json",
        ],
    );
    assert_eq!(scratch.read("notes.plan.json"), text);
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "notes-program.ts",
        ],
    );
    assert_eq!(scratch.read("notes-program.plan.json"), text);
    let mut child = command(
        &scratch,
        &["plan", "--server", "server.json", "--operations", "-"],
    )
    .stdin(Stdio::piped())
    .spawn()
    .expect("plan starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(list.to_string().as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("plan ends");
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).starts_with("codemode.plan.json  sha256:"));
    assert_eq!(scratch.read("codemode.plan.json"), text);

    // The server file is recorded relative to the plan, so that a plan in
    // another directory is applied without --server, from anywhere.
    std::fs::create_dir(scratch.file("plans")).expect("directory");
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "notes.json",
            "--out",
            "plans/deep.plan.json",
        ],
    );
    let deep = scratch.read("plans/deep.plan.json");
    assert!(deep.contains(r#""file": "../server.json""#), "{deep}");
    let digest = digest(&scratch, "plans/deep.plan.json");
    assert_eq!(
        digest,
        support::digest(&scratch, "notes.plan.json"),
        "the path is not bound"
    );
    let applied = command(&scratch, &["apply", "deep.plan.json", "--approve", &digest])
        .current_dir(scratch.file("plans"))
        .output()
        .expect("apply");
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert!(
        scratch.file("plans/deep.plan.journal.db").exists(),
        "the journal is next to the plan"
    );

    // No operations: nothing to review, and no file.
    scratch.write(
        "empty.json",
        r#"{"summary": "nothing to do", "operations": []}"#,
    );
    let empty = ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "empty.json",
        ],
    );
    assert_eq!(empty, "No changes.\n");
    assert!(!scratch.file("empty.plan.json").exists());

    // The two sources exclude each other, and program options need a program.
    for args in [
        &["plan", "--server", "server.json"][..],
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "notes-program.ts",
            "--operations",
            "notes.json",
        ],
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "notes.json",
            "--max-calls",
            "3",
        ],
        &["plan", "--operations", "notes.json"],
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "notes.json",
            "extra",
        ],
    ] {
        exits(&scratch, 2, args);
    }
}

#[test]
fn a_plan_that_was_approved_is_never_overwritten() {
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &[]);
    let program = proposing(2, "put", "notes.put");
    let plan_file = plan(&scratch, &file, "both", &program);
    // Before approval a plan may be made again.
    plan(&scratch, &file, "both", &program);
    let before = scratch.read(&plan_file);
    let (code, _) = approve(&scratch, &plan_file, &[]);
    assert_eq!(code, 0);

    let refused = exits(
        &scratch,
        2,
        &["plan", "--server", "server.json", "--program", "both.ts"],
    );
    let message = stderr(&refused);
    assert!(
        message.contains(
            "both.plan.json was approved or applied: its journal both.plan.journal.db exists"
        ),
        "{message}"
    );
    assert!(
        message.contains("choose another file with --out"),
        "{message}"
    );
    assert_eq!(scratch.read(&plan_file), before);
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "both.ts",
            "--out",
            "again.plan.json",
        ],
    );
    assert_eq!(scratch.read("again.plan.json"), before);
}

#[test]
fn without_a_terminal_only_the_full_digest_approves() {
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &[]);
    let plan_file = plan(&scratch, &file, "both", PROGRAM);
    let digest = digest(&scratch, &plan_file);
    let journal = scratch.file("both.plan.journal.db");
    let untouched = |why: &str| {
        assert!(!journal.exists(), "{why}: a journal was made");
        assert!(ledger(&scratch).is_empty(), "{why}: a write was sent");
    };

    // Nobody is asked. The review is printed, and the refusal says what to do.
    let refused = exits(&scratch, 4, &["apply", &plan_file]);
    assert_eq!(
        stdout(&refused),
        ok(&scratch, &["status", &plan_file]),
        "the review"
    );
    assert_eq!(
        stderr(&refused),
        format!(
            "codemode: not approved. A person must approve this plan:\n  \
             run `codemode apply both.plan.json` in a terminal and answer the question, or\n  \
             pass `--approve {digest}`\n\
             Nothing was sent.\n"
        )
    );
    untouched("no approval");

    // With --json, stdout still holds exactly one document.
    let refused = exits(&scratch, 4, &["apply", &plan_file, "--json"]);
    assert_eq!(json_of(&stdout(&refused))["state"], "not_approved");
    untouched("no approval, --json");

    // A wrong digest, a short one, or a digest of the plan before an edit.
    let short = &digest[..19];
    let other = format!("sha256:{}", "0".repeat(64));
    for wrong in [other.as_str(), short, "yes"] {
        let refused = exits(&scratch, 4, &["apply", &plan_file, "--approve", wrong]);
        let message = stderr(&refused);
        assert!(
            message.contains(&format!(
                "not approved. --approve {wrong} is not this plan's digest:\n  {digest}\n"
            )),
            "{message}"
        );
        untouched("wrong digest");
    }
    let original = scratch.read(&plan_file);
    scratch.write(&plan_file, &original.replace("\"one\"", "\"one!\""));
    let edited = support::digest(&scratch, &plan_file);
    assert_ne!(edited, digest, "one changed character changes the digest");
    exits(&scratch, 4, &["apply", &plan_file, "--approve", &digest]);
    untouched("the digest of another text");
    scratch.write(&plan_file, &original);

    // The right digest approves. The approver's name is recorded as given.
    let applied = exits(
        &scratch,
        0,
        &[
            "apply",
            &plan_file,
            "--approve",
            &digest,
            "--approver",
            "alberto",
        ],
    );
    let text = stdout(&applied);
    assert!(
        text.contains("status   not approved\n"),
        "the review comes first: {text}"
    );
    assert!(
        text.contains("status   done: every write is applied\n         approved by alberto on 2"),
        "{text}"
    );
    assert_eq!(ledger(&scratch).len(), 2);
    let status = status(&scratch, &plan_file);
    assert_eq!(status["approval"]["approver"], "alberto");
    assert_eq!(status["approval"]["digest"], digest.as_str());
    assert_eq!(status["approval"]["concurrency"], 4);
    assert_eq!(status["approval"]["max_attempts"], 3);

    // The approval binds how the writes are sent: the review says so before,
    // and the status after.
    let plan_file = plan(&scratch, &file, "policy", PROGRAM);
    let digest = support::digest(&scratch, &plan_file);
    let applied = exits(
        &scratch,
        0,
        &[
            "apply",
            &plan_file,
            "--approve",
            &digest,
            "--concurrency",
            "2",
            "--max-attempts",
            "5",
        ],
    );
    let text = stdout(&applied);
    assert!(
        text.contains("a repeatable write is sent again (up to 5 attempts)"),
        "{text}"
    );
    assert!(
        text.contains("Sending: the first write alone, then up to 2 at a time."),
        "{text}"
    );
    let status = support::status(&scratch, &plan_file);
    assert_eq!(status["approval"]["concurrency"], 2);
    assert_eq!(status["approval"]["max_attempts"], 5);
    for zero in ["--concurrency", "--max-attempts"] {
        let plan_file = plan(&scratch, &file, "zero", PROGRAM);
        let digest = support::digest(&scratch, &plan_file);
        let refused = exits(
            &scratch,
            2,
            &["apply", &plan_file, "--approve", &digest, zero, "0"],
        );
        assert!(
            stderr(&refused).contains(&format!("{zero} must be positive")),
            "{}",
            stderr(&refused)
        );
        assert!(!scratch.file("zero.plan.journal.db").exists());
    }

    // Without --approver the name is $USER, or "unknown".
    for (user, expected) in [(Some("someone"), "someone"), (None, "unknown")] {
        let name = format!("user-{expected}");
        let plan_file = plan(&scratch, &file, &name, PROGRAM);
        let digest = support::digest(&scratch, &plan_file);
        let mut command = command(&scratch, &["apply", &plan_file, "--approve", &digest]);
        command.env_remove("USER");
        if let Some(user) = user {
            command.env("USER", user);
        }
        let output = command.output().expect("apply");
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(
            support::status(&scratch, &plan_file)["approval"]["approver"],
            expected
        );
    }
}

#[test]
fn a_later_apply_continues_without_a_question_and_an_edited_plan_is_refused() {
    let scratch = Scratch::new();
    // The first call of notes.append fails; the next one works.
    let server = serve(&scratch, "s", &["--fault", "notes.append=is-error"]);
    http_file(&scratch, &server);
    let list = operations(&[
        ("put-a", "notes.put", json!({"id": "a", "text": "one"})),
        ("log", "notes.append", json!({"id": "log", "text": "line"})),
        ("put-b", "notes.put", json!({"id": "b", "text": "two"})),
    ]);
    let plan_file = plan_list(&scratch, "notes", &list);
    let (code, stopped) = approve(&scratch, &plan_file, &["--concurrency", "1"]);
    assert_eq!(code, 3, "{stopped}");
    assert_eq!(
        states(&stopped),
        [
            ("put-a".to_string(), "applied".to_string()),
            ("log".to_string(), "failed".to_string()),
            ("put-b".to_string(), "not_sent".to_string()),
        ]
    );
    let journal = scratch.file("notes.plan.journal.db");

    // An edited plan is not the approved plan: nothing is sent or recorded.
    let approved = scratch.read(&plan_file);
    let sent = ledger(&scratch).len();
    scratch.write(&plan_file, &approved.replace("\"two\"", "\"2\""));
    let before = checksum(&journal);
    let refused = exits(&scratch, 4, &["apply", &plan_file]);
    let message = stderr(&refused);
    assert!(
        message.contains(
            "not approved. notes.plan.json was edited after its approval:\n  approved   sha256:"
        ),
        "{message}"
    );
    assert!(
        message.contains(stopped["digest"].as_str().expect("digest")),
        "{message}"
    );
    assert!(message.contains("Nothing was sent."), "{message}");
    let shown = ok(&scratch, &["status", &plan_file]);
    assert!(
        shown.contains("status   edited after approval\n"),
        "{shown}"
    );
    assert!(
        shown.contains("This file is not the plan that was approved"),
        "{shown}"
    );
    assert_eq!(status(&scratch, &plan_file)["state"], "edited");
    let unresolved = exits(
        &scratch,
        4,
        &["resolve", &plan_file, "log", "--as", "applied"],
    );
    assert!(stderr(&unresolved).contains("Nothing was recorded."));
    assert_eq!(ledger(&scratch).len(), sent);
    assert_eq!(checksum(&journal), before, "the journal was not changed");

    // The same plan in another layout is the same plan.
    let reformatted = serde_json::to_string_pretty(&json_of(&approved)).expect("JSON");
    assert_ne!(reformatted, approved);
    scratch.write(&plan_file, &reformatted);
    assert_eq!(status(&scratch, &plan_file), stopped);
    scratch.write(&plan_file, &approved);

    // A person settles the failed write; apply then continues, and asks nobody.
    ok(
        &scratch,
        &["resolve", &plan_file, "log", "--as", "not-applied"],
    );
    let (code, done) = apply(&scratch, &plan_file, &[]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")), "{done}");
    assert_eq!(write(&done, "log")["attempts"], 2);
    assert_eq!(
        done["approval"], stopped["approval"],
        "the approval is the first one"
    );
    assert_eq!(ledger(&scratch).len(), 3);
    assert_eq!(calls(&scratch, "notes.append"), ["is-error", "run"]);
}

#[test]
fn status_reads_at_every_stage_and_changes_nothing() {
    let scratch = Scratch::new();
    // The append takes four seconds: long enough to look at a running apply.
    let server = serve(&scratch, "s", &["--fault", "notes.append=delay-ms:4000"]);
    let file = http_file(&scratch, &server);
    let planned = {
        scratch.write("both.ts", PROGRAM);
        ok(
            &scratch,
            &["plan", "--server", path(&file), "--program", "both.ts"],
        )
    };
    let plan_file = "both.plan.json";

    // Not approved: the review, word for word.
    assert_eq!(ok(&scratch, &["status", plan_file]), planned);
    let before = status(&scratch, plan_file);
    assert_eq!(
        (&before["state"], &before["approval"]),
        (&json!("not_approved"), &Value::Null)
    );
    assert_eq!(
        states(&before)[1],
        ("append".to_string(), "not_sent".to_string())
    );

    // Running: the first write is applied, the second has no answer yet.
    let digest = digest(&scratch, plan_file);
    let child = command(
        &scratch,
        &["apply", plan_file, "--approve", &digest, "--json"],
    )
    .spawn()
    .expect("apply starts");
    let running = wait_for(&scratch, plan_file, "the append in flight", |status| {
        status["writes"][1]["attempts"] == 1
    });
    assert_eq!(running["state"], "running");
    assert_eq!(standing(&running, "put"), ("applied".into(), "none".into()));
    assert_eq!(write(&running, "append")["state"], "no_reply");
    assert_eq!(write(&running, "append")["error_code"], "no_result");
    let journal = scratch.file("both.plan.journal.db");
    let storage = storage_state(&journal);
    let text = ok(&scratch, &["status", plan_file]);
    assert!(
        text.contains("status   running, or interrupted before it could record its end\n"),
        "{text}"
    );
    assert!(
        text.contains("\napplied 1 · failed 0 · no reply 1 · not sent 0\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "no answer is recorded: codemode is still waiting for it, or stopped before it came"
        ),
        "{text}"
    );
    assert_eq!(
        storage_state(&journal),
        storage,
        "status changed the journal's storage"
    );

    // A second apply, or a resolve, cannot take the journal over.
    for args in [
        &["apply", plan_file][..],
        &["resolve", plan_file, "append", "--as", "applied"],
    ] {
        let refused = exits(&scratch, 2, args);
        let message = stderr(&refused);
        assert!(
            message.contains("locked by another codemode executor"),
            "{message}"
        );
        assert!(
            message.contains("`codemode status` can read it meanwhile"),
            "{message}"
        );
    }
    assert_eq!(storage_state(&journal), storage, "a refused command wrote");

    // Finished: the running apply ends by itself and prints the same status.
    let output = child.wait_with_output().expect("apply ends");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let done = status(&scratch, plan_file);
    assert_eq!(json_of(&stdout(&output)), done);
    assert_eq!(done["state"], "done");
    assert_eq!(done["counts"]["applied"], 2);
    let text = ok(&scratch, &["status", plan_file]);
    assert!(text.ends_with("\napplied (2)\n  put, append\n"), "{text}");
    for extra in ["--full", "--json"] {
        exits(&scratch, 0, &["status", plan_file, extra]);
    }
    // Status needs no server: no connection, and not even the server file.
    drop(server);
    std::fs::remove_file(scratch.file("server.json")).expect("remove");
    let alone = status(&scratch, plan_file);
    assert_eq!(alone["server"]["file_state"], "unreadable");
    assert_eq!(
        (&alone["state"], &alone["writes"]),
        (&done["state"], &done["writes"])
    );
    let text = ok(&scratch, &["status", plan_file]);
    assert!(
        text.contains("\nconfig   server.json  cannot be read\n"),
        "{text}"
    );
    exits(&scratch, 2, &["status"]);
    exits(&scratch, 2, &["status", plan_file, "other.plan.json"]);
}

/// Start `apply`, wait until write `id` has `attempts` recorded attempts, and
/// kill it with SIGKILL while the server still holds the call.
fn kill_while_sent(scratch: &Scratch, args: &[&str], plan: &str, id: &str, attempts: u64) {
    let mut child = command(scratch, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("apply starts");
    wait_for(scratch, plan, "the write in flight", |status| {
        status["state"] != "not_approved" && write(status, id)["attempts"] == attempts
    });
    child.kill().expect("SIGKILL");
    child.wait().expect("reap");
}

#[test]
fn a_killed_apply_is_continued_only_where_a_repeat_is_safe() {
    // A write that is not repeatable: after the kill only a person can settle it.
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--fault", "notes.append=delay-ms:30000,sticky"],
    );
    http_file(&scratch, &server);
    let list = operations(&[
        (
            "log",
            "notes.append",
            json!({"id": "log", "text": "ARGUMENT-VALUE"}),
        ),
        ("put", "notes.put", json!({"id": "a", "text": "one"})),
    ]);
    let plan_file = plan_list(&scratch, "append", &list);
    let digest = digest(&scratch, &plan_file);
    kill_while_sent(
        &scratch,
        &["apply", &plan_file, "--approve", &digest],
        &plan_file,
        "log",
        1,
    );
    let journal = scratch.file("append.plan.journal.db");
    assert!(
        sidecar(&journal, "-wal").exists(),
        "the killed apply left its write-ahead log"
    );
    let storage = storage_state(&journal);
    let killed = status(&scratch, &plan_file);
    assert_eq!(
        (checksum(&journal), checksum(&sidecar(&journal, "-wal"))),
        (storage.0, storage.1)
    );
    assert_eq!(
        killed["state"], "running",
        "an open session: running, or killed"
    );
    assert_eq!(
        standing(&killed, "log"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(
        standing(&killed, "put"),
        ("not_sent".into(), "apply".into())
    );
    let text = ok(&scratch, &["status", &plan_file]);
    assert!(
        text.contains("A session may still be running. If none is, run `codemode apply append.plan.json` to continue."),
        "{text}"
    );
    assert!(
        !text.contains("ARGUMENT-VALUE"),
        "a status prints no argument value: {text}"
    );

    // Apply continues with what is safe: the other write, never the lost one.
    let started = Instant::now();
    let (code, continued) = apply(&scratch, &plan_file, &[]);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the lost write was sent again"
    );
    assert_eq!(code, 3, "{continued}");
    assert_eq!(write(&continued, "log")["attempts"], 1);
    assert_eq!(
        standing(&continued, "log"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(
        standing(&continued, "put"),
        ("applied".into(), "none".into())
    );
    ok(
        &scratch,
        &["resolve", &plan_file, "--no-reply", "--as", "applied"],
    );
    let (code, done) = apply(&scratch, &plan_file, &[]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")));
    assert_eq!(write(&done, "log")["state"], "resolved");

    // A repeatable write is sent again, but only as often as was approved.
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--fault", "notes.put=delay-ms:30000,sticky"],
    );
    http_file(&scratch, &server);
    let list = operations(&[("put", "notes.put", json!({"id": "a", "text": "one"}))]);
    let plan_file = plan_list(&scratch, "put", &list);
    let digest = support::digest(&scratch, &plan_file);
    kill_while_sent(
        &scratch,
        &[
            "apply",
            &plan_file,
            "--approve",
            &digest,
            "--max-attempts",
            "2",
        ],
        &plan_file,
        "put",
        1,
    );
    assert_eq!(
        standing(&status(&scratch, &plan_file), "put"),
        ("no_reply".into(), "apply".into())
    );
    kill_while_sent(&scratch, &["apply", &plan_file], &plan_file, "put", 2);
    let started = Instant::now();
    let (code, spent) = apply(&scratch, &plan_file, &[]);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a third attempt was sent"
    );
    assert_eq!(code, 3, "{spent}");
    assert_eq!(write(&spent, "put")["attempts"], 2, "no third attempt");
    assert_eq!(
        standing(&spent, "put"),
        ("no_reply".into(), "resolve".into())
    );
}

#[test]
fn resolve_selects_by_identity_error_or_silence_and_refuses_what_is_settled() {
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &[
            "--fault",
            "notes.append=is-error",
            "--fault",
            "plain.unannotated=no-effect-drop",
        ],
    );
    http_file(&scratch, &server);
    let list = operations(&[
        ("put", "notes.put", json!({"id": "a", "text": "one"})),
        ("log", "notes.append", json!({"id": "log", "text": "line"})),
        ("plain", "plain.unannotated", json!({"value": "x"})),
        (
            "log-2",
            "notes.append",
            json!({"id": "log", "text": "more"}),
        ),
    ]);
    let plan_file = plan_list(&scratch, "mixed", &list);

    // Nothing was sent yet, so there is nothing to resolve.
    let early = exits(
        &scratch,
        2,
        &["resolve", &plan_file, "log", "--as", "applied"],
    );
    assert!(
        stderr(&early).contains("was not approved yet"),
        "{}",
        stderr(&early)
    );

    let (code, first) = approve(
        &scratch,
        &plan_file,
        &["--concurrency", "1", "--max-failures", "0"],
    );
    assert_eq!(code, 3, "{first}");
    let expected = [
        ("put", "applied"),
        ("log", "failed"),
        ("plain", "no_reply"),
        ("log-2", "applied"),
    ];
    assert_eq!(
        states(&first),
        expected.map(|(id, state)| (id.to_string(), state.to_string()))
    );
    let text = ok(&scratch, &["status", &plan_file]);
    assert!(
        text.contains("codemode resolve mixed.plan.json log --as not-applied   (or --as applied)")
    );
    assert!(
        text.contains(
            "codemode resolve mixed.plan.json plain --as not-applied   (or --as applied)"
        )
    );

    // What cannot be resolved is refused, and nothing is recorded.
    let cases: [(&[&str], &str); 6] = [
        (
            &["resolve", &plan_file, "nope", "--as", "applied"],
            "the plan has no write 'nope'",
        ),
        (
            &["resolve", &plan_file, "put", "--as", "not-applied"],
            "write 'put' has nothing to resolve: it is applied. Nothing was recorded",
        ),
        (
            &["resolve", &plan_file, "log", "put", "--as", "applied"],
            "write 'put' has nothing to resolve",
        ),
        (
            &["resolve", &plan_file, "--as", "applied"],
            "name the writes to resolve",
        ),
        (
            &["resolve", &plan_file, "log", "--as", "maybe"],
            "--as must be applied or not-applied",
        ),
        (&["resolve", &plan_file, "log"], "--as is required"),
    ];
    for (args, expected) in cases {
        let refused = exits(&scratch, 2, args);
        assert!(
            stderr(&refused).contains(expected),
            "{args:?}: {}",
            stderr(&refused)
        );
    }
    assert_eq!(status(&scratch, &plan_file), first, "nothing was recorded");

    // --errors selects the write the server answered with an error.
    let recorded = ok(
        &scratch,
        &[
            "resolve",
            &plan_file,
            "--errors",
            "--as",
            "not-applied",
            "--reviewer",
            "rev",
        ],
    );
    assert_eq!(
        recorded,
        "Recording 1 write as not applied (reviewer rev):\n\n  log  notes.append\n    \
         \"upstream said no\"\n\nRecorded 1 write. Next: codemode apply mixed.plan.json\n"
    );
    let after = status(&scratch, &plan_file);
    assert_eq!(standing(&after, "log"), ("not_sent".into(), "apply".into()));
    assert_eq!(
        standing(&after, "plain"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(
        ok(
            &scratch,
            &["resolve", &plan_file, "--errors", "--as", "applied"]
        ),
        "Nothing to resolve: no write matches.\n"
    );

    // --no-reply selects the write without an answer; naming it too changes nothing.
    let recorded = ok(
        &scratch,
        &[
            "resolve",
            &plan_file,
            "plain",
            "--no-reply",
            "--as",
            "applied",
            "--reviewer",
            "rev",
        ],
    );
    assert!(
        recorded.starts_with(
            "Recording 1 write as applied (reviewer rev):\n\n  plain  plain.unannotated\n"
        ),
        "{recorded}"
    );
    assert!(
        recorded.ends_with("Recorded 1 write. Next: codemode apply mixed.plan.json\n"),
        "{recorded}"
    );

    let (code, done) = apply(&scratch, &plan_file, &[]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")), "{done}");
    assert_eq!(standing(&done, "plain"), ("resolved".into(), "none".into()));
    assert_eq!(done["counts"]["resolved"], 1);
    let text = ok(&scratch, &["status", &plan_file]);
    assert!(
        text.contains("resolved by a person (1): recorded as applied\n  plain\n"),
        "{text}"
    );
    assert_eq!(calls(&scratch, "notes.append"), ["is-error", "run", "run"]);
    assert_eq!(
        calls(&scratch, "plain.unannotated").len(),
        1,
        "never sent again"
    );
}

/// No character a terminal could act on, apart from the line feeds between lines.
fn assert_printable(text: &str, what: &str) {
    assert!(
        text.chars().all(|c| c == '\n' || !unsafe_to_print(c)),
        "{what}: {text:?}"
    );
}

#[test]
fn text_from_a_plan_or_a_server_cannot_reach_the_terminal_as_it_is() {
    let hostile = "ok\u{1b}[2J\u{1b}[31mAPPROVED\nforged line \u{202e}desrever\u{200b}";
    let scratch = Scratch::new();
    // The server repeats a variable of its environment in its error text.
    let state = scratch.file("state.json");
    scratch.write(
        "server.json",
        &json!({"command": FIXTURE,
                "args": ["--stdio", "--state", path(&state), "--fault", "notes.append=is-error-echo"],
                "trust_annotations": true})
        .to_string(),
    );
    let list = json!({"summary": hostile, "operations": [
        {"id": "log", "tool": "notes.append", "arguments": {"id": "log", "text": hostile}},
        {"id": "put-1", "tool": "notes.put", "arguments": {"id": "a", "text": hostile}},
        {"id": "put-2", "tool": "notes.put", "arguments": {"id": "b", "text": "plain"}},
    ]});
    scratch.write("hostile.json", &list.to_string());
    let plan_file = "hostile.plan.json";

    // An argument and the summary, in the review and in the plan file.
    let review = ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "hostile.json",
        ],
    );
    assert_printable(&review, "the review");
    assert!(!review.contains("\nforged line"), "{review}");
    assert!(
        review.contains(
            r"summary  ok\u{1b}[2J\u{1b}[31mAPPROVED forged line \u{202e}desrever\u{200b}"
        ),
        "{review}"
    );
    assert!(
        review.contains(
            r#"text="ok\u001b[2J\u001b[31mAPPROVED\nforged line \u202edesrev ..." (46 bytes)"#
        ),
        "{review}"
    );
    let full = ok(&scratch, &["status", plan_file, "--full"]);
    assert!(
        full.contains(
            r#""text":"ok\u001b[2J\u001b[31mAPPROVED\nforged line \u202edesrever\u200b"}"#
        ),
        "{full}"
    );
    let file = scratch.read(plan_file);
    assert_printable(&file, "the plan file");
    let reread = PlanFile::parse(&file).expect("the plan file reads back");
    assert_eq!(
        reread.summary.as_deref(),
        Some(hostile),
        "the approved data is unchanged"
    );
    assert_eq!(reread.writes[0].arguments["text"], hostile);
    for extra in [&[][..], &["--full"], &["--json"]] {
        let mut args = vec!["status", plan_file];
        args.extend_from_slice(extra);
        assert_printable(&ok(&scratch, &args), "status before approval");
    }

    // The server's error message, in everything apply, status, and resolve print.
    let digest = digest(&scratch, plan_file);
    let run = |args: &[&str]| {
        command(&scratch, args)
            .env("CODEMODE_MCP_FIXTURE_ECHO", hostile)
            .output()
            .expect("codemode starts")
    };
    let applied = run(&["apply", plan_file, "--approve", &digest]);
    assert_eq!(applied.status.code(), Some(3), "{applied:?}");
    let text = stdout(&applied);
    assert_printable(&text, "apply");
    assert_printable(&stderr(&applied), "apply's diagnostics");
    assert!(
        text.contains(r#""upstream said no to ok\u001b[2J\u001b[31mAPPROVED\nforged line \u202edesrever\u200b""#),
        "{text}"
    );
    for extra in [&[][..], &["--full"], &["--json"]] {
        let mut args = vec!["status", plan_file];
        args.extend_from_slice(extra);
        assert_printable(&ok(&scratch, &args), "status after a failure");
    }
    let status = status(&scratch, plan_file);
    assert_eq!(
        write(&status, "log")["error_text"],
        format!("upstream said no to {hostile}"),
        "--json carries the exact text"
    );
    let recorded = ok(
        &scratch,
        &["resolve", plan_file, "--errors", "--as", "applied"],
    );
    assert_printable(&recorded, "resolve");
    assert!(recorded.contains(r"\u202edesrever"), "{recorded}");
}

#[test]
fn a_plan_at_the_limit_is_reviewed_in_full_on_request() {
    let scratch = Scratch::new();
    scratch.write(
        "server.json",
        &json!({"command": FIXTURE, "args": ["--stdio"], "trust_annotations": true}).to_string(),
    );
    // As many writes as a plan may hold, each with a body of its own.
    let list = |count: usize| {
        let operations: Vec<Value> = (0..count)
            .map(|index| {
                json!({"id": format!("op-{index}"), "tool": "notes.put",
                       "arguments": {"id": format!("note-{index}"),
                                     "text": format!("comment {index} ").repeat(20)}})
            })
            .collect();
        json!({ "operations": operations })
    };
    let largest = list(codemode::MAX_OPERATIONS);
    let plan_file = plan_list(&scratch, "largest", &largest);

    // By default the review fits on a screen and says what it hides.
    let short = ok(&scratch, &["status", &plan_file]);
    assert!(
        short.lines().count() < 45,
        "{} lines",
        short.lines().count()
    );
    assert!(short.contains("10,000 writes with 1 tool\n"), "{short}");
    assert!(short.contains("  ... 9,980 more (--full)\n"), "{short}");
    assert!(short.contains(" ...\" (2"), "a long value is cut: {short}");

    // --full prints every write, in plan order, with its arguments never cut.
    let full = ok(&scratch, &["status", &plan_file, "--full"]);
    let mut rest = full.as_str();
    for operation in largest["operations"].as_array().expect("operations") {
        let block = format!(
            "  {}\n    {{\"id\":{},\"text\":{}}}\n",
            operation["id"].as_str().expect("id"),
            operation["arguments"]["id"],
            operation["arguments"]["text"]
        );
        let Some(at) = rest.find(&block) else {
            panic!("{} missing from the full review", operation["id"]);
        };
        rest = &rest[at + block.len()..];
    }
    assert!(!full.contains("--full") && !full.contains("bytes)"));

    // Escaping can make the file larger than the change list: 110 writes of
    // 60,000 DEL characters are 7 MB as given and 40 MB as a plan file. A
    // file that no command would read back is not written.
    let delete = char::from_u32(0x7f).expect("a character").to_string();
    let escaped: Vec<Value> = (0..110)
        .map(|index| {
            json!({"id": format!("w{index}"), "tool": "notes.put",
                   "arguments": {"id": "a", "text": delete.repeat(60_000)}})
        })
        .collect();
    scratch.write(
        "escaped.json",
        &json!({ "operations": escaped }).to_string(),
    );
    let refused = exits(
        &scratch,
        2,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "escaped.json",
        ],
    );
    let message = stderr(&refused);
    assert!(
        message.contains(
            "the plan file would be too large: plan exceeds 33554432 bytes. Nothing was written"
        ),
        "{message}"
    );
    assert!(!scratch.file("escaped.plan.json").exists());

    scratch.write("over.json", &list(codemode::MAX_OPERATIONS + 1).to_string());
    let over = exits(
        &scratch,
        2,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "over.json",
        ],
    );
    assert!(
        stderr(&over).contains("1 to 10000 operations"),
        "{}",
        stderr(&over)
    );
    assert!(!scratch.file("over.plan.json").exists());
}

#[test]
fn an_unreadable_or_invalid_plan_or_journal_is_refused() {
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &[]);
    let plan_file = plan(&scratch, &file, "both", PROGRAM);
    let good = scratch.read(&plan_file);

    for command in ["status", "apply"] {
        let missing = exits(&scratch, 2, &[command, "missing.plan.json"]);
        assert!(stderr(&missing).contains("cannot read plan file missing.plan.json"));
    }
    let cases = [
        ("{}".to_string(), "not a plan file"),
        (
            good.replace("\"codemode_plan\": 1", "\"codemode_plan\": 9"),
            "version 9 is not supported",
        ),
        (
            good.replace("\"tool\": \"notes.put\"", "\"tool\": \"notes.zap\""),
            "does not describe its tool \"notes.zap\"",
        ),
        (
            good.replace(
                "{\"id\": \"a\", \"text\": \"one\"}",
                "{\"id\": \"a\", \"n\": 9007199254740993}",
            ),
            "operation put: arguments.n: 9007199254740993 is outside",
        ),
        (
            good.replace(
                "{\"id\": \"a\", \"text\": \"one\"}",
                "{\"id\": \"a\", \"api_key\": \"k\"}",
            ),
            "looks like a credential",
        ),
    ];
    for (text, expected) in cases {
        scratch.write("bad.plan.json", &text);
        for command in ["status", "apply", "resolve"] {
            let refused = exits(&scratch, 2, &[command, "bad.plan.json"]);
            let message = stderr(&refused);
            assert!(
                message.contains(expected),
                "{command}: {message} should mention {expected}"
            );
        }
    }

    // A journal that is not a journal fails closed, for reading and for writing.
    scratch.write("both.plan.journal.db", "SQLite format 3\0 but not really");
    let digest = PlanFile::parse(&good).expect("plan").digest();
    for args in [
        &["status", plan_file.as_str()][..],
        &["apply", plan_file.as_str(), "--approve", digest.as_str()],
        &["resolve", plan_file.as_str(), "put", "--as", "applied"],
    ] {
        let refused = exits(&scratch, 2, args);
        let message = stderr(&refused);
        assert!(
            message.contains("damaged") || message.contains("not a journal"),
            "{args:?}: {message}"
        );
    }
    assert!(ledger(&scratch).is_empty());
}

#[test]
fn status_in_a_read_only_directory_never_writes() {
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &[]);
    let plan_file = plan(&scratch, &file, "both", PROGRAM);
    let (code, _) = approve(&scratch, &plan_file, &[]);
    assert_eq!(code, 0);
    let listing = || {
        let mut names: Vec<String> = std::fs::read_dir(&scratch.0)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        names.sort();
        names
    };
    let journal = scratch.file("both.plan.journal.db");
    let before = listing();
    let hash = checksum(&journal);
    let status = Command::new("chmod")
        .args(["555", path(&scratch.0)])
        .status()
        .expect("chmod");
    assert!(status.success());
    if std::fs::write(scratch.file("probe"), b"").is_ok() {
        // A privileged user ignores directory permissions; nothing to observe.
        return;
    }
    let output = codemode(&scratch, &["status", &plan_file]);
    if output.status.success() {
        assert!(stdout(&output).contains("status   done: every write is applied"));
    } else {
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            stderr(&output).contains("could not be opened read-only"),
            "{}",
            stderr(&output)
        );
    }
    assert_eq!(listing(), before, "{}", stderr(&output));
    assert_eq!(checksum(&journal), hash);
}

/// A stdio server file whose command writes `<name>` in the scratch directory
/// when it starts, then runs the fixture with a ledger named by `$LEDGER`.
fn marking_server(scratch: &Scratch, file: &str, ledger: &str, extra: Value) -> PathBuf {
    let mut config = json!({
        "command": "sh",
        "args": ["-c", format!("echo started >> \"$MARK\"; exec {FIXTURE} --stdio --ledger \"$LEDGER\"")],
        "env": {"LEDGER": path(&scratch.file(ledger)), "MARK": path(&scratch.file("STARTED"))},
        "trust_annotations": true,
    });
    for (key, value) in extra.as_object().expect("object") {
        config[key] = value.clone();
    }
    scratch.write(file, &config.to_string())
}

fn lines(scratch: &Scratch, name: &str) -> usize {
    std::fs::read_to_string(scratch.file(name))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn an_approval_binds_the_server_file() {
    let scratch = Scratch::new();
    // The operator's file: the environment picks the target, and write_tools
    // allows one tool. A second file has the same command, another target,
    // and no limit.
    marking_server(
        &scratch,
        "server.json",
        "staging.jsonl",
        json!({"write_tools": ["notes.put"]}),
    );
    std::fs::create_dir(scratch.file("agent")).expect("directory");
    marking_server(&scratch, "agent/server.json", "PRODUCTION.jsonl", json!({}));
    let put = operations(&[("p", "notes.put", json!({"id": "a", "text": "b"}))]);
    let plan_file = plan_list(&scratch, "ops", &put);
    let approved = scratch.read(&plan_file);
    let digest = digest(&scratch, &plan_file);
    let recorded = codemode::Digest::of(scratch.read("server.json").as_bytes());
    assert_eq!(json_of(&approved)["server"]["config"], recorded.as_str());
    let before = ok(&scratch, &["status", &plan_file]);
    assert!(
        before.contains(&format!(
            "\nconfig   server.json  sha256:{}\n",
            recorded.short()
        )),
        "{before}"
    );

    // Editing only the path does not change the digest, and sends nothing.
    scratch.write(
        &plan_file,
        &approved.replace(
            "\"file\": \"server.json\"",
            "\"file\": \"agent/server.json\"",
        ),
    );
    assert_eq!(
        support::digest(&scratch, &plan_file),
        digest,
        "the path is not bound"
    );
    let refused = exits(&scratch, 2, &["apply", &plan_file, "--approve", &digest]);
    let message = stderr(&refused);
    assert!(
        message
            .contains("the server file agent/server.json is not the one this plan was made with"),
        "{message}"
    );
    assert!(message.contains("run `codemode plan` again"), "{message}");
    assert!(stdout(&refused).is_empty(), "refused before the review");
    let shown = ok(&scratch, &["status", &plan_file]);
    assert!(
        shown.contains("\nconfig   agent/server.json  changed since the plan was made\n"),
        "{shown}"
    );
    assert_eq!(
        status(&scratch, &plan_file)["server"]["file_state"],
        "changed"
    );

    // A plan made by hand for the other file names that file's digest. The
    // operator's file, given with --server, is not that file.
    let definition = json_of(&ok(
        &scratch,
        &["tools", "--server", "server.json", "--json"],
    ))["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "notes.delete")
        .expect("listed")["definition"]
        .clone();
    let mut forged = json_of(&approved);
    forged["server"]["file"] = json!("agent/server.json");
    forged["server"]["config"] = json!(codemode::Digest::of(
        scratch.read("agent/server.json").as_bytes()
    ));
    forged["tools"] = json!({"notes.delete": {"class": "repeatable_write", "definition": definition,
                                               "destructive": true, "annotated": true}});
    forged["operations"] = json!([{"id": "d", "tool": "notes.delete", "arguments": {"id": "a"}}]);
    scratch.write("forged.plan.json", &forged.to_string());
    let forged_digest = support::digest(&scratch, "forged.plan.json");
    assert_ne!(
        forged_digest, digest,
        "the server file is part of the digest"
    );
    let refused = exits(
        &scratch,
        2,
        &[
            "apply",
            "forged.plan.json",
            "--approve",
            &forged_digest,
            "--server",
            "server.json",
        ],
    );
    assert!(
        stderr(&refused)
            .contains("the server file server.json is not the one this plan was made with"),
        "{}",
        stderr(&refused)
    );

    // A changed or missing server file is refused too, on a later run as well.
    scratch.write(&plan_file, &approved);
    let original = scratch.read("server.json");
    scratch.write(
        "server.json",
        &original.replace("staging.jsonl", "elsewhere.jsonl"),
    );
    exits(&scratch, 2, &["apply", &plan_file, "--approve", &digest]);
    std::fs::remove_file(scratch.file("server.json")).expect("remove");
    let missing = exits(&scratch, 2, &["apply", &plan_file, "--approve", &digest]);
    assert!(stderr(&missing).contains("cannot read server file server.json"));

    // Only `plan` and `tools` started a server; no apply did, and nothing
    // was written anywhere.
    assert_eq!(lines(&scratch, "STARTED"), 2, "an apply started a server");
    for name in ["staging.jsonl", "PRODUCTION.jsonl", "elsewhere.jsonl"] {
        assert_eq!(lines(&scratch, name), 0, "{name}: a write was sent");
    }
    assert!(!scratch.file("PRODUCTION.jsonl").exists());
    for journal in ["ops.plan.journal.db", "forged.plan.journal.db"] {
        assert!(
            !scratch.file(journal).exists(),
            "{journal}: an approval was recorded"
        );
    }

    // The file the plan was made with, from its recorded path or from
    // --server after it moved, is accepted.
    std::fs::create_dir(scratch.file("ci")).expect("directory");
    scratch.write("ci/operator.json", &original);
    let (code, done) = approve(&scratch, &plan_file, &["--server", "ci/operator.json"]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")), "{done}");
    assert_eq!(done["server"]["file"], "ci/operator.json");
    assert_eq!(lines(&scratch, "staging.jsonl"), 1);
    assert!(!scratch.file("PRODUCTION.jsonl").exists());
}

#[test]
fn no_server_is_started_and_no_request_is_made_before_the_approval() {
    let scratch = Scratch::new();
    marking_server(&scratch, "server.json", "ledger.jsonl", json!({}));
    let list = operations(&[("p", "notes.put", json!({"id": "a", "text": "b"}))]);
    let plan_file = plan_list(&scratch, "ops", &list);
    assert_eq!(lines(&scratch, "STARTED"), 1, "plan lists the tools");

    // Status, an apply nobody approved, and a wrong digest start nothing.
    ok(&scratch, &["status", &plan_file]);
    exits(&scratch, 4, &["apply", &plan_file]);
    exits(&scratch, 4, &["apply", &plan_file, "--json"]);
    exits(
        &scratch,
        4,
        &["apply", &plan_file, "--approve", "sha256:00"],
    );
    assert_eq!(
        lines(&scratch, "STARTED"),
        1,
        "a server was started before an approval"
    );

    // The review shows the command that an approval will run, in full.
    let review = ok(&scratch, &["status", &plan_file]);
    let command = review
        .lines()
        .find(|line| line.starts_with("command  "))
        .expect("the identity is shortened, so the command is shown");
    assert!(
        command.starts_with("command  stdio:[\"sh\",\"-c\",\"echo started >> "),
        "{command}"
    );
    assert!(
        command.ends_with("--stdio --ledger \\\"$LEDGER\\\"\"]"),
        "{command}"
    );

    // With the approval the server is started, once.
    let (code, done) = approve(&scratch, &plan_file, &[]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")), "{done}");
    assert_eq!(lines(&scratch, "STARTED"), 2);
    assert_eq!(lines(&scratch, "ledger.jsonl"), 1);
}

#[test]
fn a_failure_after_the_approval_records_nothing() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let plan_file = plan(&scratch, &scratch.file("server.json"), "both", PROGRAM);
    let digest = digest(&scratch, &plan_file);
    // The server is gone when the approved apply connects.
    drop(server);
    let failed = exits(&scratch, 2, &["apply", &plan_file, "--approve", &digest]);
    let message = stderr(&failed);
    assert!(
        message.contains("the plan was approved, but nothing was recorded or sent: "),
        "{message}"
    );
    assert!(!scratch.file("both.plan.journal.db").exists(), "no journal");
    assert_eq!(status(&scratch, &plan_file)["state"], "not_approved");
    assert!(ledger(&scratch).is_empty());
}

#[test]
fn a_stdio_servers_standard_error_cannot_reach_the_terminal_as_it_is() {
    let scratch = Scratch::new();
    let hostile = "\u{1b}[2J\u{1b}[HAPPROVED \u{202e}desrever token=env-s3cret-value";
    scratch.write(
        "server.json",
        &json!({
            "command": "sh",
            "args": ["-c", format!("printf '%s\\n' \"$NOISE\" >&2; exec {FIXTURE} --stdio")],
            "env": {"NOISE": hostile, "TOKEN": "env-s3cret-value"},
            "trust_annotations": true,
        })
        .to_string(),
    );
    let listed = exits(&scratch, 0, &["tools", "--server", "server.json"]);
    let shown = stderr(&listed);
    assert_printable(&shown, "the server's standard error");
    // The whole line is a configured value, so it is removed; what the
    // server adds around a configured value is escaped.
    assert!(shown.contains("server: [redacted]"), "{shown}");
    assert!(!shown.contains("env-s3cret-value"), "{shown}");

    scratch.write(
        "plain.json",
        &json!({
            "command": "sh",
            "args": ["-c", format!("printf 'ready\\033[31m \\342\\200\\256x\\n' >&2; exec {FIXTURE} --stdio")],
        })
        .to_string(),
    );
    let listed = exits(&scratch, 0, &["tools", "--server", "plain.json"]);
    let shown = stderr(&listed);
    assert_printable(&shown, "the server's standard error");
    assert!(
        shown.contains("server: ready\\u{1b}[31m \\u{202e}x\n"),
        "{shown}"
    );
}
