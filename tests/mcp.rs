//! The MCP client through the real binaries, against the offline MCP fixture
//! server in both protocol eras and on both transports: tool classes, what a
//! program may do, and what happens to each answer a write can get. The
//! fixture's own ledger shows which effects were applied, and its call log
//! which calls it received, whatever codemode believes.
#![cfg(all(unix, feature = "testkit"))]

mod support;

use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::*;

fn classes(scratch: &Scratch, server_file: &std::path::Path) -> Vec<(String, String)> {
    let listing = json_of(&ok(
        scratch,
        &["tools", "--server", path(server_file), "--json"],
    ));
    listing["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("name").to_string(),
                tool["class"].as_str().expect("class").to_string(),
            )
        })
        .collect()
}

#[test]
fn tools_are_writes_until_a_person_trusts_the_server() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--page-size", "2"]);
    let untrusted = scratch.write(
        "untrusted.json",
        &json!({"url": server.url, "read_tools": ["notes.get"]}).to_string(),
    );
    let found = classes(&scratch, &untrusted);
    assert_eq!(found.len(), 9, "every page is read: {found:?}");
    for (name, class) in &found {
        let expected = if name == "notes.get" { "read" } else { "write" };
        assert_eq!(class, expected, "{name}");
    }
    let text = ok(&scratch, &["tools", "--server", path(&untrusted)]);
    assert!(
        text.contains("trust_annotations is off: every tool outside read_tools is a write"),
        "{text}"
    );
    let bare = scratch.write("bare.json", &json!({"url": server.url}).to_string());
    let text = ok(&scratch, &["tools", "--server", path(&bare)]);
    assert!(
        text.contains(
            "trust_annotations is off: every tool is a write; set trust_annotations or \
             read_tools to let a program read."
        ),
        "{text}"
    );

    let trusted = http_file(&scratch, &server);
    let class = |name: &str| {
        classes(&scratch, &trusted)
            .into_iter()
            .find(|(tool, _)| tool == name)
            .expect("listed")
            .1
    };
    assert_eq!(class("notes.list"), "read");
    assert_eq!(class("notes.put"), "repeatable_write");
    assert_eq!(class("notes.delete"), "repeatable_write");
    assert_eq!(class("notes.append"), "write");
    assert_eq!(class("plain.unannotated"), "write");
    // The text says what the server marks, and has no line about trust.
    let text = ok(&scratch, &["tools", "--server", path(&trusted)]);
    assert!(
        text.contains("  notes.delete       repeatable write  destructive      yes\n"),
        "{text}"
    );
    assert!(
        text.contains("  plain.unannotated  write             no annotations   yes\n"),
        "{text}"
    );
    assert!(text.contains(
        "  notes.list         read              -                no: a program may call it\n"
    ));
    assert!(!text.contains("trust_annotations") && !text.contains("write_tools"));

    for key in ["read_tools", "write_tools"] {
        let unknown = scratch.write(
            "unknown.json",
            &json!({"url": server.url, key: ["nope"]}).to_string(),
        );
        let output = exits(&scratch, 2, &["tools", "--server", path(&unknown)]);
        let stderr = stderr(&output);
        assert!(
            stderr.contains(&format!(
                "\"{key}\" names \"nope\", which the server does not list"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn write_tools_decides_which_writes_a_plan_may_hold() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    let narrow = scratch.write(
        "server.json",
        &json!({"url": server.url, "trust_annotations": true, "write_tools": ["notes.put"]})
            .to_string(),
    );
    let listing = json_of(&ok(
        &scratch,
        &["tools", "--server", path(&narrow), "--json"],
    ));
    let plannable: Vec<&str> = listing["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter(|tool| tool["plannable"] == true)
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(plannable, ["notes.put"]);
    let text = ok(&scratch, &["tools", "--server", path(&narrow)]);
    assert!(
        text.contains(
            "notes.append       write             -                no: not in write_tools"
        )
    );
    assert!(
        text.contains("write_tools allows 1 of 4 write tools in a plan."),
        "{text}"
    );

    // `plan` refuses a write outside the list, from a program or from a file.
    scratch.write("program.ts", PROGRAM);
    let refused = exits(
        &scratch,
        2,
        &["plan", "--server", "server.json", "--program", "program.ts"],
    );
    let message = stderr(&refused);
    assert!(
        message.contains("write \"append\": it names \"notes.append\", which \"write_tools\" in the server file does not allow"),
        "{message}"
    );
    assert!(!scratch.file("program.plan.json").exists());
    scratch.write(
        "allowed.json",
        r#"{"operations": [{"id": "put", "tool": "notes.put", "arguments": {"id": "a", "text": "one"}}]}"#,
    );
    ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--operations",
            "allowed.json",
        ],
    );

    // A plan made with another server file, one without the list, cannot be
    // applied through the operator's file: the plan names the file it was
    // made with.
    let open = scratch.write(
        "open.json",
        &json!({"url": server.url, "trust_annotations": true}).to_string(),
    );
    let made = plan(&scratch, &open, "both", PROGRAM);
    let digest = digest(&scratch, &made);
    let refused = exits(
        &scratch,
        2,
        &[
            "apply",
            &made,
            "--approve",
            &digest,
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
    assert!(ledger(&scratch).is_empty(), "nothing was sent");
    assert!(
        !scratch.file("both.plan.journal.db").exists(),
        "nothing was approved"
    );
}

#[test]
fn each_era_and_transport_agrees_on_a_version() {
    let scratch = Scratch::new();
    for (era, expected) in [
        ("modern", "2026-07-28"),
        ("legacy", "2025-11-25"),
        ("dual", "2026-07-28"),
    ] {
        let server = serve(&scratch, era, &["--era", era]);
        let file = scratch.write(
            &format!("{era}.json"),
            &json!({"url": server.url}).to_string(),
        );
        let listing = json_of(&ok(&scratch, &["tools", "--server", path(&file), "--json"]));
        assert_eq!(listing["protocol_version"], expected, "http {era}");
        assert_eq!(listing["tools"].as_array().expect("tools").len(), 9);

        let stdio = scratch.write(
            &format!("{era}-stdio.json"),
            &json!({"command": FIXTURE, "args": ["--stdio", "--era", era]}).to_string(),
        );
        let listing = json_of(&ok(
            &scratch,
            &["tools", "--server", path(&stdio), "--json"],
        ));
        assert_eq!(listing["protocol_version"], expected, "stdio {era}");
        assert_eq!(listing["tools"].as_array().expect("tools").len(), 9);
    }
}

#[test]
fn a_header_comes_from_the_environment_and_is_never_printed() {
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--require-header", "x-api-key=s3cret-value"],
    );
    let file = scratch.write(
        "server.json",
        &json!({"url": server.url, "headers": {"x-api-key": "${FIXTURE_KEY}"}}).to_string(),
    );
    let run = |key: Option<&str>| {
        let mut command = command(&scratch, &["tools", "--server", path(&file)]);
        command.env_remove("FIXTURE_KEY");
        if let Some(key) = key {
            command.env("FIXTURE_KEY", key);
        }
        command.output().expect("starts")
    };
    let missing = run(None);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("FIXTURE_KEY is not set"));

    let wrong = run(Some("wrong-value"));
    assert_eq!(wrong.status.code(), Some(2));
    let message = stderr(&wrong);
    assert!(message.contains("401"), "{message}");
    assert!(!message.contains("wrong-value"));

    let right = run(Some("s3cret-value"));
    assert!(right.status.success(), "{right:?}");
    let all = format!("{}{}", stdout(&right), stderr(&right));
    assert!(all.contains("notes.list") && !all.contains("s3cret-value"));
}

#[test]
fn a_program_reads_and_proposes_but_cannot_write() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--sse"]);
    http_file(&scratch, &server);
    scratch.write(
        "program.ts",
        r#"
const text = await tools.call("raw.text", {});
const parsed = await tools.call("raw.json_text", {});
const echoed = await tools.call("region.echo", { region: "Zürich 北", query: "q" });
let denied = "allowed";
try { await tools.call("notes.append", { id: "x", text: "sneaky" }); } catch (error) { denied = "denied"; }
let missing = "found";
try { await tools.call("notes.get", { id: "absent" }); } catch (error) { missing = error.code; }
return { operations: [{ id: "record", tool: "notes.put", arguments: {
  id: "seen", text: [text, parsed.answer, echoed.region, denied, missing, input.who].join("|") } }] };
"#,
    );
    scratch.write("input.json", r#"{"who": "me", "__proto__": {"x": 1}}"#);
    let result = json_of(&ok(
        &scratch,
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "program.ts",
            "--out",
            "seen.json",
            "--input",
            "input.json",
            "--trace",
            "trace.json",
            "--json",
        ],
    ));
    assert_eq!(result["file"], "seen.json");
    assert_eq!(result["state"], "not_approved");
    assert_eq!(result["counts"]["total"], 1);
    let plan = json_of(&scratch.read("seen.json"));
    let operation = &plan["operations"][0];
    assert_eq!(
        operation["arguments"]["text"],
        "plain words, not JSON|42|Zürich 北|denied|tool_error|me"
    );
    assert_eq!(
        plan["server"],
        json!({"identity": server.url, "file": "server.json",
               "config": codemode::Digest::of(scratch.read("server.json").as_bytes()),
               "protocol": "2026-07-28", "trust_annotations": true})
    );
    assert_eq!(plan["tools"]["notes.put"]["class"], "repeatable_write");
    assert!(ledger(&scratch).is_empty(), "the program applied nothing");
    assert!(scratch.file("trace.json").exists());
    assert!(!scratch.file("seen.journal.db").exists());
}

#[test]
fn a_proposal_outside_what_the_server_offers_is_refused_before_review() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    http_file(&scratch, &server);
    let one = |tool: &str, arguments: Value| json!({"operations": [{"id": "a", "tool": tool, "arguments": arguments}]});
    let cases = [
        (one("notes.list", json!({})), "a read tool"),
        (one("nope", json!({})), "does not list"),
        (one("notes.put", json!({"id": "a"})), "\"text\" is missing"),
        (
            one("notes.put", json!({"id": "a", "text": "t", "extra": 1})),
            "not in the tool's schema",
        ),
        (
            json!({"operations": [{"id": "a", "tool": "notes.put",
                   "arguments": {"id": "a", "text": "t"}, "preconditions": [{}]}]}),
            "unknown key \"preconditions\"",
        ),
        (
            json!({"operations": [
                {"id": "a", "tool": "notes.put", "arguments": {"id": "a", "text": "t"}},
                {"id": "a", "tool": "notes.put", "arguments": {"id": "b", "text": "t"}}]}),
            "operation id 'a' appears twice",
        ),
        (json!([]), "a change list is"),
    ];
    for (list, expected) in cases {
        // The same validation, whether a program returns the list or a file holds it.
        scratch.write("program.ts", &format!("return {list};"));
        scratch.write("list.json", &list.to_string());
        for source in [["--program", "program.ts"], ["--operations", "list.json"]] {
            let output = exits(
                &scratch,
                2,
                &[
                    "plan",
                    "--server",
                    "server.json",
                    source[0],
                    source[1],
                    "--out",
                    "out.json",
                ],
            );
            let message = stderr(&output);
            assert!(
                message.contains(expected),
                "{source:?}: {message} should mention {expected}"
            );
            assert!(
                !scratch.file("out.json").exists(),
                "no plan is written for {expected}"
            );
        }
    }
    assert!(ledger(&scratch).is_empty());
}

