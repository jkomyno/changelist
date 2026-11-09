//! A durable, single-owner operation journal in SQLite.
//!
//! The journal records one frozen plan and append-only facts about it: approvals,
//! sessions, dispatch intents, observations, reconciliation probes, and reviews.
//! Every fact carries an ordinal from one counter, so facts can be folded in the
//! order they were committed.
//!
//! # Durability
//!
//! The journal uses SQLite's write-ahead log in exclusive locking mode. With
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
//! Opening a journal takes SQLite's exclusive lock and holds it until the
//! [`Journal`] is dropped or its process exits, so two executors can never own
//! one journal. A second open fails with [`JournalError::Busy`].

use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::OpenOptions;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, params};

use crate::plan::{Digest, FrozenPlan, Plan};
use crate::recovery::{Approval, Observed, Reconciliation, Resolution, Verdict};
use crate::runtime::{ProviderError, RunReport};

/// The journal format this build reads and writes.
pub const JOURNAL_FORMAT_VERSION: u32 = 1;
const FORMAT: &str = "codemode-journal";
/// Largest provider result kept per observation; larger results keep only their size.
const MAX_VALUE_BYTES: usize = 16 * 1024;
/// Error code of a probe whose answer was never recorded, for example because
/// the executor was killed while the provider was answering it.
pub const UNANSWERED: &str = "unanswered";
/// Largest kept message, evidence, note, or diagnostic.
const MAX_TEXT_BYTES: usize = 1024;

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
#[derive(Debug)]
pub enum JournalError {
    /// Another process holds the journal's lock.
    Busy(PathBuf),
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
                "journal {} is locked by another codemode process (a run, resume, review, or \
                 inspection); wait for it to exit",
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
    /// How it ended; absent when the executor stopped without recording an end.
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

/// An open, exclusively locked journal for one plan.
pub struct Journal {
    connection: Connection,
    path: PathBuf,
    id: String,
    plan: FrozenPlan,
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

fn storage(error: rusqlite::Error) -> JournalError {
    JournalError::Storage(error.to_string())
}

fn sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Keep a result only when it is small; its size is always kept.
pub(crate) fn bounded_observation(observed: Observed) -> Observed {
    match observed {
        Observed::Succeeded { value, bytes } => Observed::Succeeded {
            value: value.filter(|_| bytes <= MAX_VALUE_BYTES as u64),
            bytes,
        },
        Observed::Failed { error, effect } => Observed::Failed {
            error: ProviderError {
                code: bounded(&error.code),
                message: bounded(&error.message),
            },
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
    /// # Errors
    /// Returns [`JournalError::Exists`] if the file exists, or a storage error.
    pub fn create(
        path: impl AsRef<Path>,
        plan: &FrozenPlan,
        durability: Durability,
    ) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => JournalError::Exists(path.clone()),
                _ => JournalError::Storage(error.to_string()),
            })?;
        let mut connection = lock(&path, durability)?;
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
        Ok(Self {
            connection,
            path,
            id,
            plan: plan.clone(),
        })
    }

