//! An offline issue tracker with an independent side-effect ledger.
//!
//! The fixture plays a remote issue tracker for demos, tests, and baselines. Its
//! SQLite file is separate from any executor journal and records every request it
//! receives in a ledger, in the same transaction as the request's effect. The
//! ledger is the correctness oracle: it distinguishes repeated *requests*, which
//! are harmless, from repeated *effects*, which are duplicates.
//!
//! [`Fixture`] creates, configures, and reads a fixture file. [`FixtureProvider`]
//! serves its tools as a [`RecoverableProvider`] acting as one account:
//!
//! | Tool                 | Arguments         | Repeat   | Reconcile |
//! |----------------------|-------------------|----------|-----------|
//! | `issues.add_label`   | `{issue, label}`  | `safe`   | `receipt` |
//! | `issues.remove_label`| `{issue, label}`  | `safe`   | `state`   |
//! | `issues.add_comment` | `{issue, body}`   | `unsafe` | `none`    |
//!
//! The only supported precondition is `{"issue_state": "open" | "closed"}`,
//! checked in the same transaction as the effect.
//!
//! # Durability and sharing
//!
//! The file uses SQLite's write-ahead log with `synchronous=FULL` and normal
//! locking, so several processes can use it at once; writers wait up to five
//! seconds for each other. A request that committed survives its caller being
//! killed.
//!
//! # Faults
//!
//! A fault ([`FaultMode`]) is consumed in the transaction of the request that
//! triggers it, so a restarted caller never triggers a consumed fault again. Both
//! modes leave the request pending forever after the transaction commits; only the
//! caller's deadline, cancellation, or death ends it.

use std::collections::HashMap;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{sleep, sleep_until};

use crate::journal::now_ms;
use crate::plan::{Digest, Operation, Scope};
use crate::recovery::{
    Attempt, AttemptError, FailureEffect, ProbeContext, ProviderDeclaration, ReconcileMethod,
    Reconciliation, RecoverableProvider, RecoveryContract, RepeatSafety, Verdict,
};
use crate::runtime::{CallContext, ProviderError};

/// The fixture format this build reads and writes.
pub const FIXTURE_FORMAT_VERSION: u32 = 1;
/// The provider name in [`FixtureProvider`]'s declaration.
pub const FIXTURE_PROVIDER: &str = "fixture.issues";
/// The actor recorded for [`Fixture::external_label`] edits.
pub const EXTERNAL_ACTOR: &str = "external";

const FORMAT: &str = "codemode-fixture";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const ADD_LABEL: &str = "issues.add_label";
const REMOVE_LABEL: &str = "issues.remove_label";
const ADD_COMMENT: &str = "issues.add_comment";
/// Characters of a comment body kept in its ledger summary.
const BODY_SUMMARY_CHARS: usize = 40;

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE accounts (
  name TEXT PRIMARY KEY,
  revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1))
) STRICT;
CREATE TABLE issues (
  number INTEGER PRIMARY KEY CHECK (number > 0),
  state TEXT NOT NULL CHECK (state IN ('open', 'closed'))
) STRICT;
CREATE TABLE labels (
  issue INTEGER NOT NULL REFERENCES issues (number),
  label TEXT NOT NULL,
  PRIMARY KEY (issue, label)
) STRICT;
CREATE TABLE comments (
  id INTEGER PRIMARY KEY,
  issue INTEGER NOT NULL REFERENCES issues (number),
  body TEXT NOT NULL
) STRICT;
CREATE TABLE ledger (
  seq INTEGER PRIMARY KEY,
  unix_ms INTEGER NOT NULL,
  actor TEXT NOT NULL,
  tool TEXT NOT NULL,
  attempt_key TEXT,
  issue INTEGER,
  detail TEXT,
  outcome TEXT NOT NULL,
  effect INTEGER NOT NULL CHECK (effect IN (0, 1))
) STRICT;
CREATE TABLE receipts (
  attempt_key TEXT PRIMARY KEY,
  seq INTEGER NOT NULL REFERENCES ledger (seq)
) STRICT;
CREATE TABLE fences (attempt_key TEXT PRIMARY KEY, unix_ms INTEGER NOT NULL) STRICT;
CREATE TABLE faults (
  id INTEGER PRIMARY KEY,
  tool TEXT NOT NULL,
  issue INTEGER NOT NULL,
  mode TEXT NOT NULL CHECK (mode IN ('lose_response', 'hold')),
  remaining INTEGER NOT NULL CHECK (remaining >= 0)
) STRICT;
";

/// Why a fixture operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureError {
    /// [`Fixture::create`] found an existing file.
    Exists(PathBuf),
    /// The fixture file does not exist.
    NotFound(PathBuf),
    /// The file is not a fixture of [`FIXTURE_FORMAT_VERSION`].
    Incompatible {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
    /// A seed, account, tool, issue, or edit was refused.
    Invalid(String),
    /// SQLite or the filesystem failed.
    Storage(String),
}

impl fmt::Display for FixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exists(path) => write!(formatter, "{} already exists", path.display()),
            Self::NotFound(path) => write!(formatter, "{} does not exist", path.display()),
            Self::Incompatible { path, detail } => {
                write!(formatter, "{} is not usable: {detail}", path.display())
            }
            Self::Invalid(message) => formatter.write_str(message),
            Self::Storage(message) => write!(formatter, "fixture storage: {message}"),
        }
    }
}

impl std::error::Error for FixtureError {}

impl From<rusqlite::Error> for FixtureError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// The initial contents of a fixture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seed {
    /// Repository name, such as `acme/widgets`; operations may name it as their destination.
    pub repository: String,
    /// Accounts, all initially active.
    pub accounts: Vec<String>,
    /// Issue numbers, all initially open.
    pub issues: Vec<u64>,
    /// Issues, among `issues`, that start closed.
    pub closed: Vec<u64>,
    /// Initial labels as `(issue, label)`.
    pub labels: Vec<(u64, String)>,
}