/// Sort a ledger, because concurrent dispatch has no fixed order.
fn sorted(mut ledger: Vec<(String, Value)>) -> Vec<(String, Value)> {
    ledger.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_string().cmp(&b.1.to_string())));
    ledger
}

#[test]
fn an_approved_plan_is_applied_once_on_each_transport_and_era() {
    for era in ["modern", "legacy"] {
        let scratch = Scratch::new();
        let server = serve(&scratch, "s", &["--era", era, "--sse"]);
        let file = http_file(&scratch, &server);
        let plan = plan(&scratch, &file, "both", PROGRAM);
        let (code, status) = approve(&scratch, &plan, &[]);
        assert_eq!(
            (code, status["state"].as_str()),
            (0, Some("done")),
            "{status}"
        );
        assert_eq!(
            sorted(ledger(&scratch)),
            [
                (
                    "notes.append".to_string(),
                    json!({"id": "log", "text": "line"})
                ),
                ("notes.put".to_string(), json!({"id": "a", "text": "one"})),
            ],
            "{era}"
        );
        // A second apply has nothing left: it prints the status and sends nothing.
        let (code, again) = apply(&scratch, &plan, &[]);
        assert_eq!((code, &again), (0, &status));
        assert_eq!(ledger(&scratch).len(), 2);
    }

    // stdio: every command starts the server again, so its state is a file.
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &[]);
    let plan = plan(&scratch, &file, "both", PROGRAM);
    let (code, status) = approve(&scratch, &plan, &[]);
    assert_eq!(
        (code, status["state"].as_str()),
        (0, Some("done")),
        "{status}"
    );
    assert_eq!(ledger(&scratch).len(), 2);
}

