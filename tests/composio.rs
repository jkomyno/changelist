//! Offline checks of the Composio GitHub host's investigation step. They run the
//! production host through its test-only entry (`integrations/composio/testing/host.ts`)
//! under Deno with a stand-in for the `composio run` helper that keeps its own
//! ledger; no account or network is used. The live, read-only check is
//! `mise run composio-live-check`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CODEMODE: &str = env!("CARGO_BIN_EXE_codemode");
const ACCOUNT: &str = "ca_fake_bot";
const CREDENTIAL: &str = "codemode-composio-secret-value";
/// Text the stand-in puts in results, errors, logs, and signed URLs; none may be kept.
const PROVIDER_TEXT: [&str; 4] = [
    "MARK-SECRET",
    "ca_LEAKEDinERROR",
    "MARK-LOG-BODY",
    "MARK-SIGNED",
];
static SCRATCH: AtomicU64 = AtomicU64::new(0);

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// An absolute Deno path: the host resolves executables itself.
fn deno() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEMODE_TEST_DENO") {
        return path.into();
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("deno"))
        .find(|candidate| candidate.is_file())
        .expect("deno is on PATH")
}

struct Scratch(PathBuf);

impl Scratch {
    /// A scratch directory with the stand-in's GitHub, and `faults`.
    fn new(faults: Value) -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-composio-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("scratch");
        let issue = |title: &str, state: &str| json!({"title": title, "state": state, "labels": [], "comments": []});
        let state = json!({
            "accounts": {"ca_fake_bot": "widget-bot"},
            "repositories": {
                "acme/widgets": {
                    "id": 1001,
                    "labels": ["bug", "triage"],
                    "permissions": {"push": true, "triage": true},
                    "issues": {
                        "1": issue("CI fails on main", "open"),
                        "2": issue("Docs typo", "open"),
                        "3": issue("CI fails on main!", "open"),
                        "4": issue("CI fails on main", "closed"),
                    },
                    "runs": [{"id": 77, "name": "CI", "conclusion": "failure", "head_sha": "9359a12aaaa",
                              "jobs": [{"id": 5, "name": "check", "conclusion": "failure"},
                                       {"id": 6, "name": "msrv", "conclusion": "success"}]}],
                },
                "acme/private": {"id": 2002, "labels": [], "permissions": {"push": true, "triage": true},
                                 "issues": {"1": issue("secret", "open")}},
            },
            "faults": faults,
            "calls": [],
        });
        let scratch = Self(path);
        std::fs::write(scratch.file("github.json"), state.to_string()).expect("state");
        scratch
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Calls the stand-in received, as `slug target`.
    fn calls(&self) -> Vec<String> {
        let ledger: Value = serde_json::from_str(
            &std::fs::read_to_string(self.file("github.json")).expect("ledger"),
        )
        .expect("ledger is JSON");
        ledger["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|call| {
                assert_eq!(call["account"], ACCOUNT, "every call names the account");
                format!(
                    "{} {}",
                    call["slug"].as_str().unwrap_or(""),
                    call["target"].as_str().unwrap_or("")
                )
            })
            .collect()
    }

    /// Write a program to a scratch file and return its path.
    fn program(&self, source: &str) -> PathBuf {
        let path = self.file(&format!(
            "program-{}.ts",
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, source).expect("program written");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// `investigate` under Deno, offline, with a host credential in its own environment only.
fn investigate(scratch: &Scratch, program: &Path, account: &str) -> Command {
    let mut command = Command::new(deno());
    command
        .current_dir(root())
        .env("COMPOSIO_API_KEY", CREDENTIAL)
        .args([
            "run",
            "--allow-run",
            "--allow-read",
            "--allow-write",
            "--allow-env",
        ])
        .arg("integrations/composio/testing/host.ts")
        .args([
            "investigate",
            "--account",
            account,
            "--repo",
            "acme/widgets",
        ])
        .args(["--offline", path(&scratch.file("github.json"))])
        .args(["--bin", CODEMODE])
        .args(["--program", path(program)])
        .args(["--out", path(&scratch.file("plan.json"))])
        .args(["--trace", path(&scratch.file("trace.json"))])
        .stdin(Stdio::null());
    command
}

fn clean(text: &str, what: &str) {
    for kept in PROVIDER_TEXT.iter().chain([&CREDENTIAL]) {
        assert!(!text.contains(kept), "{what} kept {kept}: {text}");
    }
}

/// The host's summary line; its stdout and stderr must hold no provider text.
fn summary(output: &Output) -> Value {
    clean(&String::from_utf8_lossy(&output.stdout), "stdout");
    clean(&String::from_utf8_lossy(&output.stderr), "stderr");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "host prints one JSON summary; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The trace, which must hold no provider text, and its calls as `tool status code`.
fn trace(scratch: &Scratch) -> (Value, Vec<String>) {
    let text = std::fs::read_to_string(scratch.file("trace.json")).expect("trace written");
    clean(&text, "trace");
    let trace: Value = serde_json::from_str(&text).expect("trace is JSON");
    let outcomes = trace["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            let outcome = &call["outcome"];
            let code = outcome["error"]["code"].as_str().unwrap_or("");
            format!(
                "{} {} {code}",
                call["call"]["tool"].as_str().unwrap_or(""),
                outcome["status"].as_str().unwrap_or("")
            )
            .trim_end()
            .to_owned()
        })
        .collect();
    (trace, outcomes)
}

fn check(plan: &Path) -> Value {
    let output = Command::new(CODEMODE)
        .args(["plan", "check", path(plan), "--json"])
        .output()
        .expect("check");
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("check JSON")
}

#[test]
fn the_triage_example_becomes_a_checked_plan_with_marked_comments_and_no_program() {
    let scratch = Scratch::new(json!({}));
    let program = root().join("integrations/composio/examples/triage.ts");
    let output = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    let summary = summary(&output);
    assert!(output.status.success(), "{summary}");
    assert_eq!(summary["status"], "success");
    assert_eq!(summary["operations"], 4);

    let text = std::fs::read_to_string(scratch.file("plan.json")).expect("plan written");
    let plan: Value = serde_json::from_str(&text).expect("plan JSON");
    let checked = check(&scratch.file("plan.json"));
    assert_eq!(summary["digest"], checked["digest"]);
    let source = std::fs::read_to_string(&program).expect("program");
    assert!(
        !text.contains("Triage the open issues") && !text.contains(source.trim()),
        "the program is not part of the plan"
    );

    let operations = plan["operations"].as_array().expect("operations");
    let ids: Vec<&str> = operations
        .iter()
        .map(|operation| operation["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["label-1", "comment-1", "comment-3", "close-3"]);
    let scope = json!({"provider": "composio.github", "account": "ca_fake_bot:widget-bot", "destination": "acme/widgets:1001"});
    let mut plan_ids = Vec::new();
    for operation in operations {
        assert_eq!(operation["scope"], scope, "{operation}");
        let expected = if operation["id"] == "comment-3" {
            Value::Null
        } else {
            json!([{"issue_state": "open"}])
        };
        assert_eq!(operation["preconditions"], expected, "{operation}");
        if operation["tool"] == "GITHUB_CREATE_AN_ISSUE_COMMENT" {
            let body = operation["arguments"]["body"].as_str().expect("body");
            let last = body.lines().last().expect("a final line");
            let id = operation["id"].as_str().expect("id");
            let plan_id = last
                .strip_prefix("<!-- codemode:")
                .and_then(|rest| rest.strip_suffix(&format!(":{id} -->")))
                .unwrap_or_else(|| panic!("{body} ends with its marker"));
            assert!(plan_id.len() >= 16 && plan_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            plan_ids.push(plan_id.to_string());
        }
    }
    assert!(
        plan_ids.windows(2).all(|pair| pair[0] == pair[1]),
        "one plan id"
    );
    assert_eq!(
        operations[1]["arguments"]["body"]
            .as_str()
            .expect("body")
            .lines()
            .next(),
        Some("The latest failed CI run (9359a12) failed in: check.")
    );
    assert_eq!(
        operations[3]["arguments"],
        json!({"owner": "acme", "repo": "widgets", "issue_number": 3, "state": "closed", "state_reason": "not_planned"})
    );

    // The marker is part of the digest.
    let edited = text.replacen(&plan_ids[0], &"0".repeat(plan_ids[0].len()), 1);
    std::fs::write(scratch.file("edited.json"), edited).expect("edited plan");
    assert_ne!(
        check(&scratch.file("edited.json"))["digest"],
        checked["digest"]
    );

    // Investigation only read, and a second investigation gets a new plan id.
    assert!(
        scratch.calls().iter().all(|call| !call.contains("ADD")
            && !call.contains("UPDATE")
            && !call.contains("CREATE")),
        "{:?}",
        scratch.calls()
    );
    let again = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    assert!(again.status.success());
    let second = std::fs::read_to_string(scratch.file("plan.json")).expect("plan");
    assert!(!second.contains(&plan_ids[0]), "a fresh plan id");
    let (_, outcomes) = trace(&scratch);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.ends_with("succeeded"))
    );
}

#[test]
fn an_account_selector_that_is_not_a_connected_account_id_is_refused_before_any_call() {
    let scratch = Scratch::new(json!({}));
    let program = scratch.program("return { operations: [] };");
    for account in ["default", "work-alias", "ca_"] {
        let output = investigate(&scratch, &program, account)
            .output()
            .expect("host runs");
        assert_eq!(output.status.code(), Some(2), "{account}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("connected-account id (ca_...)"),
            "{output:?}"
        );
    }
    assert!(scratch.calls().is_empty());
}

#[test]
fn writes_and_meta_tools_are_denied_by_rust_and_other_repositories_by_the_host() {
    let scratch = Scratch::new(json!({}));
    let program = scratch.program(
        r#"const codes = [];
for (const [slug, args] of [
  ["GITHUB_ADD_LABELS_TO_AN_ISSUE", { owner: "acme", repo: "widgets", issue_number: 1, labels: ["bug"] }],
  ["COMPOSIO_MULTI_EXECUTE_TOOL", { tools: [] }],
  ["GITHUB_GET_AN_ISSUE", { owner: "acme", repo: "private", issue_number: 1 }],
  ["GITHUB_GET_AN_ISSUE", { owner: "ACME", repo: "Widgets", issue_number: 9 }],
]) {
  try { await tools.call(slug, args); codes.push("none"); } catch (error) { codes.push(error.code); }
}
if (codes.join() !== "tool_denied,tool_denied,tool_denied,composio_tool_failed") throw new Error(codes.join());
return { operations: [] };"#,
    );
    let output = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    let summary = summary(&output);
    assert!(output.status.success(), "{summary}");
    assert_eq!(summary["operations"], 0);
    assert!(
        !scratch.file("plan.json").exists(),
        "an empty proposal writes no plan"
    );
    let (_, outcomes) = trace(&scratch);
    assert_eq!(
        outcomes,
        [
            "GITHUB_ADD_LABELS_TO_AN_ISSUE rejected tool_denied",
            "COMPOSIO_MULTI_EXECUTE_TOOL rejected tool_denied",
            "GITHUB_GET_AN_ISSUE provider_failed tool_denied",
            "GITHUB_GET_AN_ISSUE provider_failed composio_tool_failed",
        ]
    );
    // Only the host's identity reads and the one read of --repo reached Composio.
    assert_eq!(
        scratch.calls(),
        [
            "GITHUB_GET_THE_AUTHENTICATED_USER /",
            "GITHUB_GET_A_REPOSITORY acme/widgets",
            "GITHUB_GET_AN_ISSUE acme/widgets#9",
        ]
    );
}

#[test]
fn a_trace_of_log_reads_keeps_sizes_and_no_signed_url_or_log_body() {
    let scratch = Scratch::new(json!({}));
    let program = scratch.program(
        r#"const { owner, repo } = repository;
const archive = await tools.call("GITHUB_DOWNLOAD_WORKFLOW_RUN_LOGS", { owner, repo, run_id: 77 });
const log = await tools.call("GITHUB_DOWNLOAD_JOB_LOGS_FOR_A_WORKFLOW_RUN", { owner, repo, job_id: 5 });
if (archive.logs_archive.s3url !== "[signed URL removed]") throw new Error("the guest saw a signed URL");
if (!log.details.includes("error: the check failed")) throw new Error("the guest did not see the log");
return { operations: [] };"#,
    );
    let output = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    let summary = summary(&output);
    assert!(output.status.success(), "{summary}");
    let (trace, outcomes) = trace(&scratch);
    assert_eq!(
        outcomes,
        [
            "GITHUB_DOWNLOAD_WORKFLOW_RUN_LOGS succeeded",
            "GITHUB_DOWNLOAD_JOB_LOGS_FOR_A_WORKFLOW_RUN succeeded"
        ]
    );
    for call in trace["calls"].as_array().expect("calls") {
        let bytes = call["result_bytes"].as_u64().expect("result size");
        assert!(bytes > 0);
        assert_eq!(call["outcome"]["value"], json!({"redacted_bytes": bytes}));
    }
    let inspected = Command::new(CODEMODE)
        .args(["inspect", path(&scratch.file("trace.json"))])
        .output()
        .expect("inspect");
    assert!(inspected.status.success(), "{inspected:?}");
}

#[test]
fn a_proposal_outside_the_provider_contract_writes_no_plan() {
    let scratch = Scratch::new(json!({}));
    let program = scratch.program(
        r#"return { operations: [{ id: "relabel-1", tool: "GITHUB_UPDATE_AN_ISSUE",
  arguments: { owner: "acme", repo: "widgets", issue_number: 1, state: "open", labels: [] } }] };"#,
    );
    let output = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    clean(&String::from_utf8_lossy(&output.stderr), "stderr");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("relabel-1 is outside the provider contract (unsupported_arguments)"),
        "{output:?}"
    );
    assert!(!scratch.file("plan.json").exists());
}