/// How a fault disturbs the next matching request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultMode {
    /// Apply and commit normally, then never return.
    LoseResponse,
    /// Ledger the request as `held` without applying it, then never return.
    Hold,
}

impl FaultMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::LoseResponse => "lose_response",
            Self::Hold => "hold",
        }
    }
}

/// An edit made outside codemode by [`Fixture::external_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelEdit {
    /// Add the label.
    Add,
    /// Remove the label.
    Remove,
}

/// One request the fixture received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerRow {
    /// Order of receipt, starting at 1.
    pub seq: u64,
    /// Wall-clock time of receipt, Unix milliseconds.
    pub unix_ms: u64,
    /// The account name, or [`EXTERNAL_ACTOR`].
    pub actor: String,
    /// The requested tool, as sent.
    pub tool: String,
    /// The attempt key, when the request carried one.
    pub key: Option<String>,
    /// The issue, when the arguments were valid.
    pub issue: Option<u64>,
    /// The label, or a comment body summary, when the arguments were valid.
    pub detail: Option<String>,
    /// `applied`, `no_change`, `held`, or `refused:<code>`.
    pub outcome: String,
    /// Whether this request changed state.
    pub effect: bool,
}

/// Requests and effects for one logical change: the same tool, issue, and detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeSummary {
    /// The tool.
    pub tool: String,
    /// The issue, if known.
    pub issue: Option<u64>,
    /// The label or comment body summary, if known.
    pub detail: Option<String>,
    /// Requests received for this change.
    pub requests: u64,
    /// Requests that changed state.
    pub effects: u64,
    /// Effects beyond the first: repeated effects of one logical change.
    pub duplicates: u64,
}

/// The whole ledger, with a summary per logical change in order of first request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ledger {
    /// Every request, in order.
    pub rows: Vec<LedgerRow>,
    /// One entry per logical change.
    pub changes: Vec<ChangeSummary>,
}

/// One account and whether it is revoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountState {
    /// Account name.
    pub name: String,
    /// Whether requests from it are refused.
    pub revoked: bool,
}

/// One comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comment {
    /// Comment identity, increasing.
    pub id: u64,
    /// Comment text.
    pub body: String,
}

/// One issue's current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueState {
    /// Issue number.
    pub number: u64,
    /// `open` or `closed`.
    pub state: String,
    /// Labels, sorted.
    pub labels: Vec<String>,
    /// Comments, oldest first.
    pub comments: Vec<Comment>,
}

/// The fixture's current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    /// Repository name.
    pub repository: String,
    /// Per-request latency.
    pub latency_ms: u64,
    /// Accounts, sorted by name.
    pub accounts: Vec<AccountState>,
    /// Issues, sorted by number.
    pub issues: Vec<IssueState>,
}

/// An open fixture file, for setup and inspection.
#[derive(Debug)]
pub struct Fixture {
    connection: Connection,
}