#[test]
fn a_lost_reply_repeats_only_the_write_the_server_calls_idempotent() {
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &[
            "--fault",
            "notes.put=drop-reply",
            "--fault",
            "notes.append=drop-reply",
        ],
    );
    let file = http_file(&scratch, &server);
    let plan = plan(&scratch, &file, "both", PROGRAM);

    // Both effects landed; codemode saw neither reply. Without the limit the
    // session would stop after the first lost reply and never send the second.
    let (code, first) = approve(
        &scratch,
        &plan,
        &["--concurrency", "1", "--max-failures", "0"],
    );
    assert_eq!(code, 3, "{first}");
    assert_eq!(standing(&first, "put"), ("no_reply".into(), "apply".into()));
    assert_eq!(
        standing(&first, "append"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(write(&first, "append")["error_code"], "no_response");
    let text = ok(&scratch, &["status", &plan]);
    assert!(
        text.contains(
            "no reply (2): the write may have reached the server, and no answer came back"
        ),
        "{text}"
    );
    assert!(
        text.contains("1 write is repeatable: `codemode apply both.plan.json` sends it again (up to 3 attempts each)."),
        "{text}"
    );
    assert!(
        text.contains(
            "codemode resolve both.plan.json --no-reply --as not-applied   (or --as applied)"
        ),
        "{text}"
    );

    let (code, second) = apply(&scratch, &plan, &[]);
    assert_eq!(code, 3, "{second}");
    assert_eq!(standing(&second, "put"), ("applied".into(), "none".into()));
    assert_eq!(write(&second, "put")["attempts"], 2);
    assert_eq!(
        standing(&second, "append"),
        ("no_reply".into(), "resolve".into())
    );

    let applied = ledger(&scratch);
    let count = |tool: &str| applied.iter().filter(|(name, _)| name == tool).count();
    assert_eq!(count("notes.put"), 2, "the idempotent write was sent again");
    assert_eq!(
        count("notes.append"),
        1,
        "the other write was never repeated"
    );

    // A person settles the append; nothing more is sent.
    let recorded = ok(
        &scratch,
        &[
            "resolve",
            &plan,
            "append",
            "--as",
            "applied",
            "--reviewer",
            "tester",
        ],
    );
    assert!(
        recorded.ends_with("Recorded 1 write. Every write is applied or resolved.\n"),
        "{recorded}"
    );
    let (code, done) = apply(&scratch, &plan, &[]);
    assert_eq!((code, done["state"].as_str()), (0, Some("done")), "{done}");
    assert_eq!(
        standing(&done, "append"),
        ("resolved".into(), "none".into())
    );
    assert_eq!(write(&done, "append")["resolved_by"], "tester");
    assert_eq!(ledger(&scratch).len(), 3);
}

#[test]
fn a_tool_error_or_a_server_error_leaves_a_write_for_a_person() {
    // The fault, the attempt's code, the write's state, and the text kept.
    let cases = [
        ("is-error", "tool_error", "failed", "upstream said no"),
        (
            "rpc-error",
            "rpc_-32603",
            "failed",
            "the server returned error -32603: internal error",
        ),
        (
            "http-500",
            "http_500",
            "failed",
            "the server returned HTTP status 500 Internal Server Error",
        ),
        (
            "input-required",
            "input_required",
            "failed",
            "the server asked for input",
        ),
        (
            "no-effect-drop",
            "no_response",
            "no_reply",
            "no response arrived",
        ),
    ];
    for (fault, code, state, text) in cases {
        let scratch = Scratch::new();
        let server = serve(
            &scratch,
            "s",
            &["--fault", &format!("notes.append={fault},sticky")],
        );
        let file = http_file(&scratch, &server);
        let plan = plan(&scratch, &file, "both", PROGRAM);
        let (exit, status) = approve(&scratch, &plan, &[]);
        assert_eq!(exit, 3, "{fault}: {status}");
        assert_eq!(standing(&status, "put"), ("applied".into(), "none".into()));
        assert_eq!(
            standing(&status, "append"),
            (state.into(), "resolve".into()),
            "{fault}"
        );

        // The status says why, in the server's words.
        let failed = write(&status, "append");
        assert_eq!(failed["error_code"], code, "{fault}");
        assert_eq!(failed["answered"], state == "failed", "{fault}");
        assert_eq!(failed["attempts"], 1, "{fault}");
        let message = failed["error_text"].as_str().expect("text");
        assert!(message.starts_with(text), "{fault}: {message}");
        let applied = write(&status, "put");
        assert_eq!(
            (&applied["error_code"], &applied["error_text"]),
            (&Value::Null, &Value::Null)
        );
        assert_eq!(status["detail"], "finished with 1 failure");
        assert_eq!(status["state"], "needs_attention");

        // So does the text, with the exact command to run next.
        let shown = ok(&scratch, &["status", &plan]);
        assert!(
            shown.contains(&format!("\n  append  notes.append\n    \"{text}")),
            "{fault}: {shown}"
        );
        assert!(
            shown.contains("      codemode resolve both.plan.json append --as not-applied   (or --as applied)\n"),
            "{fault}: {shown}"
        );

        // Another apply sends nothing: the write waits for a person.
        let (exit, again) = apply(&scratch, &plan, &[]);
        assert_eq!((exit, &again), (3, &status), "{fault}");
        assert_eq!(
            ledger(&scratch),
            [("notes.put".to_string(), json!({"id": "a", "text": "one"}))],
            "{fault}: the failed write was sent once and never applied"
        );
        assert_eq!(calls(&scratch, "notes.append").len(), 1, "{fault}");
    }
}

#[test]
fn a_session_stops_after_the_first_failed_write_and_apply_sends_the_rest() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=is-error,sticky"]);
    let file = http_file(&scratch, &server);
    let plan = plan(
        &scratch,
        &file,
        "appends",
        &proposing(12, "append", "notes.append"),
    );

    let (code, first) = approve(&scratch, &plan, &["--concurrency", "1"]);
    assert_eq!(
        (code, first["state"].as_str()),
        (3, Some("needs_attention"))
    );
    assert_eq!(first["detail"], "stopped after 1 failure");
    assert_eq!(
        first["counts"],
        json!({"total": 12, "applied": 0, "failed": 1, "no_reply": 0, "refused": 0,
               "resolved": 0, "not_sent": 11})
    );
    assert_eq!(
        calls(&scratch, "notes.append"),
        ["is-error"],
        "the server received one write, not twelve"
    );
    assert_eq!(
        standing(&first, "append-0"),
        ("failed".into(), "resolve".into())
    );
    for index in 1..12 {
        let id = format!("append-{index}");
        assert_eq!(
            standing(&first, &id),
            ("not_sent".into(), "apply".into()),
            "{id}"
        );
        assert_eq!(write(&first, &id)["attempts"], 0, "{id}");
    }

    // A higher limit sends until that many more have failed.
    let (code, second) = apply(&scratch, &plan, &["--max-failures", "4"]);
    assert_eq!(code, 3);
    assert_eq!(second["detail"], "stopped after 5 failures");
    assert_eq!(second["counts"]["not_sent"], 7);
    assert_eq!(calls(&scratch, "notes.append").len(), 5);

    // 0 never stops: every remaining write is sent.
    let (code, third) = apply(&scratch, &plan, &["--max-failures", "0"]);
    assert_eq!(code, 3);
    assert_eq!(third["detail"], "finished with 12 failures");
    assert_eq!(calls(&scratch, "notes.append").len(), 12);
    assert!(ledger(&scratch).is_empty(), "no write was applied");

    // Every failed write is selected at once, with its error text shown once.
    let recorded = ok(
        &scratch,
        &[
            "resolve",
            &plan,
            "--errors",
            "--as",
            "not-applied",
            "--reviewer",
            "tester",
        ],
    );
    assert!(
        recorded.starts_with(
            "Recording 12 writes as not applied (reviewer tester):\n\n  12 writes  notes.append\n    \"upstream said no\"\n    append-0, append-1,"
        ),
        "{recorded}"
    );
    assert!(recorded.ends_with("Recorded 12 writes. Next: codemode apply appends.plan.json\n"));
    let after = status(&scratch, &plan);
    assert_eq!(after["counts"]["not_sent"], 12);

    for bad in [["--max-failures", "few"], ["--deadline-ms", "0"]] {
        exits(&scratch, 2, &["apply", &plan, bad[0], bad[1]]);
    }
    // The approval binds these; a later run cannot change them.
    let late = exits(&scratch, 2, &["apply", &plan, "--concurrency", "8"]);
    assert!(stderr(&late).contains("accepted on the first run only"));
}

