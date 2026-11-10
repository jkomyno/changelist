//! A durable, single-owner operation journal in SQLite.
//!
//! The journal records one frozen plan and append-only facts about it: approvals,
//! sessions, dispatch intents, observations, reconciliation probes, and reviews.
//! Every fact carries an ordinal from one counter, so facts can be folded in the
//! order they were committed.
//!
//! # Durability
//!
//! The journal uses SQLite's write-ahead log in normal locking mode. With
//! [`Durability::Storage`] (`synchronous=FULL`, plus `F_FULLFSYNC` on macOS), a
//! write that returns `Ok` has committed and SQLite has asked the operating
//! system to flush it to stable storage. Whether that survives power loss depends
//! on the storage device honoring flushes; that is not tested here. With
//! [`Durability::Process`] (`synchronous=NORMAL`), a committed write survives the
//! process being killed, but the latest commits may be lost if the operating
//! system crashes or power fails. The tests kill processes; they establish only
//! process-crash durability.
//!
//! # Ownership
//!
//! A writer ([`Journal::create`], [`Journal::open`]) first takes an exclusive
//! operating-system lock on the sidecar file `<journal>.lock`, created beside
//! the journal and never removed, and holds it until the [`Journal`] is dropped
//! (after its SQLite connection closes) or its process exits, so two executors
//! can never own one journal. A second writer fails with [`JournalError::Busy`].
//! The lock file is part of the journal: deleting it while an executor holds it
//! lets a second executor in, which is tampering outside this guarantee.
//!
//! A reader ([`JournalReader::open`]) never touches the lock file. It opens
//! SQLite read-only, changes no durability or journal-mode setting, and reads
//! one consistent snapshot of committed facts, so it works while an executor
//! runs. It writes no journal data and never checkpoints the write-ahead log;
//! SQLite may create the log's shared-memory index (`<journal>-shm`) and an
//! empty `<journal>-wal`.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, params};

use crate::plan::{Digest, FrozenPlan, Plan};
use crate::recovery::{
    Approval, Observed, Reconciliation, Resolution, Retention, Verdict, machine_code,
};
use crate::runtime::{ProviderError, RunReport};

/// The journal format this build reads and writes.
pub const JOURNAL_FORMAT_VERSION: u32 = 2;
const FORMAT: &str = "codemode-journal";
/// Error code of a probe whose answer was never recorded, for example because
/// the executor was killed while the provider was answering it.
pub const UNANSWERED: &str = "unanswered";
/// Largest kept message, evidence, name, note, or diagnostic.
const MAX_TEXT_BYTES: usize = 1024;
/// How long a connection waits on SQLite's own locks. Executors exclude each
/// other through the lock file, so SQLite contention is only between one writer
/// and readers over the write-ahead log's shared-memory index, for example while
/// one of them rebuilds it after a crash; that takes milliseconds.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE clock (id INTEGER PRIMARY KEY CHECK (id = 1), ordinal INTEGER NOT NULL) STRICT;
INSERT INTO clock VALUES (1, 0);
CREATE TABLE plan (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  digest TEXT NOT NULL,
  canonical TEXT NOT NULL
) STRICT;
CREATE TABLE approvals (
  ordinal INTEGER PRIMARY KEY,
  binding TEXT NOT NULL,
  record TEXT NOT NULL
) STRICT;
CREATE TABLE sessions (
  session INTEGER PRIMARY KEY,
  ordinal INTEGER NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK (kind IN ('run', 'resume')),
  pid INTEGER NOT NULL,
  started_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE session_ends (
  session INTEGER PRIMARY KEY REFERENCES sessions (session),
  ordinal INTEGER NOT NULL UNIQUE,
  unix_ms INTEGER NOT NULL,
  status TEXT NOT NULL,
  detail TEXT,
  trace TEXT
) STRICT;
CREATE TABLE attempts (
  key TEXT PRIMARY KEY,
  ordinal INTEGER NOT NULL UNIQUE,
  operation TEXT NOT NULL,
  number INTEGER NOT NULL CHECK (number > 0),
  session INTEGER NOT NULL REFERENCES sessions (session),
  call_id INTEGER,
  unix_ms INTEGER NOT NULL,
  UNIQUE (operation, number)
) STRICT;
CREATE TABLE observations (
  key TEXT PRIMARY KEY REFERENCES attempts (key),
  ordinal INTEGER NOT NULL UNIQUE,
  unix_ms INTEGER NOT NULL,
  record TEXT NOT NULL
) STRICT;
CREATE TABLE probes (
  ordinal INTEGER PRIMARY KEY,
  operation TEXT NOT NULL,
  session INTEGER NOT NULL REFERENCES sessions (session),
  keys TEXT NOT NULL,
  unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE reconciliations (
  probe INTEGER PRIMARY KEY REFERENCES probes (ordinal),
  ordinal INTEGER NOT NULL UNIQUE,
  unix_ms INTEGER NOT NULL,
  elapsed_ms INTEGER NOT NULL,
  verdict TEXT CHECK (verdict IN ('applied', 'satisfied', 'not_applied', 'unknown')),
  evidence TEXT,
  error_code TEXT,
  error_message TEXT
) STRICT;
CREATE TABLE reviews (
  ordinal INTEGER PRIMARY KEY,
  operation TEXT NOT NULL,
  resolution TEXT NOT NULL CHECK (resolution IN ('applied', 'not_applied')),
  reviewer TEXT NOT NULL,
  note TEXT,
  unix_ms INTEGER NOT NULL
) STRICT;
";

/// How hard each commit pushes data toward stable storage; see the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Durability {
    /// Commit, then ask the OS to flush to storage (`synchronous=FULL`, `fullfsync`).
    #[default]
    Storage,
    /// Commit to the OS only (`synchronous=NORMAL`); survives a killed process.
    Process,
}

/// Why a journal could not be created, opened, or written.
#[derive(Debug, Clone)]
pub enum JournalError {
    /// Another executor holds the journal's lock file.
    Busy(PathBuf),
    /// The journal could not be opened read-only, for example because SQLite
    /// cannot create the write-ahead log's shared-memory index beside it.
    /// Inspection never falls back to opening a journal for writing.
    ReadOnly {
        /// The journal path.
        path: PathBuf,
        /// What SQLite reported.
        detail: String,
    },
    /// The file is missing.
    NotFound(PathBuf),
    /// `create` found an existing file.
    Exists(PathBuf),
    /// The file is damaged or its contents contradict each other.
    Corrupt {
        /// The journal path.
        path: PathBuf,
        /// What check failed.
        detail: String,
    },
    /// The file is not a journal this build can read.
    Incompatible {
        /// The journal path.
        path: PathBuf,
        /// What was found.
        detail: String,
    },
    /// The request contradicts the journal, for example a different plan.
    Conflict(String),
    /// SQLite or file I/O failed.
    Storage(String),
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(path) => write!(
                formatter,
                "journal {} is locked by another codemode executor (a run, resume, approve, or \
                 review); wait for it to exit. `codemode plan inspect` can read it meanwhile",
                path.display()
            ),
            Self::ReadOnly { path, detail } => write!(
                formatter,
                "journal {} could not be opened read-only ({detail}). Reading it needs SQLite's \
                 shared-memory index ({}-shm), which must exist or be creatable beside the \
                 journal; inspection never opens a journal for writing",
                path.display(),
                path.display()
            ),
            Self::NotFound(path) => write!(formatter, "journal {} does not exist", path.display()),
            Self::Exists(path) => write!(
                formatter,
                "journal {} already exists; each journal holds one plan",
                path.display()
            ),
            Self::Corrupt { path, detail } => write!(
                formatter,
                "journal {} is damaged ({detail}); it was not changed. Keep it for \
                 investigation and do not resume from it",
                path.display()
            ),
            Self::Incompatible { path, detail } => write!(
                formatter,
                "{} is not a journal this build can use: {detail}",
                path.display()
            ),
            Self::Conflict(message) => formatter.write_str(message),
            Self::Storage(message) => write!(formatter, "journal storage failed: {message}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// One executor session: a `run` or a `resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// The first execution of an approved plan.
    Run,
    /// Recovery and continuation after an earlier session.
    Resume,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Resume => "resume",
        }
    }
}

