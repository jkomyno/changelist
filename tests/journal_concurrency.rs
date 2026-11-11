//! A real executor and a reader on one journal file, in one process: a read
//! sees committed facts while a session runs and changes nothing, and a second
//! writer is refused. `tests/apply.rs` shows the same between processes, with
//! `codemode status` and a second `codemode apply`.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use codemode::{
    Approval, Attempt, AttemptError, CallContext, Durability, Journal, JournalError,
    JournalInspectOptions, JournalReader, Operation, Plan, ProbeContext, ProviderDeclaration,
    ProviderError, ReconcileMethod, Reconciliation, RecoverableProvider, RecoveryContract,
    RecoveryPolicy, RepeatSafety, RunCancellation, Scope, SessionKind, SessionOptions, Verdict,
    approval_binding, inspect_journal, run_session,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

const OPEN_NOTE: &str = "no end recorded; this does not show that an executor is running";
/// The inspection keeps this many sessions, latest first.
const SHOWN_SESSIONS: usize = 20;

static SCRATCH: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "codemode-journal-concurrency-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("create scratch directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn journal(&self) -> PathBuf {
        self.file("journal.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.0)
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// What an inspection of `journal` answers now, as JSON and as text.
fn inspect_json(journal: &Path) -> (Value, String) {
    let reader = JournalReader::open(journal).expect("the journal opens read-only");
    let inspection =
        inspect_journal(&reader, &JournalInspectOptions::default()).expect("inspection");
    (
        serde_json::to_value(&inspection).expect("inspection is JSON"),
        inspection.render(),
    )
}

/// A checksum of `file`, computed in another process: closing any descriptor on
/// a database file in this process would release the SQLite locks an
/// in-process executor holds on it.
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

/// The database bytes, the write-ahead log bytes, and the WAL index's
/// checkpoint position (`nBackfill`, the first field of the checkpoint info
/// after the two 48-byte index headers), each read by another process.
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

fn journal_mode(journal: &Path) -> String {
    let connection =
        rusqlite::Connection::open_with_flags(journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("read-only open");
    connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode")
}

fn session(inspection: &Value, number: u64) -> &Value {
    inspection["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|session| session["session"] == number)
        .unwrap_or_else(|| panic!("session {number} in {inspection}"))
}

fn operation<'a>(inspection: &'a Value, id: &str) -> &'a Value {
    inspection["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .find(|operation| operation["operation"] == id)
        .unwrap_or_else(|| panic!("operation {id} in {inspection}"))
}

/// One gate per operation: the provider announces entry, then waits for release.
#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

/// A provider whose calls wait inside `execute` until the test releases them.
#[derive(Clone)]
struct Gated {
    gates: Arc<HashMap<String, Gate>>,
}

impl Gated {
    fn new(operations: &[&str]) -> Self {
        Self {
            gates: Arc::new(
                operations
                    .iter()
                    .map(|id| ((*id).to_string(), Gate::default()))
                    .collect(),
            ),
        }
    }

    async fn entered(&self, operation: &str) {
        tokio::time::timeout(
            Duration::from_secs(20),
            self.gates[operation].entered.notified(),
        )
        .await
        .unwrap_or_else(|_| panic!("{operation} never reached the provider"));
    }

    fn release(&self, operation: &str) {
        self.gates[operation].release.notify_one();
    }
}

impl RecoverableProvider for Gated {
    fn declaration(&self) -> ProviderDeclaration {
        ProviderDeclaration {
            provider: "test".into(),
            account: "bot".into(),
            contracts: [(
                "label".to_string(),
                RecoveryContract {
                    repeat: RepeatSafety::Safe,
                    reconcile: ReconcileMethod::Receipt,
                },
            )]
            .into(),
            retention: codemode::Retention::default(),
            definitions: std::collections::BTreeMap::new(),
        }
    }

    async fn authorize(&self, _scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn execute(
        &self,
        attempt: Attempt,
        _context: CallContext,
    ) -> Result<Value, AttemptError> {
        let gate = &self.gates[&attempt.operation.id];
        gate.entered.notify_one();
        gate.release.notified().await;
        Ok(json!({"ok": true}))
    }

    async fn reconcile(
        &self,
        _operation: Operation,
        _keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        Ok(Reconciliation {
            verdict: Verdict::Unknown,
            evidence: String::new(),
        })
    }
}

const PLAN: &str = r#"{"version": 1, "operations": [
  {"id": "label-1", "tool": "label", "scope": {"provider": "test", "account": "bot"},
   "arguments": {"issue": 1, "label": "VALUE-ONE"}},
  {"id": "label-2", "tool": "label", "scope": {"provider": "test", "account": "bot"},
   "arguments": {"issue": 2, "label": "VALUE-TWO"}}]}"#;

/// Create and approve a journal, returning its writer. Tests keep this writer
/// rather than reopening: a process spawned by another test thread can briefly
/// inherit a closed lock's descriptor between its fork and exec.
fn approve(journal: &Path, provider: &impl RecoverableProvider) -> (Journal, codemode::FrozenPlan) {
    let plan = Plan::parse(PLAN).expect("valid plan");
    let mut created = Journal::create(journal, &plan, Durability::Process).expect("create");
    let declaration = provider.declaration().for_plan(&plan);
    let policy = RecoveryPolicy {
        concurrency: 1,
        ..RecoveryPolicy::default()
    };
    created
        .approve(&Approval {
            binding: approval_binding(plan.digest(), &declaration, &policy),
            plan: plan.digest().clone(),
            declaration,
            policy,
            approver: "tester".into(),
            note: None,
            approved_unix_ms: 0,
        })
        .expect("approve");
    (created, plan)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_session_is_inspected_without_change_and_cannot_be_taken_over() {
    let scratch = Scratch::new();
    let journal = scratch.journal();
    let provider = Gated::new(&["label-1", "label-2"]);
    let (writer, _) = approve(&journal, &provider);

    let executor = tokio::spawn(run_session(
        writer,
        provider.clone(),
        SessionOptions {
            kind: SessionKind::Run,
            ..SessionOptions::default()
        },
        RunCancellation::default(),
    ));
    provider.entered("label-1").await;

    // The executor is inside the provider with label-1's intent committed.
    let before = storage_state(&journal);
    let (first, text) = inspect_json(&journal);
    let open = session(&first, 1);
    assert_eq!(open["state"], "open", "{first}");
    assert_eq!(open["note"], OPEN_NOTE, "{first}");
    let attempts = operation(&first, "label-1")["attempts"]
        .as_array()
        .expect("attempts");
    assert_eq!(attempts.len(), 1, "{first}");
    assert_eq!(attempts[0]["observed"], "no result", "{first}");
    assert_eq!(operation(&first, "label-1")["conclusion"], "unknown");
    assert!(
        operation(&first, "label-2")["attempts"]
            .as_array()
            .expect("attempts")
            .is_empty()
    );
    assert!(text.contains(&format!("open — {OPEN_NOTE}")), "{text}");
    // Inspection wrote nothing and moved no checkpoint.
    assert_eq!(
        storage_state(&journal),
        before,
        "inspection changed storage"
    );
    assert_eq!(journal_mode(&journal), "wal");

    // A second writer is refused and changes nothing.
    let refused = Journal::open(&journal, Durability::Storage).expect_err("the journal is owned");
    assert!(matches!(refused, JournalError::Busy(_)), "{refused}");
    assert!(
        refused
            .to_string()
            .contains("locked by another codemode executor"),
        "{refused}"
    );
    assert_eq!(storage_state(&journal), before, "a refused writer wrote");

    // After release, inspection shows the observation while the session is still open.
    provider.release("label-1");
    provider.entered("label-2").await;
    let (second, _) = inspect_json(&journal);
    assert_eq!(session(&second, 1)["state"], "open", "{second}");
    let attempts = operation(&second, "label-1")["attempts"]
        .as_array()
        .expect("attempts");
    assert_eq!(attempts[0]["observed"], "succeeded", "{second}");
    assert_eq!(operation(&second, "label-1")["conclusion"], "applied");

    provider.release("label-2");
    let outcome = tokio::time::timeout(Duration::from_secs(20), executor)
        .await
        .expect("session ends")
        .expect("executor joins")
        .expect("session runs");
    assert_eq!(outcome.status, "completed");
    let (last, _) = inspect_json(&journal);
    assert_eq!(session(&last, 1)["state"], "ended", "{last}");
    assert_eq!(session(&last, 1)["status"], "completed", "{last}");
    assert!(session(&last, 1).get("note").is_none(), "{last}");
}

#[test]
fn inspection_in_a_read_only_directory_never_opens_for_writing() {
    let scratch = Scratch::new();
    let journal = scratch.journal();
    drop(approve(&journal, &Gated::new(&[])));
    let before: Vec<String> = listing(&scratch.0);
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
    let outcome = JournalReader::open(&journal)
        .and_then(|reader| inspect_journal(&reader, &JournalInspectOptions::default()));
    match &outcome {
        Ok(inspection) => assert!(inspection.render().contains("## What was approved?")),
        Err(error) => {
            assert!(matches!(error, JournalError::ReadOnly { .. }), "{error}");
            assert!(
                error.to_string().contains("could not be opened read-only"),
                "{error}"
            );
        }
    }
    drop(outcome);
    assert_eq!(listing(&scratch.0), before);
    assert_eq!(checksum(&journal), hash);
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
        .collect();
    names.sort();
    names
}

#[test]
fn inspection_shows_the_latest_sessions_and_counts_the_rest() {
    let scratch = Scratch::new();
    let journal = scratch.journal();
    let (mut writer, _) = approve(&journal, &Gated::new(&[]));
    for _ in 0..500 {
        let session = writer.begin_session(SessionKind::Resume).expect("session");
        writer
            .end_session(session, "incomplete", None, None)
            .expect("end");
    }
    drop(writer);

    let (inspection, render) = inspect_json(&journal);
    let sessions: Vec<u64> = inspection["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|session| session["session"].as_u64().expect("number"))
        .collect();
    let latest: Vec<u64> = (500 - SHOWN_SESSIONS as u64 + 1..=500).collect();
    assert_eq!(sessions, latest);
    assert_eq!(inspection["sessions_omitted"], 500 - SHOWN_SESSIONS);
    let text = inspection.to_string();
    assert!(text.len() < 16 * 1024, "{} bytes", text.len());
    assert!(
        render.contains(&format!(
            "{} earlier sessions omitted",
            500 - SHOWN_SESSIONS
        )),
        "{render}"
    );
    assert!(render.contains("session 500 resume"), "{render}");
    assert!(!render.contains("session 480 resume"), "{render}");
}