#[test]
fn the_first_write_goes_alone_and_the_others_follow_its_answer() {
    // Every append fails. The default policy sends four at a time, but the
    // first write is sent alone, so the server receives one write.
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=is-error,sticky"]);
    let file = http_file(&scratch, &server);
    let plan = plan(
        &scratch,
        &file,
        "appends",
        &proposing(12, "append", "notes.append"),
    );
    let (code, status) = approve(&scratch, &plan, &[]);
    assert_eq!(code, 3, "{status}");
    assert_eq!(calls(&scratch, "notes.append").len(), 1);
    assert_eq!(status["counts"]["failed"], 1);
    assert_eq!(status["counts"]["not_sent"], 11);

    // The first write is applied, so the others are sent four at a time. They
    // fail; the ones already sent finish and are recorded.
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=is-error,sticky"]);
    let file = http_file(&scratch, &server);
    let program = r#"
const append = (i) => ({ id: `append-${i}`, tool: "notes.append", arguments: { id: "log", text: `${i}` } });
return { operations: [
  { id: "put", tool: "notes.put", arguments: { id: "a", text: "one" } },
  ...Array.from({ length: 11 }, (_, i) => append(i)),
] };"#;
    let plan = support::plan(&scratch, &file, "mixed", program);
    let (code, status) = approve(&scratch, &plan, &[]);
    assert_eq!(code, 3, "{status}");
    assert_eq!(standing(&status, "put"), ("applied".into(), "none".into()));
    let sent = calls(&scratch, "notes.append").len();
    assert!((1..=4).contains(&sent), "{sent} failing writes were sent");
    assert_eq!(
        status["counts"]["failed"], sent,
        "each call that was sent is recorded"
    );
    assert_eq!(status["counts"]["not_sent"], 11 - sent);
}