/// A recorded session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    /// Sequential session number.
    pub session: u64,
    /// Fact ordinal of its start.
    pub ordinal: u64,
    /// `run` or `resume`.
    pub kind: String,
    /// Executor process id.
    pub pid: u32,
    /// Start time, Unix milliseconds.
    pub started_unix_ms: u64,
    /// How it ended; absent while no end is recorded: the executor is still
    /// running, or stopped without recording one.
    pub end: Option<SessionEnd>,
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEnd {
    /// Fact ordinal.
    pub ordinal: u64,
    /// End time, Unix milliseconds.
    pub unix_ms: u64,
    /// Outcome, such as `completed`, `refused`, or a run status.
    pub status: String,
    /// A bounded explanation.
    pub detail: Option<String>,
    /// Whether an execution record from the dispatch run is stored.
    pub has_trace: bool,
}

/// A dispatch intent and, once observed, its result.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    /// Unique attempt key, given to the provider.
    pub key: String,
    /// Fact ordinal of the intent.
    pub ordinal: u64,
    /// The operation's stable identity.
    pub operation: String,
    /// Attempt number for this operation.
    pub number: u32,
    /// The session that dispatched it.
    pub session: u64,
    /// The call identity in that session's execution record.
    pub call_id: Option<u64>,
    /// Intent time, Unix milliseconds.
    pub unix_ms: u64,
    /// The observation, when one was recorded.
    pub observation: Option<ObservationRecord>,
}

/// An observed attempt result.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationRecord {
    /// Fact ordinal.
    pub ordinal: u64,
    /// Observation time, Unix milliseconds.
    pub unix_ms: u64,
    /// What was observed.
    pub observed: Observed,
}

/// A recorded reconciliation probe.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeRecord {
    /// Fact ordinal of the probe's start, recorded before the provider was asked.
    pub ordinal: u64,
    /// The operation probed.
    pub operation: String,
    /// The session that probed.
    pub session: u64,
    /// The attempt keys probed.
    pub keys: Vec<String>,
    /// Start time, Unix milliseconds.
    pub unix_ms: u64,
    /// Probe duration; 0 when no answer was recorded.
    pub elapsed_ms: u64,
    /// The provider's answer, or the probe's own failure; [`UNANSWERED`] when the
    /// executor stopped before an answer was recorded.
    pub outcome: Result<Reconciliation, ProviderError>,
}

/// A person's recorded review.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRecord {
    /// Fact ordinal.
    pub ordinal: u64,
    /// The operation reviewed.
    pub operation: String,
    /// The finding.
    pub resolution: Resolution,
    /// Who reviewed, as stated.
    pub reviewer: String,
    /// Optional note.
    pub note: Option<String>,
    /// Review time, Unix milliseconds.
    pub unix_ms: u64,
}

/// Everything a journal records, in commit order within each list.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    /// Approvals, oldest first; the last one applies.
    pub approvals: Vec<Approval>,
    /// Sessions, oldest first.
    pub sessions: Vec<SessionRecord>,
    /// Attempts, in intent order.
    pub attempts: Vec<AttemptRecord>,
    /// Reconciliation probes.
    pub probes: Vec<ProbeRecord>,
    /// Reviews.
    pub reviews: Vec<ReviewRecord>,
}

/// An open journal for one plan, owned by this writer through its lock file.
pub struct Journal {
    // Fields drop in order: the connection closes before the lock is released.
    connection: Connection,
    _lock: File,
    path: PathBuf,
    id: String,
    plan: FrozenPlan,
    #[cfg(test)]
    hook: Option<test_hook::CommitHook>,
}

/// A read-only snapshot of a journal; see [`JournalReader::open`].
pub struct JournalReader {
    connection: Connection,
    path: PathBuf,
    id: String,
    plan: FrozenPlan,
}

impl fmt::Debug for JournalReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JournalReader")
            .field("path", &self.path)
            .field("plan", self.plan.digest())
            .finish_non_exhaustive()
    }
}

/// Which fact a write records; only test hooks look at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only test builds classify committed records")
)]
pub(crate) enum WriteKind<'a> {
    /// A dispatch intent for this attempt key.
    Intent(&'a str),
    /// An observed result for this attempt key.
    Observation(&'a str),
    /// Any other fact.
    Other,
}

/// A fact a session records while it runs, ready to commit; see [`Journal::commit`].
#[derive(Debug)]
pub(crate) enum Record {
    /// See [`Journal::record_intent`].
    Intent {
        key: String,
        operation: String,
        number: u32,
        session: u64,
        call_id: Option<u64>,
    },
    /// See [`Journal::record_observation`]; `record` is the retained observation as JSON.
    Observation { key: String, record: String },
    /// See [`Journal::record_probe_start`]; `keys` is a JSON array.
    ProbeStart {
        operation: String,
        session: u64,
        keys: String,
    },
    /// See [`Journal::record_probe`], with only what retention keeps.
    ProbeAnswer {
        probe: u64,
        elapsed_ms: u64,
        verdict: Option<&'static str>,
        evidence: Option<String>,
        code: Option<String>,
        message: Option<String>,
    },
}

impl Record {
    pub(crate) fn intent(
        key: &str,
        operation: &str,
        number: u32,
        session: u64,
        call_id: Option<u64>,
    ) -> Self {
        Self::Intent {
            key: key.to_string(),
            operation: operation.to_string(),
            number,
            session,
            call_id,
        }
    }

    /// The observation as `retention` keeps it.
    pub(crate) fn observation(
        key: &str,
        observed: Observed,
        retention: Retention,
    ) -> Result<Self, JournalError> {
        let record =
            serde_json::to_string(&retained_observation(observed, retention)).map_err(storage)?;
        Ok(Self::Observation {
            key: key.to_string(),
            record,
        })
    }

    pub(crate) fn probe_start(
        operation: &str,
        session: u64,
        keys: &[String],
    ) -> Result<Self, JournalError> {
        let keys = serde_json::to_string(keys).map_err(storage)?;
        Ok(Self::ProbeStart {
            operation: operation.to_string(),
            session,
            keys,
        })
    }

    /// The answer as `retention` keeps it: the verdict and error code always,
    /// the evidence and error message only under `summary/1`.
    pub(crate) fn probe_answer(
        probe: u64,
        elapsed_ms: u64,
        outcome: &Result<Reconciliation, ProviderError>,
        retention: Retention,
    ) -> Self {
        let (verdict, evidence, code, message) = match outcome {
            Ok(reconciliation) => (
                Some(reconciliation.verdict.as_str()),
                Some(retained_text(&reconciliation.evidence, retention)),
                None,
                None,
            ),
            Err(error) => {
                let error = retained_error(error, retention);
                (None, None, Some(error.code), Some(error.message))
            }
        };
        Self::ProbeAnswer {
            probe,
            elapsed_ms,
            verdict,
            evidence,
            code,
            message,
        }
    }

    #[cfg(test)]
    fn kind(&self) -> WriteKind<'_> {
        match self {
            Self::Intent { key, .. } => WriteKind::Intent(key),
            Self::Observation { key, .. } => WriteKind::Observation(key),
            Self::ProbeStart { .. } | Self::ProbeAnswer { .. } => WriteKind::Other,
        }
    }

    fn insert(&self, transaction: &Transaction<'_>, ordinal: i64) -> rusqlite::Result<usize> {
        match self {
            Self::Intent {
                key,
                operation,
                number,
                session,
                call_id,
            } => transaction.execute(
                "INSERT INTO attempts VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key,
                    ordinal,
                    operation,
                    number,
                    *session as i64,
                    call_id.map(|id| id as i64),
                    now_ms() as i64
                ],
            ),
            Self::Observation { key, record } => transaction.execute(
                "INSERT INTO observations VALUES (?1, ?2, ?3, ?4)",
                params![key, ordinal, now_ms() as i64, record],
            ),
            Self::ProbeStart {
                operation,
                session,
                keys,
            } => transaction.execute(
                "INSERT INTO probes VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ordinal, operation, *session as i64, keys, now_ms() as i64],
            ),
            Self::ProbeAnswer {
                probe,
                elapsed_ms,
                verdict,
                evidence,
                code,
                message,
            } => transaction.execute(
                "INSERT INTO reconciliations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    *probe as i64,
                    ordinal,
                    now_ms() as i64,
                    *elapsed_ms as i64,
                    verdict,
                    evidence,
                    code,
                    message
                ],
            ),
        }
    }
}