    /// Open and lock an existing journal, checking its integrity and format.
    ///
    /// # Errors
    /// Returns [`JournalError::Busy`] if another process holds it,
    /// [`JournalError::Corrupt`] or [`JournalError::Incompatible`] if it cannot be
    /// trusted, or [`JournalError::NotFound`].
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(JournalError::NotFound(path));
        }
        let connection = lock(&path, durability)?;
        let corrupt = |detail: String| JournalError::Corrupt {
            path: path.clone(),
            detail,
        };
        let check: String = connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|error| corrupt(error.to_string()))?;
        if check != "ok" {
            return Err(corrupt(format!("integrity check: {check}")));
        }
        let meta = |key: &str| -> Result<Option<String>, JournalError> {
            connection
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(|error| JournalError::Incompatible {
                    path: path.clone(),
                    detail: format!("no journal metadata ({error})"),
                })
        };
        if meta("format")?.as_deref() != Some(FORMAT) {
            return Err(JournalError::Incompatible {
                path,
                detail: "not a codemode journal".into(),
            });
        }
        let version = meta("format_version")?.unwrap_or_default();
        if version != JOURNAL_FORMAT_VERSION.to_string() {
            return Err(JournalError::Incompatible {
                path,
                detail: format!(
                    "journal format version {version}; this build reads version {JOURNAL_FORMAT_VERSION}"
                ),
            });
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

    fn write<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>, i64) -> rusqlite::Result<T>,
    ) -> Result<T, JournalError> {
        let transaction = self.connection.transaction().map_err(storage)?;
        let ordinal: i64 = transaction
            .query_row(
                "UPDATE clock SET ordinal = ordinal + 1 WHERE id = 1 RETURNING ordinal",
                [],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let value = body(&transaction, ordinal).map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(value)
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
        let record = serde_json::to_string(approval)
            .map_err(|error| JournalError::Storage(error.to_string()))?;
        self.write(|transaction, ordinal| {
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
        self.write(|transaction, ordinal| {
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
            .map_err(|error| JournalError::Storage(error.to_string()))?;
        let detail = detail.map(bounded);
        self.write(|transaction, ordinal| {
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
        self.write(|transaction, ordinal| {
            transaction.execute(
                "INSERT INTO attempts VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key,
                    ordinal,
                    operation,
                    number,
                    session as i64,
                    call_id.map(|id| id as i64),
                    now_ms() as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Durably record an attempt's observed result.
    ///
    /// # Errors
    /// Returns a storage error, in which case the result must not be reported as recorded.
    pub fn record_observation(
        &mut self,
        key: &str,
        observed: Observed,
    ) -> Result<(), JournalError> {
        let record = serde_json::to_string(&bounded_observation(observed))
            .map_err(|error| JournalError::Storage(error.to_string()))?;
        self.write(|transaction, ordinal| {
            transaction.execute(
                "INSERT INTO observations VALUES (?1, ?2, ?3, ?4)",
                params![key, ordinal, now_ms() as i64, record],
            )?;
            Ok(())
        })
    }

    /// Record every attempt of `session` that has no observation as unobserved,
    /// because the run that dispatched it stopped waiting (`reason`). One commit.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn record_unobserved(&mut self, session: u64, reason: &str) -> Result<(), JournalError> {
        let record = serde_json::to_string(&bounded_observation(Observed::Unobserved {
            reason: reason.to_string(),
        }))
        .map_err(|error| JournalError::Storage(error.to_string()))?;
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
        for key in keys {
            let ordinal: i64 = transaction
                .query_row(
                    "UPDATE clock SET ordinal = ordinal + 1 WHERE id = 1 RETURNING ordinal",
                    [],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            transaction
                .execute(
                    "INSERT INTO observations VALUES (?1, ?2, ?3, ?4)",
                    params![key, ordinal, now_ms() as i64, record],
                )
                .map_err(storage)?;
        }
        transaction.commit().map_err(storage)
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
        let keys = serde_json::to_string(keys)
            .map_err(|error| JournalError::Storage(error.to_string()))?;
        self.write(|transaction, ordinal| {
            transaction.execute(
                "INSERT INTO probes VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ordinal, operation, session as i64, keys, now_ms() as i64],
            )?;
            Ok(sql_u64(ordinal))
        })
    }

    /// Record the provider's answer to the probe `probe` returned by
    /// [`Journal::record_probe_start`], or the probe's own failure.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn record_probe(
        &mut self,
        probe: u64,
        elapsed_ms: u64,
        outcome: &Result<Reconciliation, ProviderError>,
    ) -> Result<(), JournalError> {
        let (verdict, evidence, code, message) = match outcome {
            Ok(reconciliation) => (
                Some(reconciliation.verdict.as_str()),
                Some(bounded(&reconciliation.evidence)),
                None,
                None,
            ),
            Err(error) => (
                None,
                None,
                Some(bounded(&error.code)),
                Some(bounded(&error.message)),
            ),
        };
        self.write(|transaction, ordinal| {
            transaction.execute(
                "INSERT INTO reconciliations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    probe as i64,
                    ordinal,
                    now_ms() as i64,
                    elapsed_ms as i64,
                    verdict,
                    evidence,
                    code,
                    message
                ],
            )?;
            Ok(())
        })
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
        self.write(|transaction, ordinal| {
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
            path: self.path.clone(),
            detail,
        }
    }

    /// Read every recorded fact.
    ///
    /// # Errors
    /// Returns [`JournalError::Corrupt`] if a stored value cannot be read.
    pub fn history(&self) -> Result<History, JournalError> {
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

/// Open `path` and take SQLite's exclusive lock, failing at once if it is held.
fn lock(path: &Path, durability: Durability) -> Result<Connection, JournalError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(storage)?;
    let classify = |error: rusqlite::Error| match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            JournalError::Busy(path.to_path_buf())
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
    connection
        .execute_batch(&format!(
            "PRAGMA busy_timeout = 0;
             PRAGMA locking_mode = EXCLUSIVE;
             PRAGMA fullfsync = {fullfsync};
             PRAGMA checkpoint_fullfsync = {fullfsync};
             PRAGMA foreign_keys = ON;"
        ))
        .map_err(classify)?;
    // Entering WAL mode and an immediate write transaction both need the lock;
    // in exclusive locking mode it is then held until the connection closes.
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(classify)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(JournalError::Storage(format!(
            "could not enable write-ahead logging (mode {mode})"
        )));
    }
    connection
        .execute_batch(&format!(
            "PRAGMA synchronous = {synchronous}; BEGIN EXCLUSIVE; COMMIT;"
        ))
        .map_err(classify)?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("codemode-journal-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
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
            Err(JournalError::Exists(_))
        ));
        assert!(matches!(
            Journal::open(&path, Durability::Storage),
            Err(JournalError::Busy(_))
        ));
        let session = journal.begin_session(SessionKind::Run).expect("session");
        journal
            .record_intent("k1", "a", 1, session, Some(1))
            .expect("intent");
        drop(journal);

        let journal = Journal::open(&path, Durability::Process).expect("reopen");
        let history = journal.history().expect("history");
        assert_eq!(history.attempts.len(), 1);
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
}