const HEADER_SECRET: &str = "header-s3cret-value";
const QUERY_SECRET: &str = "tenant-s3cret-id";
const ENV_SECRET: &str = "env-s3cret-value";

/// Everything codemode printed or stored for the plan `name` in `scratch`.
fn everything(scratch: &Scratch, name: &str, outputs: &[&Output]) -> String {
    let mut all = String::new();
    for output in outputs {
        all.push_str(&stdout(output));
        all.push_str(&stderr(output));
    }
    for suffix in ["plan.json", "plan.journal.db", "plan.journal.db-wal"] {
        if let Ok(bytes) = std::fs::read(scratch.file(&format!("{name}.{suffix}"))) {
            all.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    let plan = format!("{name}.plan.json");
    for extra in [&[][..], &["--full"], &["--json"]] {
        let mut args = vec!["status", plan.as_str()];
        args.extend_from_slice(extra);
        all.push_str(&ok(scratch, &args));
    }
    all
}

#[test]
fn the_servers_error_text_never_carries_a_configured_value() {
    // Streamable HTTP: the server repeats the request target and every header.
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &[
            "--require-header",
            &format!("x-api-key={HEADER_SECRET}"),
            "--fault",
            "notes.append=is-error-echo",
        ],
    );
    let file = scratch.write(
        "server.json",
        &json!({"url": format!("{}?tenant={QUERY_SECRET}", server.url),
                "headers": {"x-api-key": HEADER_SECRET}, "trust_annotations": true})
        .to_string(),
    );
    let plan_file = plan(&scratch, &file, "both", PROGRAM);
    let digest = digest(&scratch, &plan_file);
    let run = exits(
        &scratch,
        3,
        &["apply", &plan_file, "--approve", &digest, "--json"],
    );
    let status = json_of(&stdout(&run));
    let message = write(&status, "append")["error_text"]
        .as_str()
        .expect("text")
        .to_string();
    // The query and the header value, wherever the server put them.
    assert!(
        message.starts_with("upstream said no to /mcp?[redacted] ")
            && message.matches("[redacted]").count() == 2,
        "{message}"
    );
    let all = everything(&scratch, "both", &[&run]);
    assert!(all.contains("upstream said no to"), "the text is kept");
    assert!(!all.contains(HEADER_SECRET) && !all.contains(QUERY_SECRET));

    // stdio: the server repeats a variable of its environment.
    let scratch = Scratch::new();
    let state = scratch.file("state.json");
    let file = scratch.write(
        "server.json",
        &json!({
            "command": FIXTURE,
            "args": ["--stdio", "--state", path(&state), "--fault", "notes.append=is-error-echo"],
            "env": {"CODEMODE_MCP_FIXTURE_ECHO": ENV_SECRET},
            "trust_annotations": true,
        })
        .to_string(),
    );
    let plan_file = plan(&scratch, &file, "both", PROGRAM);
    let digest = support::digest(&scratch, &plan_file);
    let run = exits(&scratch, 3, &["apply", &plan_file, "--approve", &digest]);
    let all = everything(&scratch, "both", &[&run]);
    assert!(all.contains("upstream said no to [redacted]"), "{all}");
    assert!(!all.contains(ENV_SECRET));

    // A program sees the same text.
    scratch.write(
        "leak.ts",
        r#"let seen = "none";
try { await tools.call("notes.append", { id: "x", text: "y" }); } catch (error) { seen = error.message; }
return { operations: [{ id: "note", tool: "notes.put", arguments: { id: "seen", text: seen } }] };"#,
    );
    scratch.write(
        "reads.json",
        &json!({
            "command": FIXTURE,
            "args": ["--stdio", "--fault", "notes.append=is-error-echo"],
            "env": {"CODEMODE_MCP_FIXTURE_ECHO": ENV_SECRET},
            "read_tools": ["notes.append"],
        })
        .to_string(),
    );
    let planned = exits(
        &scratch,
        0,
        &["plan", "--server", "reads.json", "--program", "leak.ts"],
    );
    let seen = format!("{}{}", scratch.read("leak.plan.json"), stdout(&planned));
    assert!(seen.contains("upstream said no to [redacted]"), "{seen}");
    assert!(!seen.contains(ENV_SECRET));
}

#[test]
fn a_rate_limited_write_waits_and_is_sent_again() {
    let scratch = Scratch::new();
    // One refusal that asks for a one-second wait; `notes.append` is a `write`.
    let server = serve(&scratch, "s", &["--fault", "notes.append=http-429:1"]);
    let file = http_file(&scratch, &server);
    let plan = plan(&scratch, &file, "both", PROGRAM);
    let started = Instant::now();
    let (code, status) = approve(&scratch, &plan, &["--concurrency", "1"]);
    assert_eq!(
        (code, status["state"].as_str()),
        (0, Some("done")),
        "{status}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the session waited as Retry-After asked"
    );
    assert_eq!(write(&status, "append")["attempts"], 2);
    assert_eq!(write(&status, "append")["error_code"], Value::Null);
    assert_eq!(calls(&scratch, "notes.append"), ["http-429", "run"]);
    assert_eq!(ledger(&scratch).len(), 2, "each write was applied once");
}

#[test]
fn a_lasting_rate_limit_ends_the_write_as_not_applied() {
    // Every call is refused at once, with no wait asked for.
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--fault", "notes.append=http-429:0,sticky"],
    );
    let file = http_file(&scratch, &server);
    let program = proposing(3, "append", "notes.append");
    let plan_file = plan(&scratch, &file, "appends", &program);
    let (code, status) = approve(&scratch, &plan_file, &["--concurrency", "1"]);
    assert_eq!(code, 3, "{status}");
    assert_eq!(
        calls(&scratch, "notes.append"),
        ["http-429", "http-429", "http-429"],
        "the approved budget is three attempts"
    );
    let spent = write(&status, "append-0");
    assert_eq!(
        (
            &spent["state"],
            &spent["next"],
            &spent["attempts"],
            &spent["error_code"]
        ),
        (
            &json!("refused"),
            &json!("new_plan"),
            &json!(3),
            &json!("rate_limited")
        )
    );
    let text = spent["error_text"].as_str().expect("text");
    assert!(
        text.starts_with("the server returned HTTP status 429 Too Many Requests"),
        "{text}"
    );
    assert_eq!(status["counts"]["not_sent"], 2);
    assert!(ledger(&scratch).is_empty());
    let text = ok(&scratch, &["status", &plan_file]);
    assert!(
        text.contains("refused (1): not applied, and not sent again"),
        "{text}"
    );
    assert!(
        text.contains(
            "Every attempt of 1 write is used. Make a new plan for it with `codemode plan`."
        ),
        "{text}"
    );

    // A wait that would pass the deadline ends the write at once instead.
    let scratch = Scratch::new();
    let server = serve(
        &scratch,
        "s",
        &["--fault", "notes.append=http-429:30,sticky"],
    );
    let file = http_file(&scratch, &server);
    let plan_file = plan(&scratch, &file, "appends", &program);
    let started = Instant::now();
    let (code, status) = approve(
        &scratch,
        &plan_file,
        &["--concurrency", "1", "--deadline-ms", "8000"],
    );
    assert_eq!(code, 3, "{status}");
    assert!(started.elapsed() < Duration::from_secs(6), "{status}");
    let limited = write(&status, "append-0");
    assert_eq!(
        (&limited["state"], &limited["next"], &limited["attempts"]),
        (&json!("not_sent"), &json!("apply"), &json!(1))
    );
    assert_eq!(limited["error_code"], "rate_limited");
    assert_eq!(status["state"], "incomplete");
    assert_eq!(calls(&scratch, "notes.append"), ["http-429"]);
}