/// Test-only control over commits, so tests can hold or fail a write before it
/// commits and observe when it has committed.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::sync::Arc;

    use super::WriteKind;

    /// When a hook runs: inside the open transaction, or after a successful commit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Point {
        BeforeCommit,
        Committed,
    }

    /// Called at each [`Point`] of every write. An error before the commit rolls
    /// the write back and is returned as a storage error.
    pub(crate) type CommitHook =
        Arc<dyn Fn(Point, WriteKind<'_>) -> Result<(), String> + Send + Sync>;
}

impl fmt::Debug for Journal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Journal")
            .field("path", &self.path)
            .field("plan", self.plan.digest())
            .finish_non_exhaustive()
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Truncate `text` to at most `MAX_TEXT_BYTES` on a character boundary.
fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Journals created by this process, so two in one process never share an identity.
static CREATED: AtomicU64 = AtomicU64::new(0);

/// Write transactions committed by journals in this process.
#[cfg(feature = "testkit")]
static COMMITS: AtomicU64 = AtomicU64::new(0);

/// Write transactions every journal in this process has committed so far,
/// counting only transactions that wrote facts. One commit can hold several
/// facts. Evaluations read it before and after a session to count that
/// session's commits. Requires the `testkit` feature.
///
/// # Examples
///
/// ```
/// use codemode::{Durability, Journal, Plan, SessionKind, journal_commits};
///
/// let plan = Plan::parse(r#"{"version": 1, "operations": [
///   {"id": "label-101", "tool": "issues.add_label",
///    "scope": {"provider": "fixture.issues", "account": "triage-bot"},
///    "arguments": {"issue": 101, "label": "triage"}}]}"#)?;
/// let path = std::env::temp_dir().join(format!("codemode-commits-{}.db", std::process::id()));
/// # let _ = std::fs::remove_file(&path);
/// let mut journal = Journal::create(&path, &plan, Durability::Process)?;
/// let before = journal_commits();
/// journal.begin_session(SessionKind::Run)?;
/// assert!(journal_commits() > before);
/// # drop(journal);
/// # std::fs::remove_file(&path)?;
/// # for suffix in ["db-wal", "db-shm", "db.lock"] {
/// #     let _ = std::fs::remove_file(path.with_extension(suffix));
/// # }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[cfg(feature = "testkit")]
#[must_use]
pub fn journal_commits() -> u64 {
    COMMITS.load(Ordering::Relaxed)
}

/// Count one committed write transaction; see [`journal_commits`].
fn counted() {
    #[cfg(feature = "testkit")]
    COMMITS.fetch_add(1, Ordering::Relaxed);
}

/// A new journal's identity. Attempt keys embed it, and providers may keep keys
/// across journals, so it must differ between journals: it mixes the process, a
/// per-process counter, the time in nanoseconds, and a randomly keyed hash.
fn journal_id(plan: &FrozenPlan) -> String {
    let random = RandomState::new().hash_one((plan.digest(), std::process::id()));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let seed = format!(
        "{}:{}:{}:{nanos}:{random}",
        plan.digest(),
        std::process::id(),
        CREATED.fetch_add(1, Ordering::Relaxed)
    );
    // 64 bits of the digest: an attempt key names its journal, not a secret.
    Digest::of(seed.as_bytes())
        .as_str()
        .trim_start_matches("sha256:")[..16]
        .to_string()
}

pub(crate) fn storage(error: impl ToString) -> JournalError {
    JournalError::Storage(error.to_string())
}

/// Take the next fact ordinal inside `transaction`.
fn next_ordinal(transaction: &Transaction<'_>) -> rusqlite::Result<i64> {
    transaction.query_row(
        "UPDATE clock SET ordinal = ordinal + 1 WHERE id = 1 RETURNING ordinal",
        [],
        |row| row.get(0),
    )
}

fn sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// What `retention` keeps of a provider prose field: under `summary/1` the text
/// truncated to 1 KiB, otherwise nothing.
fn retained_text(text: &str, retention: Retention) -> String {
    match retention {
        Retention::Summary => bounded(text),
        Retention::Metadata => String::new(),
    }
}

/// A provider error as `retention` keeps it: a machine code, and the message only
/// under `summary/1`.
fn retained_error(error: &ProviderError, retention: Retention) -> ProviderError {
    ProviderError {
        code: machine_code(&error.code).to_string(),
        message: retained_text(&error.message, retention),
    }
}

/// An observation as `retention` keeps it. The outcome, the stated effect, and a
/// result's size always remain; a result value never does.
fn retained_observation(observed: Observed, retention: Retention) -> Observed {
    match observed {
        Observed::Succeeded { bytes } => Observed::Succeeded { bytes },
        Observed::Failed { error, effect } => Observed::Failed {
            error: retained_error(&error, retention),
            effect,
        },
        Observed::Unobserved { reason } => Observed::Unobserved {
            reason: bounded(&reason),
        },
        other => other,
    }
}