impl Fixture {
    /// Create a fixture at `path`, which must not exist, with the contents of `seed`.
    ///
    /// # Errors
    /// Returns [`FixtureError::Invalid`] for an empty repository, no accounts or
    /// issues, issue 0, or a closed issue or label on an unknown issue;
    /// [`FixtureError::Exists`] if the file exists; or a storage error.
    pub fn create(path: impl AsRef<Path>, seed: &Seed) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        validate_seed(seed)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => FixtureError::Exists(path.to_path_buf()),
                _ => FixtureError::Storage(error.to_string()),
            })?;
        let mut connection = connect(path)?;
        let mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(FixtureError::Storage(format!(
                "could not enable write-ahead logging (mode {mode})"
            )));
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        for (key, value) in [
            ("format", FORMAT.to_string()),
            ("format_version", FIXTURE_FORMAT_VERSION.to_string()),
            ("repository", seed.repository.clone()),
            ("latency_ms", "0".to_string()),
        ] {
            transaction.execute("INSERT INTO meta VALUES (?1, ?2)", params![key, value])?;
        }
        for account in &seed.accounts {
            transaction.execute(
                "INSERT OR IGNORE INTO accounts (name) VALUES (?1)",
                [account],
            )?;
        }
        for issue in &seed.issues {
            let state = if seed.closed.contains(issue) {
                "closed"
            } else {
                "open"
            };
            transaction.execute(
                "INSERT OR IGNORE INTO issues VALUES (?1, ?2)",
                params![issue, state],
            )?;
        }
        for (issue, label) in &seed.labels {
            transaction.execute(
                "INSERT OR IGNORE INTO labels VALUES (?1, ?2)",
                params![issue, label],
            )?;
        }
        transaction.commit()?;
        Ok(Self { connection })
    }

    /// Open an existing fixture, checking its format.
    ///
    /// # Errors
    /// Returns [`FixtureError::NotFound`], [`FixtureError::Incompatible`] for a
    /// file that is not a fixture of this version, or a storage error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        let connection = open_checked(path)?;
        Ok(Self { connection })
    }

    /// Delay every later provider request by `ms` milliseconds before it is handled.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn set_latency(&self, ms: u64) -> Result<(), FixtureError> {
        self.connection.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'latency_ms'",
            [ms.to_string()],
        )?;
        Ok(())
    }

    /// Disturb the next `count` provider requests for `tool` on `issue`.
    ///
    /// # Errors
    /// Returns [`FixtureError::Invalid`] for an unknown tool, or a storage error.
    pub fn add_fault(
        &self,
        tool: &str,
        issue: u64,
        mode: FaultMode,
        count: u32,
    ) -> Result<(), FixtureError> {
        if ![ADD_LABEL, REMOVE_LABEL, ADD_COMMENT].contains(&tool) {
            return Err(FixtureError::Invalid(format!("unknown tool '{tool}'")));
        }
        self.connection.execute(
            "INSERT INTO faults (tool, issue, mode, remaining) VALUES (?1, ?2, ?3, ?4)",
            params![tool, issue, mode.as_str(), count],
        )?;
        Ok(())
    }

    /// Revoke or restore `account`; requests from a revoked account are refused.
    ///
    /// # Errors
    /// Returns [`FixtureError::Invalid`] for an unknown account, or a storage error.
    pub fn set_revoked(&self, account: &str, revoked: bool) -> Result<(), FixtureError> {
        let changed = self.connection.execute(
            "UPDATE accounts SET revoked = ?1 WHERE name = ?2",
            params![revoked, account],
        )?;
        if changed == 0 {
            return Err(FixtureError::Invalid(format!(
                "unknown account '{account}'"
            )));
        }
        Ok(())
    }

    /// Add or remove a label as someone outside codemode. The request is ledgered
    /// with actor [`EXTERNAL_ACTOR`] and no attempt key. Returns whether it changed state.
    ///
    /// # Errors
    /// Returns [`FixtureError::Invalid`] for an unknown issue or empty label, or a storage error.
    pub fn external_label(
        &mut self,
        issue: u64,
        label: &str,
        edit: LabelEdit,
    ) -> Result<bool, FixtureError> {
        let tool = match edit {
            LabelEdit::Add => ADD_LABEL,
            LabelEdit::Remove => REMOVE_LABEL,
        };
        let arguments = json!({"issue": issue, "label": label});
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let processed = process(
            &transaction,
            &Request {
                actor: Actor::External,
                tool,
                key: None,
                arguments: &arguments,
                preconditions: &[],
            },
        )?;
        transaction.commit()?;
        match processed.response {
            Ok(value) => Ok(value["changed"].as_bool().unwrap_or(false)),
            Err(refusal) => Err(FixtureError::Invalid(refusal.message)),
        }
    }

    /// Every ledgered request, with a summary per logical change.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn ledger(&self) -> Result<Ledger, FixtureError> {
        let mut statement = self.connection.prepare(
            "SELECT seq, unix_ms, actor, tool, attempt_key, issue, detail, outcome, effect
             FROM ledger ORDER BY seq",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(LedgerRow {
                    seq: row.get(0)?,
                    unix_ms: row.get(1)?,
                    actor: row.get(2)?,
                    tool: row.get(3)?,
                    key: row.get(4)?,
                    issue: row.get(5)?,
                    detail: row.get(6)?,
                    outcome: row.get(7)?,
                    effect: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut changes: Vec<ChangeSummary> = Vec::new();
        let mut index = HashMap::new();
        for row in &rows {
            let position = *index
                .entry((row.tool.clone(), row.issue, row.detail.clone()))
                .or_insert_with(|| {
                    changes.push(ChangeSummary {
                        tool: row.tool.clone(),
                        issue: row.issue,
                        detail: row.detail.clone(),
                        requests: 0,
                        effects: 0,
                        duplicates: 0,
                    });
                    changes.len() - 1
                });
            let change = &mut changes[position];
            change.requests += 1;
            change.effects += u64::from(row.effect);
            change.duplicates = change.effects.saturating_sub(1);
        }
        Ok(Ledger { rows, changes })
    }

    /// The current repository, latency, accounts, and issues.
    ///
    /// # Errors
    /// Returns a storage error.
    pub fn state(&self) -> Result<State, FixtureError> {
        let connection = &self.connection;
        let repository = meta(connection, "repository")?.unwrap_or_default();
        let latency_ms = read_latency(connection)?;
        let accounts = connection
            .prepare("SELECT name, revoked FROM accounts ORDER BY name")?
            .query_map([], |row| {
                Ok(AccountState {
                    name: row.get(0)?,
                    revoked: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut issues = connection
            .prepare("SELECT number, state FROM issues ORDER BY number")?
            .query_map([], |row| {
                Ok(IssueState {
                    number: row.get(0)?,
                    state: row.get(1)?,
                    labels: Vec::new(),
                    comments: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut labels =
            connection.prepare("SELECT label FROM labels WHERE issue = ?1 ORDER BY label")?;
        let mut comments =
            connection.prepare("SELECT id, body FROM comments WHERE issue = ?1 ORDER BY id")?;
        for issue in &mut issues {
            issue.labels = labels
                .query_map([issue.number], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            issue.comments = comments
                .query_map([issue.number], |row| {
                    Ok(Comment {
                        id: row.get(0)?,
                        body: row.get(1)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
        }
        Ok(State {
            repository,
            latency_ms,
            accounts,
            issues,
        })
    }
}

/// The fixture's tools, served as one account through a [`RecoverableProvider`].
///
/// Each request opens its own connection, so several providers and processes can
/// share one fixture file.
#[derive(Debug, Clone)]
pub struct FixtureProvider {
    path: PathBuf,
    account: String,
    repository: String,
}

impl FixtureProvider {
    /// Serve the fixture at `path` as `account`.
    ///
    /// # Errors
    /// Returns [`FixtureError::NotFound`] or [`FixtureError::Incompatible`] for a
    /// file that is not a fixture of this version, [`FixtureError::Invalid`] for an
    /// unknown account, or a storage error.
    pub fn open(path: impl AsRef<Path>, account: &str) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        let connection = open_checked(path)?;
        let known = connection
            .query_row("SELECT 1 FROM accounts WHERE name = ?1", [account], |_| {
                Ok(())
            })
            .optional()?;
        if known.is_none() {
            return Err(FixtureError::Invalid(format!(
                "unknown account '{account}'"
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
            account: account.to_string(),
            repository: meta(&connection, "repository")?.unwrap_or_default(),
        })
    }

    /// Execute one attempt with the deadline and cancellation of `context`;
    /// [`RecoverableProvider::execute`] delegates here.
    ///
    /// The configured latency elapses first; a deadline or cancellation during it
    /// returns [`FailureEffect::NotApplied`]. The request is then ledgered and, if
    /// accepted, applied in one transaction. A matching fault makes the call pend
    /// forever after that transaction commits.
    ///
    /// # Errors
    /// Returns [`FailureEffect::Refused`] for a refusal (`permission_denied`,
    /// `unknown_tool`, `invalid_arguments`, `attempt_fenced`, `not_found`,
    /// `precondition_failed`), [`FailureEffect::NotApplied`] when storage failed
    /// before the transaction could commit, and [`FailureEffect::Unknown`] when the
    /// commit itself failed.
    pub async fn apply(
        &self,
        attempt: Attempt,
        context: ProbeContext,
    ) -> Result<Value, AttemptError> {
        let path = self.path.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let connection = connect(&path)?;
            let latency = read_latency(&connection)?;
            Ok::<_, FixtureError>((connection, latency))
        })
        .await;
        let (mut connection, latency) = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return Err(storage_failure(error, FailureEffect::NotApplied)),
            Err(error) => return Err(storage_failure(error, FailureEffect::NotApplied)),
        };
        if latency > 0 {
            tokio::select! {
                () = sleep(Duration::from_millis(latency)) => {}
                () = sleep_until(context.deadline) => {
                    return Err(not_applied("deadline", "deadline reached before the request was sent"));
                }
                () = context.cancellation.cancelled() => {
                    return Err(not_applied("cancelled", "cancelled before the request was sent"));
                }
            }
        }
        let in_scope = self.in_scope(&attempt.operation.scope);
        let account = self.account.clone();
        let handled = tokio::task::spawn_blocking(move || {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| (error, FailureEffect::NotApplied))?;
            let operation = &attempt.operation;
            let request = Request {
                actor: Actor::Account {
                    name: &account,
                    in_scope,
                },
                tool: &operation.tool,
                key: Some(&attempt.key),
                arguments: &operation.arguments,
                preconditions: &operation.preconditions,
            };
            // A failure before commit rolls the transaction back: nothing applied.
            let processed = process(&transaction, &request)
                .map_err(|error| (error, FailureEffect::NotApplied))?;
            transaction
                .commit()
                .map_err(|error| (error, FailureEffect::Unknown))?;
            Ok(processed)
        })
        .await;
        let processed = match handled {
            Ok(Ok(processed)) => processed,
            Ok(Err((error, effect))) => return Err(storage_failure(error, effect)),
            Err(error) => return Err(storage_failure(error, FailureEffect::Unknown)),
        };
        if processed.pend {
            return std::future::pending().await;
        }
        processed.response.map_err(|refusal| AttemptError {
            error: refusal.into(),
            effect: FailureEffect::Refused,
        })
    }

    fn in_scope(&self, scope: &Scope) -> bool {
        scope.provider == FIXTURE_PROVIDER
            && scope.account == self.account
            && scope
                .destination
                .as_ref()
                .is_none_or(|destination| *destination == self.repository)
    }
}

impl RecoverableProvider for FixtureProvider {
    fn declaration(&self) -> ProviderDeclaration {
        let contract = |repeat, reconcile| RecoveryContract { repeat, reconcile };
        ProviderDeclaration {
            provider: FIXTURE_PROVIDER.to_string(),
            account: self.account.clone(),
            contracts: [
                (
                    ADD_LABEL,
                    contract(RepeatSafety::Safe, ReconcileMethod::Receipt),
                ),
                (
                    REMOVE_LABEL,
                    contract(RepeatSafety::Safe, ReconcileMethod::State),
                ),
                (
                    ADD_COMMENT,
                    contract(RepeatSafety::Unsafe, ReconcileMethod::None),
                ),
            ]
            .into_iter()
            .map(|(tool, contract)| (tool.to_string(), contract))
            .collect(),
        }
    }

    async fn authorize(&self, scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        let unauthorized = |message: String| ProviderError {
            code: "unauthorized".into(),
            message,
        };
        if !self.in_scope(scope) {
            return Err(unauthorized(format!(
                "this provider acts as {FIXTURE_PROVIDER}/{} on {}, not {scope}",
                self.account, self.repository
            )));
        }
        let path = self.path.clone();
        let account = self.account.clone();
        let revoked = tokio::task::spawn_blocking(move || {
            let connection = connect(&path)?;
            let revoked: Option<bool> = connection
                .query_row(
                    "SELECT revoked FROM accounts WHERE name = ?1",
                    [account],
                    |row| row.get(0),
                )
                .optional()?;
            Ok::<_, FixtureError>(revoked)
        })
        .await
        .map_err(|error| storage_error(&error))?
        .map_err(|error| storage_error(&error))?;
        match revoked {
            Some(false) => Ok(()),
            Some(true) => Err(unauthorized(format!("account {} is revoked", self.account))),
            None => Err(unauthorized(format!(
                "account {} does not exist",
                self.account
            ))),
        }
    }

    async fn execute(&self, attempt: Attempt, context: CallContext) -> Result<Value, AttemptError> {
        let context = ProbeContext {
            deadline: context.deadline,
            cancellation: context.cancellation,
        };
        self.apply(attempt, context).await
    }

    async fn reconcile(
        &self,
        operation: Operation,
        keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        // Probing reads state and fences keys: only in the scope the operation was approved for.
        if !self.in_scope(&operation.scope) {
            return Err(ProviderError {
                code: "unauthorized".into(),
                message: format!(
                    "this provider acts as {FIXTURE_PROVIDER}/{} on {}, not {}",
                    self.account, self.repository, operation.scope
                ),
            });
        }
        let change = Change::parse(&operation.tool, &operation.arguments)?;
        let path = self.path.clone();
        let probed = match change {
            Change::AddComment { .. } => {
                return Err(ProviderError {
                    code: "no_reconciliation".into(),
                    message: format!("{ADD_COMMENT} declares no reconciliation method"),
                });
            }
            Change::AddLabel { .. } => {
                tokio::task::spawn_blocking(move || reconcile_receipt(&path, &keys)).await
            }
            Change::RemoveLabel { issue, label } => {
                tokio::task::spawn_blocking(move || reconcile_state(&path, issue, &label)).await
            }
        };
        probed
            .map_err(|error| storage_error(&error))?
            .map_err(|error| match error {
                FixtureError::Invalid(message) => ProviderError {
                    code: "not_found".into(),
                    message,
                },
                other => storage_error(&other),
            })
    }
}

/// Applied if any key has a receipt; otherwise fence every key, in one transaction,
/// so none of them can apply later.
fn reconcile_receipt(path: &Path, keys: &[String]) -> Result<Reconciliation, FixtureError> {
    let mut connection = connect(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (index, key) in keys.iter().enumerate() {
        let found = transaction
            .query_row(
                "SELECT 1 FROM receipts WHERE attempt_key = ?1",
                [key],
                |_| Ok(()),
            )
            .optional()?;
        if found.is_some() {
            return Ok(Reconciliation {
                verdict: Verdict::Applied,
                evidence: format!("receipt for attempt {} found", index + 1),
            });
        }
    }
    let now = now_ms();
    for key in keys {
        transaction.execute(
            "INSERT OR IGNORE INTO fences VALUES (?1, ?2)",
            params![key, now],
        )?;
    }
    transaction.commit()?;
    Ok(Reconciliation {
        verdict: Verdict::NotApplied,
        evidence: "no receipt; attempt keys fenced so they cannot apply later".into(),
    })
}

/// Satisfied if the label is absent now; otherwise still unknown.
fn reconcile_state(path: &Path, issue: u64, label: &str) -> Result<Reconciliation, FixtureError> {
    let connection = connect(path)?;
    let known = connection
        .query_row(
            "SELECT 1 FROM issues WHERE number = ?1",
            [issue],
            |_| Ok(()),
        )
        .optional()?;
    if known.is_none() {
        return Err(FixtureError::Invalid(format!(
            "issue {issue} does not exist"
        )));
    }
    let present = connection
        .query_row(
            "SELECT 1 FROM labels WHERE issue = ?1 AND label = ?2",
            params![issue, label],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(if present {
        Reconciliation {
            verdict: Verdict::Unknown,
            evidence: format!("label still present on issue {issue}"),
        }
    } else {
        Reconciliation {
            verdict: Verdict::Satisfied,
            evidence: "label absent now; who removed it is unknown".into(),
        }
    })
}

#[derive(Debug, Clone, Copy)]
enum Actor<'a> {
    /// The provider's account; `in_scope` is false when the operation's scope does not match it.
    Account {
        name: &'a str,
        in_scope: bool,
    },
    External,
}

struct Request<'a> {
    actor: Actor<'a>,
    tool: &'a str,
    key: Option<&'a str>,
    arguments: &'a Value,
    preconditions: &'a [Value],
}

#[derive(Debug, Clone)]
struct Refusal {
    code: &'static str,
    message: String,
}

impl From<Refusal> for ProviderError {
    fn from(refusal: Refusal) -> Self {
        Self {
            code: refusal.code.into(),
            message: refusal.message,
        }
    }
}

fn refusal(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

struct Processed {
    response: Result<Value, Refusal>,
    /// Never return the response: a fault matched.
    pend: bool,
}

enum Decision {
    Refused(Refusal),
    Held,
    Done { effect: bool, value: Value },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    AddLabel { issue: u64, label: String },
    RemoveLabel { issue: u64, label: String },
    AddComment { issue: u64, body: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelArguments {
    issue: u64,
    label: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentArguments {
    issue: u64,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Precondition {
    issue_state: IssueStateName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IssueStateName {
    Open,
    Closed,
}

impl IssueStateName {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl Change {
    fn parse(tool: &str, arguments: &Value) -> Result<Self, Refusal> {
        let invalid = |message: String| refusal("invalid_arguments", message);
        let check_issue = |issue: u64| {
            if issue == 0 || i64::try_from(issue).is_err() {
                Err(invalid("issue must be a positive integer".into()))
            } else {
                Ok(issue)
            }
        };
        match tool {
            ADD_LABEL | REMOVE_LABEL => {
                let LabelArguments { issue, label } = serde_json::from_value(arguments.clone())
                    .map_err(|error| invalid(format!("expected {{issue, label}}: {error}")))?;
                let issue = check_issue(issue)?;
                if label.is_empty() {
                    return Err(invalid("label must not be empty".into()));
                }
                Ok(if tool == ADD_LABEL {
                    Self::AddLabel { issue, label }
                } else {
                    Self::RemoveLabel { issue, label }
                })
            }
            ADD_COMMENT => {
                let CommentArguments { issue, body } = serde_json::from_value(arguments.clone())
                    .map_err(|error| invalid(format!("expected {{issue, body}}: {error}")))?;
                let issue = check_issue(issue)?;
                if body.is_empty() {
                    return Err(invalid("body must not be empty".into()));
                }
                Ok(Self::AddComment { issue, body })
            }
            _ => Err(refusal("unknown_tool", format!("unknown tool '{tool}'"))),
        }
    }

    fn issue(&self) -> u64 {
        match self {
            Self::AddLabel { issue, .. }
            | Self::RemoveLabel { issue, .. }
            | Self::AddComment { issue, .. } => *issue,
        }
    }

    /// The label, or a comment body summary stable for equal bodies.
    fn detail(&self) -> String {
        match self {
            Self::AddLabel { label, .. } | Self::RemoveLabel { label, .. } => label.clone(),
            Self::AddComment { body, .. } => {
                let mut summary: String = body.chars().take(BODY_SUMMARY_CHARS).collect();
                if summary.len() < body.len() {
                    summary.push('…');
                }
                format!("{summary} ({})", Digest::of(body.as_bytes()).short())
            }
        }
    }
}

fn parse_preconditions(preconditions: &[Value]) -> Result<Vec<IssueStateName>, Refusal> {
    preconditions
        .iter()
        .map(|condition| {
            serde_json::from_value::<Precondition>(condition.clone())
                .map(|condition| condition.issue_state)
                .map_err(|_| {
                    refusal(
                        "invalid_arguments",
                        r#"unsupported precondition; supported: {"issue_state": "open" | "closed"}"#,
                    )
                })
        })
        .collect()
}

/// Ledger `request` and apply it if accepted, inside `transaction`.
fn process(transaction: &Transaction<'_>, request: &Request<'_>) -> rusqlite::Result<Processed> {
    let change = Change::parse(request.tool, request.arguments);
    let mut pend = false;
    let decision = decide(transaction, request, &change, &mut pend)?;
    let (outcome, effect, response) = match decision {
        Decision::Refused(refusal) => (format!("refused:{}", refusal.code), false, Err(refusal)),
        Decision::Held => (
            "held".to_string(),
            false,
            Err(refusal("held", "request held by a fault")),
        ),
        Decision::Done { effect, value } => {
            let outcome = if effect { "applied" } else { "no_change" };
            (outcome.to_string(), effect, Ok(value))
        }
    };
    let actor = match request.actor {
        Actor::Account { name, .. } => name,
        Actor::External => EXTERNAL_ACTOR,
    };
    let (issue, detail) = match &change {
        Ok(change) => (Some(change.issue()), Some(change.detail())),
        Err(_) => (None, None),
    };
    transaction.execute(
        "INSERT INTO ledger (unix_ms, actor, tool, attempt_key, issue, detail, outcome, effect)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            now_ms(),
            actor,
            request.tool,
            request.key,
            issue,
            detail,
            outcome,
            effect
        ],
    )?;
    let receipted = matches!(change, Ok(Change::AddLabel { .. })) && response.is_ok();
    if let (true, Some(key)) = (receipted, request.key) {
        transaction.execute(
            "INSERT OR IGNORE INTO receipts VALUES (?1, ?2)",
            params![key, transaction.last_insert_rowid()],
        )?;
    }
    Ok(Processed { response, pend })
}

fn decide(
    transaction: &Transaction<'_>,
    request: &Request<'_>,
    change: &Result<Change, Refusal>,
    pend: &mut bool,
) -> rusqlite::Result<Decision> {
    if let Actor::Account { name, in_scope } = request.actor {
        let revoked: Option<bool> = transaction
            .query_row(
                "SELECT revoked FROM accounts WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if !in_scope || revoked != Some(false) {
            return Ok(Decision::Refused(refusal(
                "permission_denied",
                format!("account {name} may not act on this repository"),
            )));
        }
    }
    let change = match change {
        Ok(change) => change,
        Err(refusal) => return Ok(Decision::Refused(refusal.clone())),
    };
    let preconditions = match parse_preconditions(request.preconditions) {
        Ok(preconditions) => preconditions,
        Err(refusal) => return Ok(Decision::Refused(refusal)),
    };
    if let Some(key) = request.key {
        let fenced = transaction
            .query_row("SELECT 1 FROM fences WHERE attempt_key = ?1", [key], |_| {
                Ok(())
            })
            .optional()?;
        if fenced.is_some() {
            return Ok(Decision::Refused(refusal(
                "attempt_fenced",
                "reconciliation declared this attempt not applied; it can never apply",
            )));
        }
    }
    let issue = change.issue();
    if matches!(request.actor, Actor::Account { .. }) {
        let fault: Option<(i64, String)> = transaction
            .query_row(
                "SELECT id, mode FROM faults WHERE tool = ?1 AND issue = ?2 AND remaining > 0
                 ORDER BY id LIMIT 1",
                params![request.tool, issue],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, mode)) = fault {
            transaction.execute(
                "UPDATE faults SET remaining = remaining - 1 WHERE id = ?1",
                [id],
            )?;
            *pend = true;
            if mode == FaultMode::Hold.as_str() {
                return Ok(Decision::Held);
            }
        }
    }
    let state: Option<String> = transaction
        .query_row(
            "SELECT state FROM issues WHERE number = ?1",
            [issue],
            |row| row.get(0),
        )
        .optional()?;
    let Some(state) = state else {
        return Ok(Decision::Refused(refusal(
            "not_found",
            format!("issue {issue} does not exist"),
        )));
    };
    if let Some(wanted) = preconditions.iter().find(|wanted| wanted.as_str() != state) {
        return Ok(Decision::Refused(refusal(
            "precondition_failed",
            format!("issue {issue} is {state}, not {}", wanted.as_str()),
        )));
    }
    Ok(match change {
        Change::AddLabel { label, .. } => {
            let effect = transaction.execute(
                "INSERT OR IGNORE INTO labels VALUES (?1, ?2)",
                params![issue, label],
            )? == 1;
            Decision::Done {
                effect,
                value: json!({"issue": issue, "label": label, "changed": effect}),
            }
        }
        Change::RemoveLabel { label, .. } => {
            let effect = transaction.execute(
                "DELETE FROM labels WHERE issue = ?1 AND label = ?2",
                params![issue, label],
            )? == 1;
            Decision::Done {
                effect,
                value: json!({"issue": issue, "label": label, "changed": effect}),
            }
        }
        Change::AddComment { body, .. } => {
            transaction.execute(
                "INSERT INTO comments (issue, body) VALUES (?1, ?2)",
                params![issue, body],
            )?;
            Decision::Done {
                effect: true,
                value: json!({"issue": issue, "comment": transaction.last_insert_rowid()}),
            }
        }
    })
}

/// Open an existing file for reading and writing with the fixture's settings.
fn connect(path: &Path) -> Result<Connection, FixtureError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
    Ok(connection)
}

/// Connect and check that `path` is a fixture of this version.
fn open_checked(path: &Path) -> Result<Connection, FixtureError> {
    if !path.exists() {
        return Err(FixtureError::NotFound(path.to_path_buf()));
    }
    let incompatible = |detail: String| FixtureError::Incompatible {
        path: path.to_path_buf(),
        detail,
    };
    let connection = connect(path)?;
    let format = meta(&connection, "format")
        .map_err(|error| incompatible(format!("no fixture metadata ({error})")))?;
    if format.as_deref() != Some(FORMAT) {
        return Err(incompatible("not a codemode fixture".into()));
    }
    let version = meta(&connection, "format_version")?.unwrap_or_default();
    if version != FIXTURE_FORMAT_VERSION.to_string() {
        return Err(incompatible(format!(
            "fixture format version {version}; this build reads version {FIXTURE_FORMAT_VERSION}"
        )));
    }
    Ok(connection)
}

fn meta(connection: &Connection, key: &str) -> Result<Option<String>, FixtureError> {
    Ok(connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn read_latency(connection: &Connection) -> Result<u64, FixtureError> {
    Ok(meta(connection, "latency_ms")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

fn validate_seed(seed: &Seed) -> Result<(), FixtureError> {
    let invalid = |message: String| Err(FixtureError::Invalid(message));
    if seed.repository.is_empty() {
        return invalid("a fixture needs a repository name".into());
    }
    if seed.accounts.is_empty() || seed.accounts.iter().any(String::is_empty) {
        return invalid("a fixture needs at least one nonempty account name".into());
    }
    if seed
        .accounts
        .iter()
        .any(|account| account == EXTERNAL_ACTOR)
    {
        return invalid(format!("'{EXTERNAL_ACTOR}' is reserved for outside edits"));
    }
    if seed.issues.is_empty() {
        return invalid("a fixture needs at least one issue".into());
    }
    if let Some(issue) = seed
        .issues
        .iter()
        .find(|issue| **issue == 0 || i64::try_from(**issue).is_err())
    {
        return invalid(format!("issue {issue} is not a positive integer"));
    }
    let listed = |issue: &u64| seed.issues.contains(issue);
    if let Some(issue) = seed.closed.iter().find(|issue| !listed(issue)) {
        return invalid(format!("closed issue {issue} is not among the issues"));
    }
    for (issue, label) in &seed.labels {
        if !listed(issue) || label.is_empty() {
            return invalid(format!(
                "label '{label}' needs a listed issue and a nonempty name (issue {issue})"
            ));
        }
    }
    Ok(())
}

fn storage_error(error: &dyn fmt::Display) -> ProviderError {
    ProviderError {
        code: "storage_error".into(),
        message: error.to_string(),
    }
}

fn storage_failure(error: impl fmt::Display, effect: FailureEffect) -> AttemptError {
    AttemptError {
        error: storage_error(&error),
        effect,
    }
}

fn not_applied(code: &str, message: &str) -> AttemptError {
    AttemptError {
        error: ProviderError {
            code: code.into(),
            message: message.into(),
        },
        effect: FailureEffect::NotApplied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RunCancellation;
    use tokio::time::Instant;

    const ACCOUNT: &str = "triage-bot";

    /// A fixture file removed when dropped.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.0.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn fixture(name: &str) -> (Scratch, FixtureProvider) {
        let path = std::env::temp_dir().join(format!(
            "codemode-fixture-{name}-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        let seed = Seed {
            repository: "acme/widgets".into(),
            accounts: vec![ACCOUNT.into()],
            issues: vec![101, 102, 103],
            closed: vec![103],
            labels: vec![(102, "stale".into())],
        };
        drop(Fixture::create(&path, &seed).expect("create"));
        let provider = FixtureProvider::open(&path, ACCOUNT).expect("provider");
        (Scratch(path), provider)
    }

    fn attempt(key: &str, tool: &str, arguments: Value, preconditions: Vec<Value>) -> Attempt {
        Attempt {
            operation: Operation {
                id: "op".into(),
                tool: tool.into(),
                scope: Scope {
                    provider: FIXTURE_PROVIDER.into(),
                    account: ACCOUNT.into(),
                    destination: Some("acme/widgets".into()),
                },
                arguments,
                preconditions,
            },
            key: key.into(),
            number: 1,
        }
    }

    fn label(key: &str, tool: &str, issue: u64, name: &str) -> Attempt {
        attempt(
            key,
            tool,
            json!({"issue": issue, "label": name}),
            Vec::new(),
        )
    }

    fn context() -> ProbeContext {
        ProbeContext {
            deadline: Instant::now() + Duration::from_secs(10),
            cancellation: RunCancellation::default(),
        }
    }

    fn ledger(scratch: &Scratch) -> Ledger {
        Fixture::open(&scratch.0)
            .expect("open")
            .ledger()
            .expect("ledger")
    }

    fn labels(scratch: &Scratch, issue: u64) -> Vec<String> {
        let state = Fixture::open(&scratch.0)
            .expect("open")
            .state()
            .expect("state");
        state
            .issues
            .into_iter()
            .find(|state| state.number == issue)
            .expect("issue")
            .labels
    }

    #[tokio::test]
    async fn the_ledger_separates_repeated_requests_from_repeated_effects() {
        let (scratch, provider) = fixture("repeat");
        for key in ["k1", "k2"] {
            provider
                .apply(label(key, ADD_LABEL, 101, "triage"), context())
                .await
                .expect("add label");
        }
        for key in ["c1", "c2"] {
            let comment = json!({"issue": 101, "body": "Looking into it"});
            provider
                .apply(attempt(key, ADD_COMMENT, comment, Vec::new()), context())
                .await
                .expect("add comment");
        }
        let ledger = ledger(&scratch);
        let effects: Vec<_> = ledger.rows.iter().map(|row| row.effect).collect();
        assert_eq!(effects, [true, false, true, true]);
        let summary: Vec<_> = ledger
            .changes
            .iter()
            .map(|change| {
                (
                    change.tool.as_str(),
                    change.requests,
                    change.effects,
                    change.duplicates,
                )
            })
            .collect();
        assert_eq!(summary, [(ADD_LABEL, 2, 1, 0), (ADD_COMMENT, 2, 2, 1)]);
    }

    #[tokio::test]
    async fn receipts_prove_application_and_fences_stop_late_attempts() {
        let (scratch, provider) = fixture("receipt");
        provider
            .apply(label("k1", ADD_LABEL, 101, "triage"), context())
            .await
            .expect("add label");
        let applied = provider
            .reconcile(
                label("k0", ADD_LABEL, 101, "triage").operation,
                vec!["k0".into(), "k1".into()],
                context(),
            )
            .await
            .expect("probe");
        assert_eq!(applied.verdict, Verdict::Applied);
        assert_eq!(applied.evidence, "receipt for attempt 2 found");

        let never_sent = label("k2", ADD_LABEL, 101, "bug");
        let fenced = provider
            .reconcile(never_sent.operation.clone(), vec!["k2".into()], context())
            .await
            .expect("probe");
        assert_eq!(fenced.verdict, Verdict::NotApplied);
        let late = provider
            .apply(never_sent, context())
            .await
            .expect_err("fenced");
        assert_eq!(
            (late.error.code.as_str(), late.effect),
            ("attempt_fenced", FailureEffect::Refused)
        );
        assert_eq!(labels(&scratch, 101), ["triage"]);
        let row = ledger(&scratch).rows.pop().expect("row");
        assert_eq!(
            (row.outcome.as_str(), row.effect),
            ("refused:attempt_fenced", false)
        );
    }

    #[tokio::test]
    async fn a_lost_response_commits_the_effect_once() {
        let (scratch, provider) = fixture("lost");
        Fixture::open(&scratch.0)
            .expect("open")
            .add_fault(ADD_LABEL, 101, FaultMode::LoseResponse, 1)
            .expect("fault");
        let lost = tokio::time::timeout(
            Duration::from_millis(300),
            provider.apply(label("k1", ADD_LABEL, 101, "triage"), context()),
        )
        .await;
        assert!(lost.is_err(), "a lost response never returns: {lost:?}");
        assert_eq!(labels(&scratch, 101), ["triage"]);

        let second = tokio::time::timeout(
            Duration::from_secs(5),
            provider.apply(label("k2", ADD_LABEL, 101, "triage"), context()),
        )
        .await
        .expect("the fault was consumed")
        .expect("no change");
        assert_eq!(second["changed"], false);
        let outcomes: Vec<_> = ledger(&scratch)
            .rows
            .into_iter()
            .map(|row| (row.outcome, row.effect))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("applied".to_string(), true),
                ("no_change".to_string(), false)
            ]
        );
    }

    #[tokio::test]
    async fn refusals_are_ledgered_without_effect() {
        let (scratch, provider) = fixture("refused");
        let closed_only = vec![json!({"issue_state": "closed"})];
        let failed = provider
            .apply(
                attempt(
                    "k1",
                    ADD_LABEL,
                    json!({"issue": 101, "label": "triage"}),
                    closed_only,
                ),
                context(),
            )
            .await
            .expect_err("precondition");
        assert_eq!(
            (failed.error.code.as_str(), failed.effect),
            ("precondition_failed", FailureEffect::Refused)
        );

        Fixture::open(&scratch.0)
            .expect("open")
            .set_revoked(ACCOUNT, true)
            .expect("revoke");
        let denied = provider
            .apply(label("k2", ADD_LABEL, 101, "triage"), context())
            .await
            .expect_err("revoked");
        assert_eq!(
            (denied.error.code.as_str(), denied.effect),
            ("permission_denied", FailureEffect::Refused)
        );
        let scope = label("k3", ADD_LABEL, 101, "triage").operation.scope;
        let unauthorized = provider
            .authorize(&scope, context())
            .await
            .expect_err("revoked");
        assert_eq!(unauthorized.code, "unauthorized");

        assert!(labels(&scratch, 101).is_empty());
        let outcomes: Vec<_> = ledger(&scratch)
            .rows
            .into_iter()
            .map(|row| (row.outcome, row.effect))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("refused:precondition_failed".to_string(), false),
                ("refused:permission_denied".to_string(), false)
            ]
        );
    }

    #[tokio::test]
    async fn remove_label_reconciles_from_current_state() {
        let (scratch, provider) = fixture("state");
        let remove = label("k1", REMOVE_LABEL, 102, "stale").operation;
        let present = provider
            .reconcile(remove.clone(), vec!["k1".into()], context())
            .await
            .expect("probe");
        assert_eq!(present.verdict, Verdict::Unknown);

        Fixture::open(&scratch.0)
            .expect("open")
            .external_label(102, "stale", LabelEdit::Remove)
            .expect("external edit");
        let absent = provider
            .reconcile(remove, vec!["k1".into()], context())
            .await
            .expect("probe");
        assert_eq!(absent.verdict, Verdict::Satisfied);
        assert_eq!(ledger(&scratch).rows[0].actor, EXTERNAL_ACTOR);
    }
}