#[test]
fn nothing_is_sent_to_the_server_while_a_rate_limit_lasts() {
    let scratch = Scratch::new();
    // Every put takes 400 ms. The append is refused after 200 ms, and for one
    // second from then the server refuses every call it receives.
    let server = serve(
        &scratch,
        "s",
        &[
            "--fault",
            "notes.put=delay-ms:400,sticky",
            "--fault",
            "notes.append=delay-ms:200,http-429:1",
        ],
    );
    let file = http_file(&scratch, &server);
    // The first write goes alone; the rate limit comes among the others.
    let program = r#"
const put = (i) => ({ id: `put-${i}`, tool: "notes.put", arguments: { id: `n${i}`, text: "t" } });
return { operations: [
  put(0),
  put(1),
  { id: "append", tool: "notes.append", arguments: { id: "log", text: "line" } },
  put(2), put(3), put(4),
] };"#;
    let plan = plan(&scratch, &file, "mixed", program);
    let (code, status) = approve(&scratch, &plan, &["--concurrency", "2"]);
    assert_eq!(
        (code, status["state"].as_str()),
        (0, Some("done")),
        "{status}"
    );
    // put-1 answered during the wait, which freed its slot for put-2. Had
    // put-2 been sent then, the server would have refused it.
    assert_eq!(calls(&scratch, "notes.append"), ["http-429", "run"]);
    assert_eq!(
        calls(&scratch, "notes.put"),
        ["run", "run", "run", "run", "run"]
    );
    for index in 0..5 {
        let id = format!("put-{index}");
        assert_eq!(write(&status, &id)["attempts"], 1, "{id}");
    }
    assert_eq!(write(&status, "append")["attempts"], 2);
    assert_eq!(ledger(&scratch).len(), 6);
}