impl Journal {
    /// Create a journal for `plan` at `path`, which must not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use codemode::{Durability, Journal, Plan, SessionKind};
    ///
    /// let plan = Plan::parse(r#"{"version": 1, "operations": [
    ///   {"id": "label-101", "tool": "issues.add_label",
    ///    "scope": {"provider": "fixture.issues", "account": "triage-bot"},
    ///    "arguments": {"issue": 101, "label": "triage"}}]}"#)?;
    /// let path = std::env::temp_dir().join(format!("codemode-doc-{}.db", std::process::id()));
    /// # let _ = std::fs::remove_file(&path);
    /// let mut journal = Journal::create(&path, &plan, Durability::Process)?;
    /// let session = journal.begin_session(SessionKind::Run)?;
    /// journal.record_intent("key-1", "label-101", 1, session, None)?;
    /// // The attempt has an intent and no observation: its outcome is unknown.
    /// assert!(journal.history()?.attempts[0].observation.is_none());
    /// # drop(journal);
    /// # std::fs::remove_file(&path)?;
    /// # for suffix in ["db-wal", "db-shm", "db.lock"] {
    /// #     let _ = std::fs::remove_file(path.with_extension(suffix));
    /// # }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// Returns [`JournalError::Busy`] if another executor holds the lock file,
    /// [`JournalError::Exists`] if the journal exists, or a storage error.
    pub fn create(
        path: impl AsRef<Path>,
        plan: &FrozenPlan,
        durability: Durability,
    ) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let lock = acquire(&path)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => JournalError::Exists(path.clone()),
                _ => JournalError::Storage(error.to_string()),
            })?;
        let (mut connection, _) = connect(&path, durability, false)?;
        let id = journal_id(plan);
        let transaction = connection.transaction().map_err(storage)?;
        transaction.execute_batch(SCHEMA).map_err(storage)?;
        for (key, value) in [
            ("format", FORMAT.to_string()),
            ("format_version", JOURNAL_FORMAT_VERSION.to_string()),
            ("journal_id", id.clone()),
            ("created_unix_ms", now_ms().to_string()),
            ("codemode_version", env!("CARGO_PKG_VERSION").to_string()),
        ] {
            transaction
                .execute("INSERT INTO meta VALUES (?1, ?2)", params![key, value])
                .map_err(storage)?;
        }
        transaction
            .execute(
                "INSERT INTO plan VALUES (1, ?1, ?2)",
                params![plan.digest().as_str(), plan.canonical()],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        counted();
        Ok(Self {
            connection,
            _lock: lock,
            path,
            id,
            plan: plan.clone(),
            #[cfg(test)]
            hook: None,
        })
    }

    /// Open an existing journal for writing, taking its lock file first and
    /// then checking the journal's integrity and format.
    ///
    /// # Errors
    /// Returns [`JournalError::Busy`] if another executor holds the lock file,
    /// [`JournalError::Conflict`] if the lock file was replaced while it was
    /// being locked, [`JournalError::Corrupt`] or [`JournalError::Incompatible`]
    /// if the journal cannot be trusted, or [`JournalError::NotFound`].
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(JournalError::NotFound(path));
        }
        let lock = acquire(&path)?;
        let (connection, stored) = connect(&path, durability, true)?;
        let (id, plan) =
            stored.ok_or_else(|| JournalError::Storage("journal was not validated".into()))?;
        Ok(Self {
            connection,
            _lock: lock,
            path,
            id,
            plan,
            #[cfg(test)]
            hook: None,
        })
    }

    /// The journal file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A short identity generated when the journal was created; part of every attempt key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The plan this journal records.
    #[must_use]
    pub fn plan(&self) -> &FrozenPlan {
        &self.plan
    }

    /// Install a hook that runs before and after every commit.
    #[cfg(test)]
    pub(crate) fn set_commit_hook(&mut self, hook: test_hook::CommitHook) {
        self.hook = Some(hook);
    }

    /// Write one transaction holding the facts `kinds` names, which `body` inserts.
    fn write<T>(
        &mut self,
        kinds: &[WriteKind<'_>],
        body: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, JournalError> {
        let transaction = self.connection.transaction().map_err(storage)?;
        let value = body(&transaction).map_err(storage)?;
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            for &kind in kinds {
                // Dropping the transaction rolls it back.
                hook(test_hook::Point::BeforeCommit, kind).map_err(JournalError::Storage)?;
            }
        }
        transaction.commit().map_err(storage)?;
        counted();
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            for &kind in kinds {
                let _ = hook(test_hook::Point::Committed, kind);
            }
        }
        #[cfg(not(test))]
        let _ = kinds;
        Ok(value)
    }

    /// Commit `records` in one transaction, in order, and return each one's fact
    /// ordinal. Either every record commits or none does.
    pub(crate) fn commit(&mut self, records: &[Record]) -> Result<Vec<u64>, JournalError> {
        // Only the test hook reads the kinds, so other builds pass none.
        #[cfg(test)]
        let kinds: Vec<WriteKind<'_>> = records.iter().map(Record::kind).collect();
        #[cfg(not(test))]
        let kinds: Vec<WriteKind<'_>> = Vec::new();
        self.write(&kinds, |transaction| {
            records
                .iter()
                .map(|record| {
                    let ordinal = next_ordinal(transaction)?;
                    record.insert(transaction, ordinal)?;
                    Ok(sql_u64(ordinal))
                })
                .collect()
        })
    }

    /// Record an approval for this journal's plan.
    ///
    /// # Errors
    /// Returns [`JournalError::Conflict`] if the approval names another plan.
    pub fn approve(&mut self, approval: &Approval) -> Result<(), JournalError> {
        if &approval.plan != self.plan.digest() {
            return Err(JournalError::Conflict(format!(
                "journal {} holds plan {}, not {}; create a new journal for a changed plan",
                self.path.display(),
                self.plan.digest(),
                approval.plan
            )));
        }
        // The binding does not cover the approver or note, so bounding them keeps it valid.
        let bounded_approval = Approval {
            approver: bounded(&approval.approver),
            note: approval.note.as_deref().map(bounded),
            ..approval.clone()
        };
        let record = serde_json::to_string(&bounded_approval).map_err(storage)?;
        self.write(&[WriteKind::Other], |transaction| {
            let ordinal = next_ordinal(transaction)?;
            transaction.execute(
                "INSERT INTO approvals VALUES (?1, ?2, ?3)",
                params![ordinal, approval.binding.as_str(), record],
            )?;
            Ok(())
        })
    }

    /// Start a session and return its number.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn begin_session(&mut self, kind: SessionKind) -> Result<u64, JournalError> {
        self.write(&[WriteKind::Other], |transaction| {
            let ordinal = next_ordinal(transaction)?;
            transaction.execute(
                "INSERT INTO sessions (ordinal, kind, pid, started_unix_ms) VALUES (?1, ?2, ?3, ?4)",
                params![ordinal, kind.as_str(), std::process::id(), now_ms() as i64],
            )?;
            Ok(sql_u64(transaction.last_insert_rowid()))
        })
    }

    /// Record how a session ended, with its dispatch run's execution record.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn end_session(
        &mut self,
        session: u64,
        status: &str,
        detail: Option<&str>,
        trace: Option<&RunReport>,
    ) -> Result<(), JournalError> {
        let trace = trace
            .map(serde_json::to_string)
            .transpose()
            .map_err(storage)?;
        let detail = detail.map(bounded);
        self.write(&[WriteKind::Other], |transaction| {
            let ordinal = next_ordinal(transaction)?;
            transaction.execute(
                "INSERT INTO session_ends VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session as i64,
                    ordinal,
                    now_ms() as i64,
                    status,
                    detail,
                    trace
                ],
            )?;
            Ok(())
        })
    }

    /// Durably record the intent to dispatch an attempt, before it can reach the provider.
    ///
    /// # Errors
    /// Returns a storage error, in which case the attempt must not be dispatched.
    pub fn record_intent(
        &mut self,
        key: &str,
        operation: &str,
        number: u32,
        session: u64,
        call_id: Option<u64>,
    ) -> Result<(), JournalError> {
        self.commit(&[Record::intent(key, operation, number, session, call_id)])
            .map(drop)
    }

    /// Durably record an attempt's observed result, keeping only what the approved
    /// provider's `retention` policy allows (see [`Retention`]).
    ///
    /// # Errors
    /// Returns a storage error, in which case the result must not be reported as recorded.
    pub fn record_observation(
        &mut self,
        key: &str,
        observed: Observed,
        retention: Retention,
    ) -> Result<(), JournalError> {
        self.commit(&[Record::observation(key, observed, retention)?])
            .map(drop)
    }

    /// Record every attempt of `session` that has no observation as unobserved,
    /// because the run that dispatched it stopped waiting (`reason`). One commit.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn record_unobserved(&mut self, session: u64, reason: &str) -> Result<(), JournalError> {
        let record = serde_json::to_string(&Observed::Unobserved {
            reason: bounded(reason),
        })
        .map_err(storage)?;
        let transaction = self.connection.transaction().map_err(storage)?;
        let keys: Vec<String> = {
            let mut statement = transaction
                .prepare(
                    "SELECT a.key FROM attempts a LEFT JOIN observations o USING (key)
                     WHERE a.session = ?1 AND o.key IS NULL ORDER BY a.ordinal",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map([session as i64], |row| row.get(0))
                .map_err(storage)?;
            rows.collect::<rusqlite::Result<_>>().map_err(storage)?
        };
        let wrote = !keys.is_empty();
        for key in keys {
            let ordinal = next_ordinal(&transaction).map_err(storage)?;
            transaction
                .execute(
                    "INSERT INTO observations VALUES (?1, ?2, ?3, ?4)",
                    params![key, ordinal, now_ms() as i64, record],
                )
                .map_err(storage)?;
        }
        transaction.commit().map_err(storage)?;
        if wrote {
            counted();
        }
        Ok(())
    }

    /// Durably record that a reconciliation probe is about to ask the provider,
    /// before it can reach the provider, and return the probe's identity. The
    /// probe counts against the probe budget even if its answer is never recorded.
    ///
    /// # Errors
    /// Returns a storage error, in which case the probe must not be sent.
    pub fn record_probe_start(
        &mut self,
        operation: &str,
        session: u64,
        keys: &[String],
    ) -> Result<u64, JournalError> {
        let ordinals = self.commit(&[Record::probe_start(operation, session, keys)?])?;
        Ok(ordinals[0])
    }

    /// Record the provider's answer to the probe `probe` returned by
    /// [`Journal::record_probe_start`], or the probe's own failure, keeping only
    /// what `retention` allows: the verdict and error code always, the evidence and
    /// error message only under `summary/1`.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn record_probe(
        &mut self,
        probe: u64,
        elapsed_ms: u64,
        outcome: &Result<Reconciliation, ProviderError>,
        retention: Retention,
    ) -> Result<(), JournalError> {
        self.commit(&[Record::probe_answer(probe, elapsed_ms, outcome, retention)])
            .map(drop)
    }

    /// Record a person's review of an operation.
    ///
    /// # Errors
    /// Returns [`JournalError::Conflict`] for an operation outside the plan, or a storage error.
    pub fn record_review(
        &mut self,
        operation: &str,
        resolution: Resolution,
        reviewer: &str,
        note: Option<&str>,
    ) -> Result<(), JournalError> {
        if self.plan.operation(operation).is_none() {
            return Err(JournalError::Conflict(format!(
                "the plan has no operation '{operation}'"
            )));
        }
        let resolution = resolution.as_str();
        let reviewer = bounded(reviewer);
        let note = note.map(bounded);
        self.write(&[WriteKind::Other], |transaction| {
            let ordinal = next_ordinal(transaction)?;
            transaction.execute(
                "INSERT INTO reviews VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ordinal,
                    operation,
                    resolution,
                    reviewer,
                    note,
                    now_ms() as i64
                ],
            )?;
            Ok(())
        })
    }

    /// The execution record stored for `session`'s dispatch run, if any.
    ///
    /// # Errors
    /// Returns [`JournalError::Corrupt`] if the stored record cannot be read.
    pub fn trace(&self, session: u64) -> Result<Option<RunReport>, JournalError> {
        self.view().trace(session)
    }

    /// Read every recorded fact.
    ///
    /// # Errors
    /// Returns [`JournalError::Corrupt`] if a stored value cannot be read.
    pub fn history(&self) -> Result<History, JournalError> {
        self.view().history()
    }

    fn view(&self) -> View<'_> {
        View {
            connection: &self.connection,
            path: &self.path,
            plan: &self.plan,
        }
    }
}