#[test]
fn a_cli_that_ends_without_a_result_leaves_the_read_unknown() {
    let scratch = Scratch::new(json!({"GITHUB_GET_AN_ISSUE acme/widgets#1": "unobserved"}));
    let program = scratch.program(
        "return await tools.call('GITHUB_GET_AN_ISSUE', { owner: 'acme', repo: 'widgets', issue_number: 1 });",
    );
    let output = investigate(&scratch, &program, ACCOUNT)
        .output()
        .expect("host runs");
    let summary = summary(&output);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(summary["status"], "cancelled");
    assert_eq!(
        summary["host_diagnostic"],
        "call 1 (GITHUB_GET_AN_ISSUE): composio execute ended without a result; run cancelled"
    );
    assert_eq!(trace(&scratch).1, ["GITHUB_GET_AN_ISSUE unknown"]);
}

/// SIGTERM to the host's whole process group, as `composio run` forwards it,
/// still yields a report in which the unabortable read is unknown.
#[cfg(unix)]
#[test]
fn terminating_the_process_group_leaves_an_unabortable_read_unknown() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;

    let scratch = Scratch::new(json!({"GITHUB_GET_AN_ISSUE acme/widgets#1": "slow"}));
    let program = scratch.program(
        "return await tools.call('GITHUB_GET_AN_ISSUE', { owner: 'acme', repo: 'widgets', issue_number: 1 });",
    );
    let mut child = investigate(&scratch, &program, ACCOUNT)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Deno runs the host");
    // The stand-in reports when the read is in flight.
    let stderr = child.stderr.take().expect("stderr piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line == "started GITHUB_GET_AN_ISSUE" {
                let _ = sender.send(());
            }
        }
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the read starts");
    let started = Instant::now();
    let signal = Command::new("kill")
        .args(["-TERM", "--", &format!("-{}", child.id())])
        .output()
        .expect("kill runs");
    assert!(
        signal.status.success(),
        "group signal failed: {}",
        String::from_utf8_lossy(&signal.stderr)
    );
    let output = child.wait_with_output().expect("host exits");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancellation does not wait for the read"
    );
    assert_eq!(summary(&output)["status"], "cancelled");
    assert_eq!(trace(&scratch).1, ["GITHUB_GET_AN_ISSUE unknown"]);
}

#[test]
fn sdk_executor_forwards_abort_signals_and_maps_results() {
    let output = Command::new(deno())
        .current_dir(root())
        .args(["run", "--allow-run", "--allow-read", "--allow-env"])
        .arg("integrations/composio/testing/sdk-check.ts")
        .arg(CODEMODE)
        .stdin(Stdio::null())
        .output()
        .expect("Deno runs the check");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(result["status"], "success");
    assert_eq!(
        result["output"],
        json!([{"login":"codemode-demo"}, "composio_tool_failed", "composio_sdk_refused", "invalid_arguments"])
    );
    let requests = result["requests"].as_array().expect("requests");
    assert_eq!(
        requests.len(),
        3,
        "non-object arguments never reach Composio"
    );
    assert_eq!(
        result["unobserved"],
        json!({"status":"cancelled","outcomes":["unknown"],"diagnostic":"call 1 (GITHUB_GET_THE_AUTHENTICATED_USER): the Composio request ended without a result; run cancelled"})
    );
    assert_eq!(
        requests[0],
        json!({"slug":"GITHUB_GET_THE_AUTHENTICATED_USER","body":{"userId":"user-1","arguments":{},"version":"20261104_00"},"signal":true})
    );
}