#[test]
fn a_program_returns_thousands_of_writes_and_hundreds_are_applied() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    let file = http_file(&scratch, &server);
    // A program's result may be as large as a plan: 4,000 writes are about
    // 400 KiB, several times the 64 KiB of one tool call.
    let large = plan(
        &scratch,
        &file,
        "large",
        &proposing(4000, "put", "notes.put"),
    );
    let text = scratch.read(&large);
    assert!(text.len() > 256 * 1024, "{} bytes", text.len());
    let status = status(&scratch, &large);
    assert_eq!(status["counts"]["total"], 4000);
    assert_eq!(write(&status, "put-3999")["tool"], "notes.put");
    // The budget still bounds what a program may return.
    scratch.write("large.ts", &proposing(4000, "put", "notes.put"));
    let over = exits(
        &scratch,
        2,
        &[
            "plan",
            "--server",
            "server.json",
            "--program",
            "large.ts",
            "--out",
            "over.json",
            "--max-output-bytes",
            "100000",
        ],
    );
    assert!(
        stderr(&over).contains("status output_limit"),
        "{}",
        stderr(&over)
    );
    assert!(!scratch.file("over.json").exists());

    // More than the 256 operations a plan could hold before.
    let plan = plan(&scratch, &file, "puts", &proposing(300, "put", "notes.put"));
    let (code, status) = approve(&scratch, &plan, &[]);
    assert_eq!(
        (code, status["state"].as_str()),
        (0, Some("done")),
        "{}",
        status["detail"]
    );
    assert_eq!(status["counts"]["applied"], 300);
    assert_eq!(ledger(&scratch).len(), 300);
}