impl JournalReader {
    /// Open `path` read-only and take one consistent snapshot of its committed
    /// facts, checking its integrity, format, and plan digest without writing.
    ///
    /// The reader never touches the executor lock file, changes no durability
    /// or journal-mode setting (it only waits up to two seconds on SQLite's own
    /// locks), and reads inside one deferred transaction, so it works while an executor
    /// runs and sees only facts committed before it opened. It never
    /// checkpoints the write-ahead log. SQLite may create the log's
    /// shared-memory index (`<journal>-shm`) and an empty `<journal>-wal`.
    ///
    /// # Examples
    ///
    /// ```
    /// use codemode::{Durability, Journal, JournalReader, Plan, SessionKind};
    ///
    /// let plan = Plan::parse(r#"{"version": 1, "operations": [
    ///   {"id": "label-101", "tool": "issues.add_label",
    ///    "scope": {"provider": "fixture.issues", "account": "triage-bot"},
    ///    "arguments": {"issue": 101, "label": "triage"}}]}"#)?;
    /// let path = std::env::temp_dir().join(format!("codemode-reader-{}.db", std::process::id()));
    /// # let _ = std::fs::remove_file(&path);
    /// let mut journal = Journal::create(&path, &plan, Durability::Process)?;
    /// journal.begin_session(SessionKind::Run)?;
    /// // The writer is still open; a reader sees its committed session.
    /// let reader = JournalReader::open(&path)?;
    /// let session = &reader.history()?.sessions[0];
    /// assert!(session.end.is_none(), "no end recorded yet");
    /// # drop((reader, journal));
    /// # std::fs::remove_file(&path)?;
    /// # for suffix in ["db-wal", "db-shm", "db.lock"] {
    /// #     let _ = std::fs::remove_file(path.with_extension(suffix));
    /// # }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// Returns [`JournalError::NotFound`], [`JournalError::ReadOnly`] if SQLite
    /// cannot read the journal without writing (for example, a write-ahead log
    /// in a read-only directory whose shared-memory index is missing), or
    /// [`JournalError::Corrupt`] or [`JournalError::Incompatible`] if it cannot
    /// be trusted.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(JournalError::NotFound(path));
        }
        let classify = |error: rusqlite::Error| match error.sqlite_error_code() {
            Some(ErrorCode::CannotOpen | ErrorCode::ReadOnly) => JournalError::ReadOnly {
                path: path.clone(),
                detail: error.to_string(),
            },
            Some(ErrorCode::NotADatabase) => JournalError::Incompatible {
                path: path.clone(),
                detail: "not an SQLite database".into(),
            },
            Some(ErrorCode::DatabaseCorrupt) => JournalError::Corrupt {
                path: path.clone(),
                detail: error.to_string(),
            },
            _ => storage(error),
        };
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(classify)?;
        connection.busy_timeout(BUSY_TIMEOUT).map_err(storage)?;
        // The first read starts the snapshot every later read in this reader shares.
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(classify)?;
        connection
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(classify)?;
        let (id, plan) = validate(&connection, &path)?;
        Ok(Self {
            connection,
            path,
            id,
            plan,
        })
    }

    /// The journal file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The journal's identity; see [`Journal::id`].
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The plan this journal records.
    #[must_use]
    pub fn plan(&self) -> &FrozenPlan {
        &self.plan
    }

    /// The execution record stored for `session`'s dispatch run, if any, as of
    /// this reader's snapshot.
    ///
    /// # Errors
    /// Returns [`JournalError::Corrupt`] if the stored record cannot be read.
    pub fn trace(&self, session: u64) -> Result<Option<RunReport>, JournalError> {
        self.view().trace(session)
    }

    /// Every fact committed as of this reader's snapshot.
    ///
    /// # Errors
    /// Returns [`JournalError::Corrupt`] if a stored value cannot be read.
    pub fn history(&self) -> Result<History, JournalError> {
        self.view().history()
    }

    fn view(&self) -> View<'_> {
        View {
            connection: &self.connection,
            path: &self.path,
            plan: &self.plan,
        }
    }
}

/// Reads shared by writers and readers.
struct View<'a> {
    connection: &'a Connection,
    path: &'a Path,
    plan: &'a FrozenPlan,
}

impl View<'_> {
    fn trace(&self, session: u64) -> Result<Option<RunReport>, JournalError> {
        let text: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT trace FROM session_ends WHERE session = ?1",
                [session as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        text.flatten()
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| self.corrupt(format!("session {session} trace: {error}")))
    }

    fn corrupt(&self, detail: String) -> JournalError {
        JournalError::Corrupt {
            path: self.path.to_path_buf(),
            detail,
        }
    }

    fn history(&self) -> Result<History, JournalError> {
        let corrupt =
            |table: &str, error: &dyn fmt::Display| self.corrupt(format!("{table}: {error}"));
        let mut approvals = Vec::new();
        let mut statement = self
            .connection
            .prepare("SELECT binding, record FROM approvals ORDER BY ordinal")
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage)?;
        for row in rows {
            let (binding, record) = row.map_err(storage)?;
            let approval: Approval =
                serde_json::from_str(&record).map_err(|error| corrupt("approvals", &error))?;
            if approval.binding.as_str() != binding {
                return Err(corrupt("approvals", &"binding does not match its record"));
            }
            approvals.push(approval);
        }

        let mut sessions = Vec::new();
        let mut statement = self
            .connection
            .prepare(
                "SELECT s.session, s.ordinal, s.kind, s.pid, s.started_unix_ms, e.ordinal, e.unix_ms,
                        e.status, e.detail, e.trace IS NOT NULL
                 FROM sessions s LEFT JOIN session_ends e USING (session) ORDER BY s.session",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                let end = match row.get::<_, Option<i64>>(5)? {
                    Some(ordinal) => Some(SessionEnd {
                        ordinal: sql_u64(ordinal),
                        unix_ms: sql_u64(row.get(6)?),
                        status: row.get(7)?,
                        detail: row.get(8)?,
                        has_trace: row.get(9)?,
                    }),
                    None => None,
                };
                Ok(SessionRecord {
                    session: sql_u64(row.get(0)?),
                    ordinal: sql_u64(row.get(1)?),
                    kind: row.get(2)?,
                    pid: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    started_unix_ms: sql_u64(row.get(4)?),
                    end,
                })
            })
            .map_err(storage)?;
        for row in rows {
            sessions.push(row.map_err(storage)?);
        }

        let mut attempts = Vec::new();
        let mut statement = self
            .connection
            .prepare(
                "SELECT a.key, a.ordinal, a.operation, a.number, a.session, a.call_id, a.unix_ms,
                        o.ordinal, o.unix_ms, o.record
                 FROM attempts a LEFT JOIN observations o USING (key) ORDER BY a.ordinal",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                let observation = match row.get::<_, Option<i64>>(7)? {
                    Some(ordinal) => Some((
                        sql_u64(ordinal),
                        sql_u64(row.get(8)?),
                        row.get::<_, String>(9)?,
                    )),
                    None => None,
                };
                Ok((
                    AttemptRecord {
                        key: row.get(0)?,
                        ordinal: sql_u64(row.get(1)?),
                        operation: row.get(2)?,
                        number: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        session: sql_u64(row.get(4)?),
                        call_id: row.get::<_, Option<i64>>(5)?.map(sql_u64),
                        unix_ms: sql_u64(row.get(6)?),
                        observation: None,
                    },
                    observation,
                ))
            })
            .map_err(storage)?;
        for row in rows {
            let (mut attempt, observation) = row.map_err(storage)?;
            if let Some((ordinal, unix_ms, record)) = observation {
                let observed: Observed = serde_json::from_str(&record)
                    .map_err(|error| corrupt("observations", &error))?;
                attempt.observation = Some(ObservationRecord {
                    ordinal,
                    unix_ms,
                    observed,
                });
            }
            if self.plan.operation(&attempt.operation).is_none() {
                return Err(corrupt(
                    "attempts",
                    &format!("unknown operation {}", attempt.operation),
                ));
            }
            attempts.push(attempt);
        }

        let mut probes = Vec::new();
        let mut statement = self
            .connection
            .prepare(
                "SELECT p.ordinal, p.operation, p.session, p.keys, p.unix_ms, r.elapsed_ms,
                        r.verdict, r.evidence, r.error_code, r.error_message, r.probe IS NOT NULL
                 FROM probes p LEFT JOIN reconciliations r ON r.probe = p.ordinal
                 ORDER BY p.ordinal",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    sql_u64(row.get(0)?),
                    row.get::<_, String>(1)?,
                    sql_u64(row.get(2)?),
                    row.get::<_, String>(3)?,
                    sql_u64(row.get(4)?),
                    row.get::<_, Option<i64>>(5)?.map_or(0, sql_u64),
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, bool>(10)?,
                ))
            })
            .map_err(storage)?;
        for row in rows {
            let (
                ordinal,
                operation,
                session,
                keys,
                unix_ms,
                elapsed_ms,
                verdict,
                evidence,
                code,
                message,
                answered,
            ) = row.map_err(storage)?;
            let keys: Vec<String> =
                serde_json::from_str(&keys).map_err(|error| corrupt("reconciliations", &error))?;
            let outcome = match verdict.as_deref() {
                None if !answered => Err(ProviderError {
                    code: UNANSWERED.into(),
                    message: "the executor stopped before the probe's answer was recorded".into(),
                }),
                Some(verdict) => Ok(Reconciliation {
                    verdict: Verdict::parse(verdict)
                        .ok_or_else(|| corrupt("reconciliations", &format!("verdict {verdict}")))?,
                    evidence: evidence.unwrap_or_default(),
                }),
                None => Err(ProviderError {
                    code: code.unwrap_or_default(),
                    message: message.unwrap_or_default(),
                }),
            };
            probes.push(ProbeRecord {
                ordinal,
                operation,
                session,
                keys,
                unix_ms,
                elapsed_ms,
                outcome,
            });
        }

        let mut reviews = Vec::new();
        let mut statement = self
            .connection
            .prepare(
                "SELECT ordinal, operation, resolution, reviewer, note, unix_ms FROM reviews ORDER BY ordinal",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    sql_u64(row.get(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    sql_u64(row.get(5)?),
                ))
            })
            .map_err(storage)?;
        for row in rows {
            let (ordinal, operation, resolution, reviewer, note, unix_ms) = row.map_err(storage)?;
            let resolution = Resolution::parse(&resolution)
                .ok_or_else(|| corrupt("reviews", &format!("resolution {resolution}")))?;
            reviews.push(ReviewRecord {
                ordinal,
                operation,
                resolution,
                reviewer,
                note,
                unix_ms,
            });
        }
        Ok(History {
            approvals,
            sessions,
            attempts,
            probes,
            reviews,
        })
    }
}