#[test]
fn a_deadline_does_not_prove_that_a_slow_write_did_nothing() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &["--fault", "notes.append=delay-ms:1500"]);
    let file = http_file(&scratch, &server);
    let plan = plan(&scratch, &file, "both", PROGRAM);
    let (code, status) = approve(&scratch, &plan, &["--deadline-ms", "500"]);
    assert_eq!(code, 3, "{status}");
    assert_eq!(status["detail"], "stopped at the deadline");
    // No answer came back, whichever side noticed the deadline first.
    let slow = write(&status, "append");
    assert_eq!(
        (&slow["state"], &slow["error_code"], &slow["answered"]),
        (&json!("no_reply"), &json!("deadline"), &json!(false)),
        "{slow}"
    );

    // The server ignored the cancellation: the effect lands after the deadline.
    let started = Instant::now();
    while ledger(&scratch).len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the late effect never landed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, again) = apply(&scratch, &plan, &[]);
    assert_eq!(
        standing(&again, "append"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(ledger(&scratch).len(), 2, "apply did not send it again");
}

#[test]
fn an_abandoned_stdio_call_is_cancelled_and_stays_unknown() {
    let scratch = Scratch::new();
    let file = stdio_file(&scratch, &["--fault", "notes.append=delay-ms:1500,sticky"]);
    let plan = plan(&scratch, &file, "both", PROGRAM);
    let (code, status) = approve(&scratch, &plan, &["--deadline-ms", "700"]);
    assert_eq!(code, 3, "{status}");
    assert_eq!(standing(&status, "put").0, "applied");
    assert_eq!(standing(&status, "append").0, "no_reply");
    let appended = |scratch: &Scratch| {
        ledger(scratch)
            .iter()
            .filter(|(tool, _)| tool == "notes.append")
            .count()
    };
    // The fixture ignores the cancellation and applies the effect before it exits.
    let started = Instant::now();
    while appended(&scratch) < 1 {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the late effect never landed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, again) = apply(&scratch, &plan, &[]);
    assert_eq!(
        standing(&again, "append"),
        ("no_reply".into(), "resolve".into())
    );
    assert_eq!(appended(&scratch), 1, "apply did not send it again");
}

#[test]
fn another_server_file_is_refused_before_any_write() {
    let scratch = Scratch::new();
    let server = serve(&scratch, "s", &[]);
    let file = http_file(&scratch, &server);
    let plan = plan(&scratch, &file, "both", PROGRAM);
    let digest = digest(&scratch, &plan);

    // Another server, and the same server with another trust setting.
    let other = serve(&scratch, "other", &[]);
    scratch.write(
        "other.json",
        &json!({"url": other.url, "trust_annotations": true}).to_string(),
    );
    scratch.write("untrusted.json", &json!({"url": server.url}).to_string());
    for name in ["other.json", "untrusted.json"] {
        let refused = exits(
            &scratch,
            2,
            &["apply", &plan, "--approve", &digest, "--server", name],
        );
        let message = stderr(&refused);
        assert!(
            message.contains(&format!(
                "the server file {name} is not the one this plan was made with"
            )),
            "{message}"
        );
        assert!(message.contains("run `codemode plan` again"), "{message}");
    }
    assert!(ledger(&scratch).is_empty(), "a refused apply sends nothing");
    assert!(
        !scratch.file("both.plan.journal.db").exists(),
        "a refused apply approves nothing"
    );
}

/// A minimal MCP server over HTTP that repeats the `Authorization` header
/// and the request target in every error it answers. With `refuse_all` it
/// answers every request with HTTP 401 and such an error; without, it
/// accepts `server/discover` and fails `tools/list`. Returns its port.
fn echoing_server(refuse_all: bool) -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut target = String::new();
            let mut authorization = String::new();
            let mut length = 0;
            let mut line = String::new();
            let mut first = true;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if first {
                    target = line.split(' ').nth(1).unwrap_or_default().to_string();
                    first = false;
                } else if let Some((name, value)) = line.trim_end().split_once(": ") {
                    match name.to_ascii_lowercase().as_str() {
                        "authorization" => authorization = value.to_string(),
                        "content-length" => length = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let echo =
                format!("invalid credentials: authorization={authorization} target={target}");
            let error = json!({"jsonrpc": "2.0", "id": request["id"],
                               "error": {"code": -32001, "message": echo}});
            let (status, reply) = if refuse_all {
                ("401 Unauthorized", error)
            } else if request["method"] == "server/discover" {
                (
                    "200 OK",
                    json!({"jsonrpc": "2.0", "id": request["id"],
                           "result": {"supportedVersions": ["2026-07-28"]}}),
                )
            } else {
                ("200 OK", error)
            };
            let reply = reply.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    port
}

#[test]
fn a_connect_time_error_never_carries_a_configured_value() {
    const KEY: &str = "sk-live-SECRETSECRET123";
    const QUERY: &str = "QUERYSECRET9876";
    for refuse_all in [false, true] {
        let scratch = Scratch::new();
        let port = echoing_server(refuse_all);
        let server_text = json!({
            "url": format!("http://127.0.0.1:{port}/mcp?api_key={QUERY}"),
            "headers": {"authorization": "Bearer ${API_KEY}"},
        })
        .to_string();
        scratch.write("server.json", &server_text);
        scratch.write(
            "ops.json",
            r#"{"operations": [{"id": "a", "tool": "notes.put", "arguments": {"id": "a", "text": "b"}}]}"#,
        );
        // A plan for this server, made by hand: `plan` cannot list its tools.
        let identity = codemode::mcp::ServerConfig::parse(&server_text, |_| Some(KEY.into()))
            .expect("server file")
            .identity();
        let made = json!({
            "codemode_plan": 1,
            "server": {"identity": identity, "file": "server.json",
                       "config": codemode::Digest::of(server_text.as_bytes()),
                       "protocol": "2026-07-28", "trust_annotations": false},
            "tools": {"notes.put": {"class": "write", "definition": codemode::Digest::of(b"x"),
                                    "destructive": false, "annotated": false}},
            "operations": [{"id": "a", "tool": "notes.put", "arguments": {"id": "a", "text": "b"}}],
        });
        scratch.write("made.plan.json", &made.to_string());
        let digest = PlanDigest::of(&scratch);

        let commands: [&[&str]; 4] = [
            &["tools", "--server", "server.json"],
            &["tools", "--server", "server.json", "--json"],
            &[
                "plan",
                "--server",
                "server.json",
                "--operations",
                "ops.json",
            ],
            &["apply", "made.plan.json", "--approve", &digest.0],
        ];
        for args in commands {
            let output = command(&scratch, args)
                .env("API_KEY", KEY)
                .output()
                .expect("codemode starts");
            assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
            let all = format!("{}{}", stdout(&output), stderr(&output));
            assert!(
                all.contains(
                    "invalid credentials: authorization=[redacted] target=/mcp?[redacted]"
                ),
                "{args:?}: the server's words are kept: {all}"
            );
            assert!(
                !all.contains("SECRET"),
                "{args:?} (refuse_all {refuse_all}): {all}"
            );
        }
        assert!(!scratch.file("made.plan.journal.db").exists());
    }
}

/// The digest of `made.plan.json`, read without the environment the server
/// file needs.
struct PlanDigest(String);

impl PlanDigest {
    fn of(scratch: &Scratch) -> Self {
        let plan = codemode::plan_file::PlanFile::parse(&scratch.read("made.plan.json"))
            .expect("plan file");
        Self(plan.digest().to_string())
    }
}

#[test]
fn an_unreachable_server_is_a_startup_error() {
    let scratch = Scratch::new();
    let file = scratch.write("server.json", r#"{"url": "http://127.0.0.1:1/mcp"}"#);
    let output = exits(&scratch, 2, &["tools", "--server", path(&file)]);
    assert!(stderr(&output).contains("not sent"));

    let missing = scratch.write("missing.json", r#"{"command": "/nonexistent/mcp-server"}"#);
    let output = exits(&scratch, 2, &["tools", "--server", path(&missing)]);
    assert!(stderr(&output).contains("cannot start"));
}