/// The executor lock file for the journal at `path`: `<path>.lock`.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".lock");
    PathBuf::from(name)
}

/// Take the executor lock for the journal at `path`, failing at once if another
/// executor holds it. The returned file holds the lock until it is closed.
fn acquire(path: &Path) -> Result<File, JournalError> {
    acquire_with(path, |_| {})
}

/// [`acquire`], running `before_lock` between opening and locking the lock file
/// so tests can replace it in that window.
fn acquire_with(path: &Path, before_lock: impl FnOnce(&Path)) -> Result<File, JournalError> {
    let lock_path = lock_path(path);
    let failed = |error: std::io::Error| {
        JournalError::Storage(format!("lock file {}: {error}", lock_path.display()))
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(failed)?;
    before_lock(&lock_path);
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(JournalError::Busy(path.to_path_buf())),
        Err(TryLockError::Error(error)) => return Err(failed(error)),
    }
    // A lock on a file that no longer has this name excludes nobody: another
    // executor opening the name now would lock a different file.
    let named = std::fs::metadata(&lock_path).ok();
    if !named.is_some_and(|named| same_file(&file, &named)) {
        return Err(JournalError::Conflict(format!(
            "lock file {} was removed or replaced while it was being locked; another process \
             is changing the journal's files, so this executor will not start",
            lock_path.display()
        )));
    }
    Ok(file)
}

/// Whether `file` is the file `named` describes.
#[cfg(unix)]
fn same_file(file: &File, named: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    file.metadata()
        .is_ok_and(|held| (held.dev(), held.ino()) == (named.dev(), named.ino()))
}

/// Without device and inode numbers, a still-existing name is all that is checked.
#[cfg(not(unix))]
fn same_file(_file: &File, _named: &std::fs::Metadata) -> bool {
    true
}

/// The journal identity and plan, read and checked before anything is written.
type Stored = (String, FrozenPlan);

/// Open `path` for writing in normal locking mode with the write-ahead log. For
/// an `existing` file, check that it is a sound journal of this format inside a
/// write transaction, before anything is written, so a foreign file is left
/// unchanged. Callers hold the executor lock.
fn connect(
    path: &Path,
    durability: Durability,
    existing: bool,
) -> Result<(Connection, Option<Stored>), JournalError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(storage)?;
    let classify = |error: rusqlite::Error| match error.sqlite_error_code() {
        // Executors exclude each other through the lock file, so this is not one.
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            JournalError::Storage(format!("SQLite stayed busy for {BUSY_TIMEOUT:?}: {error}"))
        }
        Some(ErrorCode::NotADatabase) => JournalError::Incompatible {
            path: path.to_path_buf(),
            detail: "not an SQLite database".into(),
        },
        Some(ErrorCode::DatabaseCorrupt) => JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: error.to_string(),
        },
        _ => storage(error),
    };
    let synchronous = match durability {
        Durability::Storage => "FULL",
        Durability::Process => "NORMAL",
    };
    let fullfsync = match durability {
        Durability::Storage => "ON",
        Durability::Process => "OFF",
    };
    connection.busy_timeout(BUSY_TIMEOUT).map_err(storage)?;
    connection
        .execute_batch(&format!(
            "PRAGMA fullfsync = {fullfsync};
             PRAGMA checkpoint_fullfsync = {fullfsync};
             PRAGMA foreign_keys = ON;"
        ))
        .map_err(classify)?;
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .map_err(classify)?;
    let stored = if existing {
        Some(validate(&connection, path)?)
    } else {
        None
    };
    connection.execute_batch("COMMIT").map_err(classify)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(classify)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(JournalError::Storage(format!(
            "could not enable write-ahead logging (mode {mode})"
        )));
    }
    connection
        .execute_batch(&format!("PRAGMA synchronous = {synchronous};"))
        .map_err(classify)?;
    Ok((connection, stored))
}

/// Fail closed unless `connection` holds an intact journal of this format version
/// whose plan matches its digest; return its identity and plan.
fn validate(connection: &Connection, path: &Path) -> Result<Stored, JournalError> {
    let corrupt = |detail: String| JournalError::Corrupt {
        path: path.to_path_buf(),
        detail,
    };
    let incompatible = |detail: String| JournalError::Incompatible {
        path: path.to_path_buf(),
        detail,
    };
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| match error.sqlite_error_code() {
            Some(ErrorCode::NotADatabase) => incompatible("not an SQLite database".into()),
            _ => corrupt(error.to_string()),
        })?;
    if check != "ok" {
        return Err(corrupt(format!("integrity check: {check}")));
    }
    let meta = |key: &str| -> Result<Option<String>, JournalError> {
        connection
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| incompatible(format!("no journal metadata ({error})")))
    };
    if meta("format")?.as_deref() != Some(FORMAT) {
        return Err(incompatible("not a codemode journal".into()));
    }
    let version = meta("format_version")?.unwrap_or_default();
    if version != JOURNAL_FORMAT_VERSION.to_string() {
        return Err(incompatible(format!(
            "journal format version {version}; this build reads version {JOURNAL_FORMAT_VERSION}"
        )));
    }
    let id = meta("journal_id")?.ok_or_else(|| corrupt("missing journal id".into()))?;
    let (digest, canonical): (String, String) = connection
        .query_row(
            "SELECT digest, canonical FROM plan WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| corrupt(format!("plan: {error}")))?;
    let plan = Plan::parse(&canonical).map_err(|error| corrupt(format!("plan: {error}")))?;
    if plan.digest().as_str() != digest || plan.canonical() != canonical {
        return Err(corrupt("stored plan does not match its digest".into()));
    }
    Ok((id, plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codemode-journal-{}-{name}.db", std::process::id()));
        for suffix in ["db", "db-wal", "db-shm", "db.lock"] {
            let _ = std::fs::remove_file(path.with_extension(suffix));
        }
        path
    }

    fn plan() -> FrozenPlan {
        Plan::parse(
            r#"{"version":1,"operations":[{"id":"a","tool":"t",
                "scope":{"provider":"p","account":"x"},"arguments":{}}]}"#,
        )
        .expect("valid plan")
    }

    #[test]
    fn one_owner_at_a_time_and_facts_survive_reopening() {
        let path = scratch("owner");
        let mut journal = Journal::create(&path, &plan(), Durability::Storage).expect("create");
        assert!(matches!(
            Journal::create(&path, &plan(), Durability::Storage),
            Err(JournalError::Busy(_))
        ));
        let busy = Journal::open(&path, Durability::Storage).expect_err("second writer");
        assert!(matches!(busy, JournalError::Busy(_)), "{busy}");
        assert!(
            busy.to_string()
                .contains("locked by another codemode executor")
        );
        assert!(lock_path(&path).exists());
        let session = journal.begin_session(SessionKind::Run).expect("session");
        journal
            .record_intent("k1", "a", 1, session, Some(1))
            .expect("intent");
        // A reader sees committed facts while the writer holds the journal.
        let reader = JournalReader::open(&path).expect("reader");
        assert_eq!(reader.history().expect("history").attempts.len(), 1);
        journal
            .record_intent("k2", "a", 2, session, Some(2))
            .expect("intent");
        assert_eq!(
            reader.history().expect("history").attempts.len(),
            1,
            "one reader reads one snapshot"
        );
        drop(reader);
        drop(journal);
        assert!(matches!(
            Journal::create(&path, &plan(), Durability::Storage),
            Err(JournalError::Exists(_))
        ));

        let journal = Journal::open(&path, Durability::Process).expect("reopen");
        let history = journal.history().expect("history");
        assert_eq!(history.attempts.len(), 2);
        assert_eq!(history.attempts[0].observation, None);
        assert_eq!(history.sessions[0].end, None);
        drop(journal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn damaged_or_foreign_files_fail_closed() {
        let path = scratch("garbage");
        std::fs::write(&path, b"not a database at all, just text").expect("write");
        let error = Journal::open(&path, Durability::Storage).expect_err("garbage");
        assert!(
            matches!(error, JournalError::Incompatible { .. }),
            "{error}"
        );

        let path = scratch("tampered");
        drop(Journal::create(&path, &plan(), Durability::Storage).expect("create"));
        let connection = Connection::open(&path).expect("open");
        connection
            .execute(
                "UPDATE plan SET canonical = replace(canonical, '\"t\"', '\"u\"')",
                [],
            )
            .expect("tamper");
        drop(connection);
        let error = Journal::open(&path, Durability::Storage).expect_err("tampered");
        assert!(
            error.to_string().contains("does not match its digest"),
            "{error}"
        );

        // A damaged journal in rollback mode stays in rollback mode.
        let connection = Connection::open(&path).expect("open");
        let mode: String = connection
            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
            .expect("rollback mode");
        assert_eq!(mode, "delete");
        drop(connection);
        let before = std::fs::read(&path).expect("read");
        Journal::open(&path, Durability::Storage).expect_err("still tampered");
        assert_eq!(std::fs::read(&path).expect("read"), before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_journal_from_the_previous_format_is_refused() {
        let path = scratch("previous-format");
        drop(Journal::create(&path, &plan(), Durability::Storage).expect("create"));
        let connection = Connection::open(&path).expect("open");
        connection
            .execute(
                "UPDATE meta SET value = '1' WHERE key = 'format_version'",
                [],
            )
            .expect("downgrade");
        drop(connection);
        let error = Journal::open(&path, Durability::Storage).expect_err("previous format");
        assert!(
            matches!(error, JournalError::Incompatible { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("format version 1"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn journals_for_one_plan_never_share_an_identity() {
        let ids: std::collections::HashSet<String> = (0..64).map(|_| journal_id(&plan())).collect();
        assert_eq!(ids.len(), 64);
    }

    #[test]
    fn a_probe_counts_from_its_start_even_without_an_answer() {
        let path = scratch("probe");
        let mut journal = Journal::create(&path, &plan(), Durability::Process).expect("create");
        let session = journal.begin_session(SessionKind::Resume).expect("session");
        journal
            .record_probe_start("a", session, &["k1".into()])
            .expect("probe start");
        drop(journal);
        let journal = Journal::open(&path, Durability::Process).expect("reopen");
        let probes = journal.history().expect("history").probes;
        assert_eq!(probes.len(), 1);
        assert_eq!(
            probes[0]
                .outcome
                .as_ref()
                .map_err(|error| error.code.as_str()),
            Err(UNANSWERED)
        );
        drop(journal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_summary_keeps_at_most_one_kib_and_metadata_keeps_none() {
        let path = scratch("retention");
        let mut journal = Journal::create(&path, &plan(), Durability::Process).expect("create");
        let session = journal.begin_session(SessionKind::Resume).expect("session");
        let long = "e".repeat(3 * MAX_TEXT_BYTES);
        for retention in [Retention::Summary, Retention::Metadata] {
            let probe = journal
                .record_probe_start("a", session, &[])
                .expect("probe start");
            let answer = Ok(Reconciliation {
                verdict: Verdict::Unknown,
                evidence: long.clone(),
            });
            journal
                .record_probe(probe, 1, &answer, retention)
                .expect("probe");
        }
        let kept: Vec<usize> = journal
            .history()
            .expect("history")
            .probes
            .iter()
            .map(|probe| probe.outcome.as_ref().expect("answered").evidence.len())
            .collect();
        assert_eq!(kept, [MAX_TEXT_BYTES, 0]);
        drop(journal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_a_foreign_database_leaves_it_unchanged() {
        let path = scratch("foreign");
        let foreign = Connection::open(&path).expect("create");
        foreign
            .execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('kept');")
            .expect("write");
        drop(foreign);
        let before = std::fs::read(&path).expect("read");
        let error = Journal::open(&path, Durability::Storage).expect_err("foreign");
        assert!(
            matches!(error, JournalError::Incompatible { .. }),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).expect("read"), before);
        assert!(!path.with_extension("db-wal").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_executor_refuses_a_lock_file_replaced_before_it_was_locked() {
        let path = scratch("replaced-lock");
        drop(Journal::create(&path, &plan(), Durability::Process).expect("create"));
        let error = acquire_with(&path, |lock| {
            std::fs::remove_file(lock).expect("unlink");
            std::fs::write(lock, b"").expect("replace");
        })
        .expect_err("replaced lock file");
        assert!(matches!(error, JournalError::Conflict(_)), "{error}");
        assert!(error.to_string().contains("removed or replaced"), "{error}");

        let error = acquire_with(&path, |lock| std::fs::remove_file(lock).expect("unlink"))
            .expect_err("removed lock file");
        assert!(error.to_string().contains("removed or replaced"), "{error}");
        // With the lock file in place, the journal opens.
        drop(Journal::open(&path, Durability::Process).expect("open"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn approver_and_note_are_kept_to_one_kib() {
        let path = scratch("approval-text");
        let plan = plan();
        let mut journal = Journal::create(&path, &plan, Durability::Process).expect("create");
        let declaration = crate::recovery::ProviderDeclaration {
            provider: "p".into(),
            account: "x".into(),
            contracts: std::collections::BTreeMap::new(),
            retention: Retention::default(),
        };
        let policy = crate::recovery::RecoveryPolicy::default();
        let long = "\u{e9}".repeat(MAX_TEXT_BYTES);
        let approval = Approval {
            binding: crate::recovery::approval_binding(plan.digest(), &declaration, &policy),
            plan: plan.digest().clone(),
            declaration,
            policy,
            approver: long.clone(),
            note: Some(long),
            approved_unix_ms: 0,
        };
        journal.approve(&approval).expect("approve");
        let stored = journal.history().expect("history").approvals.remove(0);
        assert_eq!(stored.approver.len(), MAX_TEXT_BYTES);
        assert_eq!(stored.note.as_deref().map(str::len), Some(MAX_TEXT_BYTES));
        assert_eq!(stored.binding, approval.binding);
        drop(journal);
        let _ = std::fs::remove_file(&path);
    }
}
