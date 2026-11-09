//! Offline evaluation of recovery after a killed executor, and of what durable
//! accounting costs when nothing fails.
//!
//! **A. Recovery.** Each repetition and latency profile creates one crashed state
//! with a real process kill: `codemode plan run` executes the 11-operation plan of
//! `examples/recovery/plan.json` against a fixture with three lost responses and
//! one held request, and is sent SIGKILL once the fixture ledger shows the held
//! request. The journal and fixture files are then copied three times, and each
//! strategy recovers its own copy from the identical state:
//!
//! - `stop`: read the journal and stop if any operation is unknown;
//! - `blind_retry`: re-send every operation not observed applied through the same
//!   runtime, with fresh attempt keys, ignoring contracts, without reconciliation
//!   or journal writes;
//! - `codemode_resume`: [`run_session`] with [`SessionKind::Resume`].
//!
//! **B. Persistence overhead.** The same plan (and a 64-operation label plan) runs
//! without faults three ways: the runtime alone, and [`run_session`] with
//! [`Durability::Storage`] and [`Durability::Process`].
//!
//! Strategies and modes run round-robin within a repetition; the starting one
//! rotates every repetition. Percentiles use the nearest-rank method. Every run is
//! checked against the fixture's state and ledger. The process exits 1 if a check
//! that should hold fails (the crashed state, codemode's recovery, a stop that sent
//! requests, or any no-fault run); blind retry's duplicates are recorded, not failed.
//! No speedup is assumed or claimed.
//!
//! ```text
//! cargo build --release --locked --bin codemode
//! cargo run --release --locked --bin codemode-recovery-eval -- \
//!     [--reps N] [--warmup N] [--latency 0,25] [--large N] [--out DIR] [--deno PATH] [--bin PATH]
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use codemode::fixture::{FaultMode, Fixture, FixtureProvider, Ledger, Seed, State};
use codemode::{
    Approval, Attempt, CallContext, CallOutcome, Conclusion, Digest, DispatchPolicy, Durability,
    ExecutionBackend, FrozenPlan, Journal, NativeCall, NextStep, Operation, Plan, ProbeContext,
    ProviderError, RecoverableProvider, RecoveryPolicy, RunCancellation, RunConfig, RunReport,
    SessionKind, SessionOptions, ToolCall, ToolProvider, approval_binding, assess_history,
    run_calls, run_session,
};
use serde::Serialize;
use serde_json::{Value, json};

const ACCOUNT: &str = "triage-bot";
const REPOSITORY: &str = "acme/widgets";
/// Operations in flight at once, for every strategy and mode.
const CONCURRENCY: usize = 4;
/// Deadline of every runtime-only dispatch run; matches the default session deadline.
const RUN_TIMEOUT_MS: u64 = 60_000;
/// Longest wait for the killed run to reach the held request.
const CRASH_TIMEOUT: Duration = Duration::from_secs(30);
const RECOVERY_PLAN: &str = include_str!("../../examples/recovery/plan.json");
const RECOVERY_WORKLOAD: &str = "recovery-plan";
const LARGE_WORKLOAD: &str = "labels";
const LARGE_FIRST_ISSUE: u64 = 201;

/// Faults armed before the killed run: three lost responses after the effect
/// commits, and one request held before any effect.
const FAULTS: [(&str, u64, FaultMode); 4] = [
    ("issues.add_label", 104, FaultMode::LoseResponse),
    ("issues.remove_label", 107, FaultMode::LoseResponse),
    ("issues.add_comment", 105, FaultMode::LoseResponse),
    ("issues.add_label", 109, FaultMode::Hold),
];
/// What the killed run's journal must show.
const CRASH_APPLIED: [&str; 6] = [
    "label-101",
    "label-102",
    "label-103",
    "label-105",
    "label-106",
    "label-108",
];
const CRASH_UNKNOWN: [&str; 4] = ["label-104", "label-107", "comment-105", "label-109"];
const CRASH_PENDING: [&str; 1] = ["label-110"];
/// Ledger rows at the crash: nine applied requests and the held one.
const CRASH_LEDGER_ROWS: usize = 10;
/// The one operation codemode must leave for a person.
const REVIEW_OPERATION: &str = "comment-105";

const HELP: &str = "codemode-recovery-eval — compare recovery strategies after a killed run, and measure journal overhead

Usage:
  codemode-recovery-eval [--reps N] [--warmup N] [--latency LIST] [--large N]
                         [--out DIR] [--deno PATH] [--bin PATH]

Options:
  --reps <n>         Measured repetitions (default: 15)
  --warmup <n>       Discarded repetitions before measuring (default: 2)
  --latency <list>   Fixture latency profiles in ms, comma-separated (default: 0,25)
  --large <n>        Operations in the extra label-only overhead plan; 0 skips it (default: 64)
  --out <dir>        Also write summary.json
  --deno <path>      Legacy option, accepted but unused by native execution
  --bin <path>       codemode executable that is killed mid-run
                     (default: codemode next to this executable)
  -h, --help         Show this help

Prints a Markdown summary to stdout and progress to stderr. Exits 1 if any
correctness check that should hold fails and 2 on a usage, setup, or output error.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    Stop,
    BlindRetry,
    CodemodeResume,
}

const STRATEGIES: [Strategy; 3] = [
    Strategy::Stop,
    Strategy::BlindRetry,
    Strategy::CodemodeResume,
];

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop on uncertainty",
            Self::BlindRetry => "blind retry",
            Self::CodemodeResume => "codemode resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Runtime,
    Storage,
    Process,
}

const MODES: [Mode; 3] = [Mode::Runtime, Mode::Storage, Mode::Process];

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Runtime => "runtime only",
            Self::Storage => "journal, storage",
            Self::Process => "journal, process",
        }
    }

    fn durability(self) -> Option<Durability> {
        match self {
            Self::Runtime => None,
            Self::Storage => Some(Durability::Storage),
            Self::Process => Some(Durability::Process),
        }
    }
}

/// Order for one repetition: round-robin, starting one later each time.
fn rotation<T: Copy, const N: usize>(items: [T; N], round: usize) -> impl Iterator<Item = T> {
    (0..N).map(move |offset| items[(round + offset) % N])
}

struct Workload {
    name: String,
    plan: FrozenPlan,
}

fn large_plan(operations: usize) -> Result<FrozenPlan, Box<dyn Error>> {
    let operations: Vec<Value> = (0..operations as u64)
        .map(|index| {
            let issue = LARGE_FIRST_ISSUE + index;
            json!({
                "id": format!("label-{issue}"),
                "tool": "issues.add_label",
                "scope": {"provider": "fixture.issues", "account": ACCOUNT, "destination": REPOSITORY},
                "arguments": {"issue": issue, "label": "triage"},
            })
        })
        .collect();
    let text = json!({"version": 1, "summary": "Label many issues", "operations": operations});
    Ok(Plan::parse(&text.to_string())?)
}

fn issue_of(operation: &Operation) -> u64 {
    operation.arguments["issue"].as_u64().unwrap_or(0)
}

/// Every issue the plan names, open; labels the plan removes start present.
fn seed_for(plan: &FrozenPlan) -> Seed {
    let mut issues = BTreeSet::new();
    let mut labels = Vec::new();
    for operation in &plan.plan().operations {
        let issue = issue_of(operation);
        issues.insert(issue);
        if operation.tool == "issues.remove_label" {
            let label = operation.arguments["label"].as_str().unwrap_or_default();
            labels.push((issue, label.to_string()));
        }
    }
    Seed {
        repository: REPOSITORY.into(),
        accounts: vec![ACCOUNT.into()],
        issues: issues.into_iter().collect(),
        closed: Vec::new(),
        labels,
    }
}

/// Create the fixture (with `latency_ms`) and, for a durability, an approved journal.
fn prepare(
    dir: &Path,
    plan: &FrozenPlan,
    latency_ms: u64,
    durability: Option<Durability>,
) -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::create(dir.join("fx.db"), &seed_for(plan))?;
    fixture.set_latency(latency_ms)?;
    drop(fixture);
    if let Some(durability) = durability {
        let provider = FixtureProvider::open(dir.join("fx.db"), ACCOUNT)?;
        let mut journal = Journal::create(dir.join("j.db"), plan, durability)?;
        let policy = RecoveryPolicy {
            concurrency: CONCURRENCY,
            ..RecoveryPolicy::default()
        };
        let declaration = provider.declaration().for_plan(plan);
        journal.approve(&Approval {
            binding: approval_binding(plan.digest(), &declaration, &policy),
            plan: plan.digest().clone(),
            declaration,
            policy,
            approver: "recovery-evaluation".into(),
            note: None,
            approved_unix_ms: 0,
        })?;
    }
    Ok(())
}

fn read_fixture(dir: &Path) -> Result<(State, Ledger), Box<dyn Error>> {
    let fixture = Fixture::open(dir.join("fx.db"))?;
    Ok((fixture.state()?, fixture.ledger()?))
}

/// The expected final state: every label added or removed as planned, exactly one
/// comment per comment operation and no other comments, and no duplicate effects.
fn check_final(plan: &FrozenPlan, state: &State, ledger: &Ledger) -> Vec<String> {
    let mut failures = Vec::new();
    let mut expected_comments = 0;
    for operation in &plan.plan().operations {
        let number = issue_of(operation);
        let Some(issue) = state.issues.iter().find(|issue| issue.number == number) else {
            failures.push(format!("{}: issue #{number} is missing", operation.id));
            continue;
        };
        let text = |key: &str| operation.arguments[key].as_str().unwrap_or_default();
        let has_label = issue.labels.iter().any(|label| label == text("label"));
        match operation.tool.as_str() {
            "issues.add_label" if !has_label => {
                failures.push(format!("{}: label absent on #{number}", operation.id));
            }
            "issues.remove_label" if has_label => {
                failures.push(format!("{}: label still on #{number}", operation.id));
            }
            "issues.add_comment" => {
                expected_comments += 1;
                let matching = issue
                    .comments
                    .iter()
                    .filter(|comment| comment.body == text("body"))
                    .count();
                if matching != 1 {
                    failures.push(format!(
                        "{}: {matching} matching comments on #{number}, expected 1",
                        operation.id
                    ));
                }
            }
            _ => {}
        }
    }
    let comments: usize = state.issues.iter().map(|issue| issue.comments.len()).sum();
    if comments != expected_comments {
        failures.push(format!(
            "{comments} comments in total, expected {expected_comments}"
        ));
    }
    let (duplicates, _) = ledger_counts(ledger);
    if duplicates > 0 {
        failures.push(format!("{duplicates} duplicate effect(s) in the ledger"));
    }
    failures
}

/// Duplicate effects, and repeated requests that added no effect, over the whole ledger.
fn ledger_counts(ledger: &Ledger) -> (u64, u64) {
    let duplicates: u64 = ledger.changes.iter().map(|change| change.duplicates).sum();
    let repeats: u64 = ledger
        .changes
        .iter()
        .map(|change| change.requests.saturating_sub(1))
        .sum();
    (duplicates, repeats.saturating_sub(duplicates))
}

/// Sends each named operation to the fixture with a fresh attempt key and no
/// journal: no intent, no observation, no reconciliation.
struct PlainProvider {
    fixture: FixtureProvider,
    operations: HashMap<String, Operation>,
    key_prefix: String,
    number: u32,
}

impl ToolProvider for PlainProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let operation = call
            .arguments
            .get("operation")
            .and_then(Value::as_str)
            .and_then(|id| self.operations.get(id))
            .filter(|operation| operation.tool == call.tool)
            .cloned()
            .ok_or_else(|| ProviderError {
                code: "plan_violation".into(),
                message: "the call names no operation of this run".into(),
            })?;
        let attempt = Attempt {
            key: format!("{}-{}", self.key_prefix, operation.id),
            number: self.number,
            operation,
        };
        let context = ProbeContext {
            deadline: context.deadline,
            cancellation: context.cancellation,
        };
        self.fixture
            .apply(attempt, context)
            .await
            .map_err(|error| error.error)
    }
}

async fn runtime_run(
    operations: Vec<Operation>,
    fixture: &Path,
    key_prefix: String,
    number: u32,
    deno: &Path,
) -> Result<RunReport, Box<dyn Error>> {
    let calls: Vec<NativeCall> = operations
        .iter()
        .map(|operation| NativeCall {
            tool: operation.tool.clone(),
            arguments: json!({"operation": operation.id}),
        })
        .collect();
    let tools: BTreeSet<String> = operations.iter().map(|op| op.tool.clone()).collect();
    let config = RunConfig {
        timeout_ms: RUN_TIMEOUT_MS,
        max_calls: operations.len(),
        max_concurrency: CONCURRENCY,
        deno: deno.to_owned(),
        allowed_tools: tools.into_iter().collect(),
        dispatch: DispatchPolicy::Rolling,
        ..RunConfig::default()
    };
    let provider = PlainProvider {
        fixture: FixtureProvider::open(fixture, ACCOUNT)?,
        operations: operations
            .into_iter()
            .map(|operation| (operation.id.clone(), operation))
            .collect(),
        key_prefix,
        number,
    };
    Ok(run_calls(&calls, config, provider, RunCancellation::default()).await?)
}

/// Native execution has no guest startup. Execution covers the entire dispatch
/// run, including admission, scheduling, and provider work, in whole milliseconds.
fn dispatch_timing(report: &RunReport) -> (Option<u64>, Option<u64>) {
    (None, Some(report.elapsed_ms))
}

fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Run `codemode plan run` until the held request reaches the fixture, then kill it.
async fn crash(
    dir: &Path,
    plan: &FrozenPlan,
    latency_ms: u64,
    args: &Args,
) -> Result<(), Box<dyn Error>> {
    prepare(dir, plan, latency_ms, Some(Durability::Storage))?;
    let fixture_path = dir.join("fx.db");
    let fixture = Fixture::open(&fixture_path)?;
    for (tool, issue, mode) in FAULTS {
        fixture.add_fault(tool, issue, mode, 1)?;
    }
    drop(fixture);
    let mut child = Command::new(&args.bin)
        .args(["plan", "run", "--journal"])
        .arg(dir.join("j.db"))
        .arg("--fixture")
        .arg(&fixture_path)
        .args(["--account", ACCOUNT, "--deno"])
        .arg(&args.deno)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", args.bin.display()))?;
    let started = Instant::now();
    let waited: Result<(), String> = loop {
        if let Some(status) = child.try_wait()? {
            break Err(format!(
                "codemode plan run exited ({status}) before the held request"
            ));
        }
        let ledger = Fixture::open(&fixture_path)?.ledger()?;
        if ledger.rows.iter().any(|row| row.outcome == "held") {
            break Ok(());
        }
        if started.elapsed() > CRASH_TIMEOUT {
            break Err("the held request did not arrive in time".into());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    // `Child::kill` sends SIGKILL on Unix: the executor gets no chance to record anything.
    let _ = child.kill();
    child.wait()?;
    Ok(waited?)
}

/// Copy each database and its write-ahead log; no process holds them now.
fn copy_state(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for name in ["fx.db", "j.db"] {
        for suffix in ["", "-wal"] {
            let file = format!("{name}{suffix}");
            let source = from.join(&file);
            if source.exists() {
                std::fs::copy(&source, to.join(&file))?;
            }
        }
    }
    Ok(())
}

/// Check, on the original files after copying, that the kill left the expected state.
fn verify_crash(dir: &Path, plan: &FrozenPlan) -> Result<Vec<String>, Box<dyn Error>> {
    let mut failures = Vec::new();
    let journal = Journal::open(dir.join("j.db"), Durability::Process)?;
    let statuses = assess_history(plan, &journal.history()?, &HashSet::new());
    drop(journal);
    let with = |conclusion: Conclusion| -> BTreeSet<&str> {
        statuses
            .iter()
            .filter(|status| status.assessment.conclusion == conclusion)
            .map(|status| status.operation.as_str())
            .collect()
    };
    for (conclusion, expected) in [
        (Conclusion::Applied, &CRASH_APPLIED[..]),
        (Conclusion::Unknown, &CRASH_UNKNOWN[..]),
        (Conclusion::Pending, &CRASH_PENDING[..]),
    ] {
        let found = with(conclusion);
        if found != expected.iter().copied().collect() {
            failures.push(format!(
                "crash left {} operations {found:?}, expected {expected:?}",
                conclusion.as_str()
            ));
        }
    }
    let (_, ledger) = read_fixture(dir)?;
    let held = ledger
        .rows
        .iter()
        .filter(|row| row.outcome == "held")
        .count();
    if ledger.rows.len() != CRASH_LEDGER_ROWS || held != 1 {
        failures.push(format!(
            "crash ledger has {} rows ({held} held), expected {CRASH_LEDGER_ROWS} (1 held)",
            ledger.rows.len()
        ));
    }
    Ok(failures)
}

#[derive(Debug, Serialize)]
struct RecoveryRun {
    latency_ms: u64,
    strategy: Strategy,
    /// Measured repetition index, starting at 0 after warmup.
    repetition: usize,
    /// Position of this strategy within its repetition's rotation.
    order: usize,
    /// Observed dispatch backend; absent for stop, which dispatches no work.
    backend: Option<ExecutionBackend>,
    guest_transport_bytes: Option<u64>,
    /// Strategy start to done, wall clock.
    recovery_us: i64,
    /// Journal open and read (all strategies), µs.
    journal_us: i64,
    /// Operations the strategy leaves unresolved by its own knowledge: unknown or
    /// pending (stop); re-sent while an earlier attempt was unknown, or not
    /// observed succeeding (blind retry); next step other than done (codemode).
    unresolved: usize,
    /// Whether the strategy considers the plan finished.
    claims_complete: bool,
    /// Ledger rows added by this strategy.
    requests_sent: usize,
    /// Effects beyond the first for one logical change, over the whole ledger.
    duplicate_effects: u64,
    /// Requests beyond the first for one logical change that added no effect.
    repeated_without_effect: u64,
    state_correct: bool,
    state_failures: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatched: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checks_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconcile_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_startup_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_ms: Option<u64>,
    /// Checks that should hold and failed.
    failures: Vec<String>,
}

impl RecoveryRun {
    fn new(latency_ms: u64, strategy: Strategy, repetition: usize, order: usize) -> Self {
        Self {
            latency_ms,
            strategy,
            repetition,
            order,
            backend: None,
            guest_transport_bytes: None,
            recovery_us: 0,
            journal_us: 0,
            unresolved: 0,
            claims_complete: false,
            requests_sent: 0,
            duplicate_effects: 0,
            repeated_without_effect: 0,
            state_correct: false,
            state_failures: Vec::new(),
            session_status: None,
            probed: None,
            dispatched: None,
            checks_ms: None,
            reconcile_ms: None,
            dispatch_ms: None,
            guest_startup_ms: None,
            execution_ms: None,
            failures: Vec::new(),
        }
    }
}

async fn recover(
    strategy: Strategy,
    dir: &Path,
    plan: &FrozenPlan,
    record: &mut RecoveryRun,
    args: &Args,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    match strategy {
        // Stop and blind retry only read the journal, so they open it with process
        // durability: closing it then checkpoints without a full storage flush.
        Strategy::Stop => {
            let journal = Journal::open(dir.join("j.db"), Durability::Process)?;
            let statuses = assess_history(plan, &journal.history()?, &HashSet::new());
            drop(journal);
            record.journal_us = micros(started.elapsed());
            record.unresolved = statuses
                .iter()
                .filter(|status| {
                    matches!(
                        status.assessment.conclusion,
                        Conclusion::Unknown | Conclusion::Pending
                    )
                })
                .count();
            record.recovery_us = micros(started.elapsed());
        }
        Strategy::BlindRetry => {
            let journal = Journal::open(dir.join("j.db"), Durability::Process)?;
            let statuses = assess_history(plan, &journal.history()?, &HashSet::new());
            drop(journal);
            record.journal_us = micros(started.elapsed());
            let mut was_unknown = HashSet::new();
            let mut targets = Vec::new();
            for status in &statuses {
                let conclusion = status.assessment.conclusion;
                if matches!(conclusion, Conclusion::Unknown | Conclusion::Pending) {
                    if conclusion == Conclusion::Unknown {
                        was_unknown.insert(status.operation.clone());
                    }
                    targets.extend(plan.operation(&status.operation).cloned());
                }
            }
            let report = runtime_run(
                targets.clone(),
                &dir.join("fx.db"),
                format!("blind-{}", record.repetition),
                2,
                &args.deno,
            )
            .await?;
            record.recovery_us = micros(started.elapsed());
            let succeeded: HashSet<&str> = report
                .calls
                .iter()
                .filter(|call| matches!(call.outcome, CallOutcome::Succeeded { .. }))
                .filter_map(|call| call.call.arguments["operation"].as_str())
                .collect();
            record.unresolved = targets
                .iter()
                .filter(|op| was_unknown.contains(&op.id) || !succeeded.contains(op.id.as_str()))
                .count();
            record.claims_complete = report.status.is_success() && succeeded.len() == targets.len();
            record.dispatched = Some(targets.len());
            record.dispatch_ms = Some(report.elapsed_ms);
            record.backend = Some(report.backend);
            record.guest_transport_bytes = Some(report.bytes.from_guest + report.bytes.to_guest);
            (record.guest_startup_ms, record.execution_ms) = dispatch_timing(&report);
        }
        Strategy::CodemodeResume => {
            let journal = Journal::open(dir.join("j.db"), Durability::Storage)?;
            record.journal_us = micros(started.elapsed());
            let provider = FixtureProvider::open(dir.join("fx.db"), ACCOUNT)?;
            let options = SessionOptions {
                kind: SessionKind::Resume,
                deno: args.deno.clone(),
            };
            let outcome =
                run_session(journal, provider, options, RunCancellation::default()).await?;
            record.recovery_us = micros(started.elapsed());
            let open: Vec<&str> = outcome
                .operations
                .iter()
                .filter(|status| status.assessment.next != NextStep::Done)
                .map(|status| status.operation.as_str())
                .collect();
            record.unresolved = open.len();
            record.claims_complete = outcome.status == "completed";
            record.probed = Some(outcome.probed);
            record.dispatched = Some(outcome.dispatched);
            record.checks_ms = Some(outcome.timing.checks_ms);
            record.reconcile_ms = Some(outcome.timing.reconcile_ms);
            record.dispatch_ms = Some(outcome.timing.dispatch_ms);
            if let Some(trace) = &outcome.trace {
                record.backend = Some(trace.backend);
                record.guest_transport_bytes = Some(trace.bytes.from_guest + trace.bytes.to_guest);
                (record.guest_startup_ms, record.execution_ms) = dispatch_timing(trace);
            }
            if outcome.status != "needs_review" || open != [REVIEW_OPERATION] {
                record.failures.push(format!(
                    "codemode ended {} with {open:?} open, expected needs_review with [{REVIEW_OPERATION}]",
                    outcome.status
                ));
            }
            record.session_status = Some(outcome.status);
        }
    }

    let (state, ledger) = read_fixture(dir)?;
    record.state_failures = check_final(plan, &state, &ledger);
    record.state_correct = record.state_failures.is_empty();
    record.requests_sent = ledger.rows.len().saturating_sub(CRASH_LEDGER_ROWS);
    (record.duplicate_effects, record.repeated_without_effect) = ledger_counts(&ledger);
    match strategy {
        Strategy::CodemodeResume if !record.state_correct => {
            let detail = record.state_failures.join("; ");
            record.failures.push(format!("final state: {detail}"));
        }
        Strategy::Stop if record.requests_sent > 0 => {
            record.failures.push(format!(
                "stop sent {} request(s); it must send none",
                record.requests_sent
            ));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct OverheadRun {
    workload: String,
    operations: usize,
    latency_ms: u64,
    mode: Mode,
    repetition: usize,
    order: usize,
    backend: Option<ExecutionBackend>,
    guest_transport_bytes: Option<u64>,
    /// Journal open (when journaled) plus the session or run, wall clock.
    total_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    journal_open_us: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checks_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconcile_ms: Option<u64>,
    /// The dispatch run: the session's dispatch phase, or the runtime run's elapsed time.
    dispatch_ms: u64,
    guest_startup_ms: Option<u64>,
    execution_ms: Option<u64>,
    /// Journal commits during the session: begin, intents, observations, probes, end.
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<usize>,
    correct: bool,
    failures: Vec<String>,
}

async fn overhead_run(
    dir: &Path,
    workload: &Workload,
    latency_ms: u64,
    mode: Mode,
    (repetition, order): (usize, usize),
    deno: &Path,
) -> Result<OverheadRun, Box<dyn Error>> {
    let plan = &workload.plan;
    prepare(dir, plan, latency_ms, mode.durability())?;
    let mut record = OverheadRun {
        workload: workload.name.clone(),
        operations: plan.plan().operations.len(),
        latency_ms,
        mode,
        repetition,
        order,
        backend: None,
        guest_transport_bytes: None,
        total_us: 0,
        journal_open_us: None,
        checks_ms: None,
        reconcile_ms: None,
        dispatch_ms: 0,
        guest_startup_ms: None,
        execution_ms: None,
        commits: None,
        correct: false,
        failures: Vec::new(),
    };
    let started = Instant::now();
    match mode.durability() {
        None => {
            let operations = plan.plan().operations.clone();
            let report =
                runtime_run(operations, &dir.join("fx.db"), "runtime".into(), 1, deno).await?;
            record.total_us = micros(started.elapsed());
            record.dispatch_ms = report.elapsed_ms;
            record.backend = Some(report.backend);
            record.guest_transport_bytes = Some(report.bytes.from_guest + report.bytes.to_guest);
            (record.guest_startup_ms, record.execution_ms) = dispatch_timing(&report);
            if !report.status.is_success() {
                record
                    .failures
                    .push(format!("run ended {}", report.status.as_str()));
            }
        }
        Some(durability) => {
            let journal = Journal::open(dir.join("j.db"), durability)?;
            record.journal_open_us = Some(micros(started.elapsed()));
            let provider = FixtureProvider::open(dir.join("fx.db"), ACCOUNT)?;
            let options = SessionOptions {
                kind: SessionKind::Run,
                deno: deno.to_owned(),
            };
            let outcome =
                run_session(journal, provider, options, RunCancellation::default()).await?;
            record.total_us = micros(started.elapsed());
            record.checks_ms = Some(outcome.timing.checks_ms);
            record.reconcile_ms = Some(outcome.timing.reconcile_ms);
            record.dispatch_ms = outcome.timing.dispatch_ms;
            if let Some(trace) = &outcome.trace {
                record.backend = Some(trace.backend);
                record.guest_transport_bytes = Some(trace.bytes.from_guest + trace.bytes.to_guest);
                (record.guest_startup_ms, record.execution_ms) = dispatch_timing(trace);
            }
            if outcome.status != "completed" {
                record
                    .failures
                    .push(format!("session ended {}", outcome.status));
            }
            let history = Journal::open(dir.join("j.db"), durability)?.history()?;
            let sessions: usize = history
                .sessions
                .iter()
                .map(|session| 1 + usize::from(session.end.is_some()))
                .sum();
            let observations = history
                .attempts
                .iter()
                .filter(|attempt| attempt.observation.is_some())
                .count();
            record.commits =
                Some(sessions + history.attempts.len() + observations + history.probes.len());
        }
    }
    let (state, ledger) = read_fixture(dir)?;
    record.failures.extend(check_final(plan, &state, &ledger));
    if ledger.rows.len() != record.operations {
        record.failures.push(format!(
            "{} requests for {} operations",
            ledger.rows.len(),
            record.operations
        ));
    }
    record.correct = record.failures.is_empty();
    Ok(record)
}

#[derive(Debug, Serialize)]
struct Stats {
    min: i64,
    p50: i64,
    p90: i64,
    max: i64,
    mean: f64,
}

/// Nearest-rank percentile: the smallest value with at least `p`% of samples at
/// or below it. `sorted` must be non-empty and ascending.
fn nearest_rank(sorted: &[i64], p: usize) -> i64 {
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn stats(values: impl IntoIterator<Item = i64>) -> Option<Stats> {
    let mut sorted: Vec<i64> = values.into_iter().collect();
    sorted.sort_unstable();
    let (&min, &max) = (sorted.first()?, sorted.last()?);
    Some(Stats {
        min,
        p50: nearest_rank(&sorted, 50),
        p90: nearest_rank(&sorted, 90),
        max,
        mean: sorted.iter().sum::<i64>() as f64 / sorted.len() as f64,
    })
}

fn opt_stats(values: impl IntoIterator<Item = Option<u64>>) -> Option<Stats> {
    stats(values.into_iter().flatten().map(to_i64))
}

#[derive(Debug, Serialize)]
struct RecoveryAggregate {
    latency_ms: u64,
    strategy: Strategy,
    runs: usize,
    state_correct_runs: usize,
    claims_complete_runs: usize,
    failed_runs: usize,
    unresolved: (usize, usize),
    requests_sent: (usize, usize),
    duplicate_effects: (u64, u64),
    repeated_without_effect: (u64, u64),
    recovery_us: Option<Stats>,
    journal_us: Option<Stats>,
    checks_ms: Option<Stats>,
    reconcile_ms: Option<Stats>,
    dispatch_ms: Option<Stats>,
    guest_startup_ms: Option<Stats>,
    execution_ms: Option<Stats>,
}

fn span<T: Ord + Copy>(values: impl IntoIterator<Item = T>) -> Option<(T, T)> {
    let values: Vec<T> = values.into_iter().collect();
    Some((*values.iter().min()?, *values.iter().max()?))
}

fn recovery_aggregate(
    latency_ms: u64,
    strategy: Strategy,
    runs: &[&RecoveryRun],
) -> RecoveryAggregate {
    let count = |predicate: fn(&RecoveryRun) -> bool| runs.iter().filter(|r| predicate(r)).count();
    RecoveryAggregate {
        latency_ms,
        strategy,
        runs: runs.len(),
        state_correct_runs: count(|r| r.state_correct),
        claims_complete_runs: count(|r| r.claims_complete),
        failed_runs: count(|r| !r.failures.is_empty()),
        unresolved: span(runs.iter().map(|r| r.unresolved)).unwrap_or_default(),
        requests_sent: span(runs.iter().map(|r| r.requests_sent)).unwrap_or_default(),
        duplicate_effects: span(runs.iter().map(|r| r.duplicate_effects)).unwrap_or_default(),
        repeated_without_effect: span(runs.iter().map(|r| r.repeated_without_effect))
            .unwrap_or_default(),
        recovery_us: stats(runs.iter().map(|r| r.recovery_us)),
        journal_us: stats(runs.iter().map(|r| r.journal_us)),
        checks_ms: opt_stats(runs.iter().map(|r| r.checks_ms)),
        reconcile_ms: opt_stats(runs.iter().map(|r| r.reconcile_ms)),
        dispatch_ms: opt_stats(runs.iter().map(|r| r.dispatch_ms)),
        guest_startup_ms: opt_stats(runs.iter().map(|r| r.guest_startup_ms)),
        execution_ms: opt_stats(runs.iter().map(|r| r.execution_ms)),
    }
}

#[derive(Debug, Serialize)]
struct OverheadAggregate {
    workload: String,
    operations: usize,
    latency_ms: u64,
    mode: Mode,
    runs: usize,
    correct_runs: usize,
    total_us: Option<Stats>,
    journal_open_us: Option<Stats>,
    checks_ms: Option<Stats>,
    dispatch_ms: Option<Stats>,
    guest_startup_ms: Option<Stats>,
    execution_ms: Option<Stats>,
    commits: Option<(usize, usize)>,
    /// Per repetition, this mode's total minus the runtime-only total, µs.
    paired_total_delta_us: Option<Stats>,
    /// Per repetition, this mode's dispatch execution minus the runtime-only one, ms.
    paired_execution_delta_ms: Option<Stats>,
    /// Median paired total delta divided by operations, ms.
    per_operation_ms: Option<f64>,
    /// Median paired total delta divided by commits, ms.
    per_commit_ms: Option<f64>,
}

fn overhead_aggregate(
    workload: &str,
    latency_ms: u64,
    mode: Mode,
    all: &[OverheadRun],
) -> OverheadAggregate {
    let cell = |mode: Mode| -> Vec<&OverheadRun> {
        all.iter()
            .filter(|r| r.workload == workload && r.latency_ms == latency_ms && r.mode == mode)
            .collect()
    };
    let runs = cell(mode);
    let baseline: HashMap<usize, &OverheadRun> = cell(Mode::Runtime)
        .into_iter()
        .map(|r| (r.repetition, r))
        .collect();
    let paired = |delta: fn(&OverheadRun, &OverheadRun) -> Option<i64>| {
        if mode == Mode::Runtime {
            return None;
        }
        stats(
            runs.iter()
                .filter_map(|r| baseline.get(&r.repetition).and_then(|b| delta(r, b))),
        )
    };
    let total_delta = paired(|r, b| Some(r.total_us - b.total_us));
    let execution_delta = paired(|r, b| Some(to_i64(r.execution_ms?) - to_i64(b.execution_ms?)));
    let operations = runs.first().map_or(0, |r| r.operations);
    let commits = span(runs.iter().filter_map(|r| r.commits));
    let per = |divisor: usize| {
        total_delta
            .as_ref()
            .filter(|_| divisor > 0)
            .map(|s| s.p50 as f64 / 1000.0 / divisor as f64)
    };
    OverheadAggregate {
        workload: workload.to_string(),
        operations,
        latency_ms,
        mode,
        runs: runs.len(),
        correct_runs: runs.iter().filter(|r| r.correct).count(),
        total_us: stats(runs.iter().map(|r| r.total_us)),
        journal_open_us: stats(runs.iter().filter_map(|r| r.journal_open_us)),
        checks_ms: opt_stats(runs.iter().map(|r| r.checks_ms)),
        dispatch_ms: stats(runs.iter().map(|r| to_i64(r.dispatch_ms))),
        guest_startup_ms: opt_stats(runs.iter().map(|r| r.guest_startup_ms)),
        execution_ms: opt_stats(runs.iter().map(|r| r.execution_ms)),
        per_operation_ms: per(operations),
        per_commit_ms: commits.and_then(|(low, _)| per(low)),
        commits,
        paired_total_delta_us: total_delta,
        paired_execution_delta_ms: execution_delta,
    }
}

fn ms_from_us(us: i64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

fn p50_us(stats: Option<&Stats>) -> String {
    stats.map_or("—".into(), |s| ms_from_us(s.p50))
}

fn p50_ms(stats: Option<&Stats>) -> String {
    stats.map_or("—".into(), |s| s.p50.to_string())
}

fn p50_p90_ms(stats: Option<&Stats>) -> String {
    stats.map_or("—".into(), |s| format!("{} / {}", s.p50, s.p90))
}

fn range_text<T: PartialEq + std::fmt::Display>((low, high): (T, T)) -> String {
    if low == high {
        low.to_string()
    } else {
        format!("{low}–{high}")
    }
}

fn render_markdown(
    environment: &Environment,
    args: &Args,
    recovery: &[RecoveryAggregate],
    overhead: &[OverheadAggregate],
    failures: &[String],
) -> String {
    let mut out = String::new();
    let commit = match (&environment.git_commit, environment.git_dirty) {
        (Some(commit), Some(true)) => format!("{commit} (tracked changes uncommitted)"),
        (Some(commit), _) => commit.clone(),
        (None, _) => "unknown".into(),
    };
    let _ = writeln!(out, "# Recovery evaluation\n");
    let _ = writeln!(
        out,
        "- Build: commit {commit}, {} profile, codemode {}, {}.",
        environment.build_profile,
        environment.codemode_version,
        environment.rustc.as_deref().unwrap_or("rustc unknown")
    );
    let _ = writeln!(
        out,
        "- Environment: {} {}, {}, {} logical CPUs.",
        environment.os,
        environment.arch,
        environment
            .cpu_model
            .as_deref()
            .unwrap_or("CPU model unknown"),
        environment
            .logical_cpus
            .map_or("unknown".into(), |n| n.to_string()),
    );
    let _ = writeln!(
        out,
        "- Backend: native for every dispatch. Guest startup is absent (—); execution is the full native dispatch run. No Deno executable is discovered or launched."
    );
    let _ = writeln!(
        out,
        "- Repetitions: {} measured after {} discarded warmups. Strategies (A) and modes (B) run round-robin within a repetition; the starting one rotates every repetition.",
        args.reps, args.warmup
    );
    let _ = writeln!(
        out,
        "- Matching parameters: concurrency {CONCURRENCY} and dispatch deadline {RUN_TIMEOUT_MS} ms for every strategy and mode; fixture latency per profile; the same frozen plan and fixture seed."
    );
    let _ = writeln!(
        out,
        "- Percentiles use the nearest-rank method. Totals are measured in µs and shown in ms; phases come from the session's own millisecond timings and the trace."
    );

    let _ = writeln!(out, "\n## A. Recovery after a killed run\n");
    let _ = writeln!(
        out,
        "Crashed state, identical for all strategies: 6 operations observed applied, 4 unknown (3 lost responses after the effect, 1 held before it), 1 never dispatched.\n"
    );
    let _ = writeln!(
        out,
        "| latency | strategy | final state correct | claims complete | unresolved ops | requests sent | duplicate effects | repeats without effect |"
    );
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---:|---:|---:|");
    for a in recovery {
        let _ = writeln!(
            out,
            "| {} ms | {} | {}/{} | {}/{} | {} | {} | {} | {} |",
            a.latency_ms,
            a.strategy.name(),
            a.state_correct_runs,
            a.runs,
            a.claims_complete_runs,
            a.runs,
            range_text(a.unresolved),
            range_text(a.requests_sent),
            range_text(a.duplicate_effects),
            range_text(a.repeated_without_effect),
        );
    }
    let _ = writeln!(out, "\n### Recovery time (ms, strategy start to done)\n");
    let _ = writeln!(out, "| latency | strategy | min | p50 | p90 | max | mean |");
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---:|---:|");
    for a in recovery {
        if let Some(s) = &a.recovery_us {
            let _ = writeln!(
                out,
                "| {} ms | {} | {} | {} | {} | {} | {:.1} |",
                a.latency_ms,
                a.strategy.name(),
                ms_from_us(s.min),
                ms_from_us(s.p50),
                ms_from_us(s.p90),
                ms_from_us(s.max),
                s.mean / 1000.0
            );
        }
    }
    let _ = writeln!(out, "\n### Where the time goes (p50, ms)\n");
    let _ = writeln!(
        out,
        "| latency | strategy | journal open/read | checks | reconcile | guest startup | dispatch execution | dispatch phase | total |"
    );
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---:|---:|---:|---:|");
    for a in recovery {
        let _ = writeln!(
            out,
            "| {} ms | {} | {} | {} | {} | {} | {} | {} | {} |",
            a.latency_ms,
            a.strategy.name(),
            p50_us(a.journal_us.as_ref()),
            p50_ms(a.checks_ms.as_ref()),
            p50_ms(a.reconcile_ms.as_ref()),
            p50_ms(a.guest_startup_ms.as_ref()),
            p50_ms(a.execution_ms.as_ref()),
            p50_ms(a.dispatch_ms.as_ref()),
            p50_us(a.recovery_us.as_ref()),
        );
    }

    if !overhead.is_empty() {
        let _ = writeln!(out, "\n## B. Persistence overhead without faults\n");
        let _ = writeln!(
            out,
            "| workload | ops | latency | mode | correct | total p50 / p90 | dispatch p50 / p90 | journal open p50 | checks p50 | guest startup p50 | execution p50 / p90 | commits |"
        );
        let _ = writeln!(
            out,
            "|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for a in overhead {
            let total = a.total_us.as_ref().map_or("—".into(), |s| {
                format!("{} / {}", ms_from_us(s.p50), ms_from_us(s.p90))
            });
            let _ = writeln!(
                out,
                "| {} | {} | {} ms | {} | {}/{} | {total} | {} | {} | {} | {} | {} | {} |",
                a.workload,
                a.operations,
                a.latency_ms,
                a.mode.name(),
                a.correct_runs,
                a.runs,
                p50_p90_ms(a.dispatch_ms.as_ref()),
                p50_us(a.journal_open_us.as_ref()),
                p50_ms(a.checks_ms.as_ref()),
                p50_ms(a.guest_startup_ms.as_ref()),
                p50_p90_ms(a.execution_ms.as_ref()),
                a.commits.map_or("—".into(), range_text),
            );
        }
        let _ = writeln!(
            out,
            "\n### Journal cost against the runtime alone (paired by repetition)\n"
        );
        let _ = writeln!(
            out,
            "| workload | ops | latency | durability | Δ total p50 / p90 (ms) | per op (ms) | per commit (ms) | Δ execution p50 / p90 (ms) |"
        );
        let _ = writeln!(out, "|---|---:|---:|---|---:|---:|---:|---:|");
        for a in overhead.iter().filter(|a| a.mode != Mode::Runtime) {
            let total = a.paired_total_delta_us.as_ref().map_or("—".into(), |s| {
                format!("{} / {}", ms_from_us(s.p50), ms_from_us(s.p90))
            });
            let per = |value: Option<f64>| value.map_or("—".into(), |v| format!("{v:.2}"));
            let _ = writeln!(
                out,
                "| {} | {} | {} ms | {} | {total} | {} | {} | {} |",
                a.workload,
                a.operations,
                a.latency_ms,
                a.mode.name(),
                per(a.per_operation_ms),
                per(a.per_commit_ms),
                p50_p90_ms(a.paired_execution_delta_ms.as_ref()),
            );
        }
    }

    if failures.is_empty() {
        let _ = writeln!(out, "\nAll correctness checks passed.");
    } else {
        let _ = writeln!(out, "\n## Correctness failures\n");
        for failure in failures {
            let _ = writeln!(out, "- {failure}");
        }
    }
    out
}

#[derive(Debug, Serialize)]
struct Environment {
    backend: ExecutionBackend,
    os: &'static str,
    arch: &'static str,
    cpu_model: Option<String>,
    logical_cpus: Option<usize>,
    codemode_version: &'static str,
    build_profile: &'static str,
    /// `rustc --version` on PATH when the evaluation ran.
    rustc: Option<String>,
    /// Commit of the source checkout when the evaluation runs, when `git` works.
    git_commit: Option<String>,
    /// Whether tracked files differ from that commit (untracked files ignored).
    git_dirty: Option<bool>,
    evaluator_binary_digest: Option<Digest>,
    codemode_binary_digest: Option<Digest>,
    evaluator_source_digest: Digest,
    fixture_source_digest: Digest,
    recovery_plan_source_digest: Digest,
    /// The executable killed mid-run, relative to the source checkout when inside it.
    codemode_bin: PathBuf,
}

/// Trimmed stdout of a command that exited successfully.
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn cpu_model() -> Option<String> {
    if cfg!(target_os = "macos") {
        command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
    } else {
        let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        info.lines()
            .find(|line| line.starts_with("model name"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, model)| model.trim().to_owned())
    }
}

fn environment(args: &Args) -> Environment {
    let source = env!("CARGO_MANIFEST_DIR");
    let binary_digest = |path: &Path| std::fs::read(path).ok().map(|bytes| Digest::of(&bytes));
    Environment {
        backend: ExecutionBackend::Native,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_model: cpu_model().filter(|model| !model.is_empty()),
        logical_cpus: std::thread::available_parallelism().ok().map(Into::into),
        codemode_version: env!("CARGO_PKG_VERSION"),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        rustc: command_output("rustc", &["--version"]),
        git_commit: command_output("git", &["-C", source, "rev-parse", "HEAD"]),
        git_dirty: command_output(
            "git",
            &[
                "-C",
                source,
                "status",
                "--porcelain",
                "--untracked-files=no",
            ],
        )
        .map(|status| !status.is_empty()),
        evaluator_binary_digest: std::env::current_exe()
            .ok()
            .and_then(|path| binary_digest(&path)),
        codemode_binary_digest: binary_digest(&args.bin),
        evaluator_source_digest: Digest::of(include_bytes!("codemode-recovery-eval.rs")),
        fixture_source_digest: Digest::of(include_bytes!("../fixture.rs")),
        recovery_plan_source_digest: Digest::of(RECOVERY_PLAN.as_bytes()),
        codemode_bin: args
            .bin
            .strip_prefix(source)
            .unwrap_or(&args.bin)
            .to_path_buf(),
    }
}

#[derive(Debug, Serialize)]
struct EvalConfig {
    backend: ExecutionBackend,
    reps: usize,
    warmup: usize,
    latencies_ms: Vec<u64>,
    concurrency: usize,
    dispatch_timeout_ms: u64,
    workloads: Vec<(String, usize)>,
    faults: Vec<String>,
    percentiles: &'static str,
    ordering: &'static str,
    crash: &'static str,
}

#[derive(Debug, Serialize)]
struct Summary<'a> {
    environment: &'a Environment,
    config: &'a EvalConfig,
    recovery_runs: &'a [RecoveryRun],
    recovery: &'a [RecoveryAggregate],
    overhead_runs: &'a [OverheadRun],
    overhead: &'a [OverheadAggregate],
    failures: &'a [String],
}

struct Args {
    reps: usize,
    warmup: usize,
    latencies: Vec<u64>,
    large: usize,
    out: Option<PathBuf>,
    deno: PathBuf,
    bin: PathBuf,
}

fn default_bin() -> PathBuf {
    let name = format!("codemode{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Returns `None` when help was requested.
fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        reps: 15,
        warmup: 2,
        latencies: vec![0, 25],
        large: 64,
        out: None,
        deno: PathBuf::from("deno"),
        bin: default_bin(),
    };
    let mut input = std::env::args_os().skip(1);
    while let Some(flag) = input.next() {
        let flag = flag.to_string_lossy().into_owned();
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--reps" | "--warmup" | "--latency" | "--large" | "--out" | "--deno" | "--bin" => {}
            _ => return Err(format!("unknown option '{flag}'; use --help for usage")),
        }
        let value = input
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        let text = value.to_string_lossy().into_owned();
        let number = || {
            text.parse::<usize>()
                .map_err(|_| format!("{flag} requires a nonnegative integer"))
        };
        match flag.as_str() {
            "--reps" => args.reps = number()?,
            "--warmup" => args.warmup = number()?,
            "--large" => args.large = number()?,
            "--latency" => {
                args.latencies = text
                    .split(',')
                    .map(|part| part.trim().parse::<u64>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "--latency requires comma-separated milliseconds".to_string())?;
            }
            "--out" => args.out = Some(PathBuf::from(value)),
            "--deno" => args.deno = PathBuf::from(value),
            _ => args.bin = PathBuf::from(value),
        }
    }
    if args.reps == 0 || args.latencies.is_empty() {
        return Err("--reps and --latency need at least one value".into());
    }
    if args.large > codemode::MAX_OPERATIONS {
        return Err(format!(
            "--large must be at most {}",
            codemode::MAX_OPERATIONS
        ));
    }
    if !args.bin.is_file() {
        return Err(format!(
            "{} does not exist; build it with `cargo build --release --locked --bin codemode` or pass --bin",
            args.bin.display()
        ));
    }
    Ok(Some(args))
}

/// A private directory for every database file, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let root = std::env::temp_dir().join(format!(
            "codemode-recovery-eval-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root)?;
        Ok(Self(root))
    }

    /// A fresh, empty directory.
    fn fresh(&self, name: &str) -> std::io::Result<PathBuf> {
        let dir = self.0.join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match evaluate().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("codemode-recovery-eval: {error}");
            ExitCode::from(2)
        }
    }
}

/// One repetition of part A at one latency: crash once, then recover three copies.
async fn recovery_round(
    scratch: &Scratch,
    plan: &FrozenPlan,
    latency_ms: u64,
    (round, repetition): (usize, usize),
    args: &Args,
) -> Result<(Vec<RecoveryRun>, Vec<String>), Box<dyn Error>> {
    let crashed = scratch.fresh("crash")?;
    crash(&crashed, plan, latency_ms, args).await?;
    for strategy in STRATEGIES {
        copy_state(&crashed, &scratch.fresh(&format!("{strategy:?}"))?)?;
    }
    let crash_failures = verify_crash(&crashed, plan)?;
    let mut runs = Vec::new();
    for (order, strategy) in rotation(STRATEGIES, round).enumerate() {
        let dir = scratch.0.join(format!("{strategy:?}"));
        let mut record = RecoveryRun::new(latency_ms, strategy, repetition, order);
        recover(strategy, &dir, plan, &mut record, args).await?;
        runs.push(record);
    }
    Ok((runs, crash_failures))
}

/// Run parts A and B. Exit 0 when all checks pass, 1 when any fails (even if
/// writing results also fails), and 2 on a setup or output error.
async fn evaluate() -> Result<ExitCode, Box<dyn Error>> {
    let Some(args) = parse_args()? else {
        print!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    };
    let recovery_plan = Plan::parse(RECOVERY_PLAN)?;
    let mut workloads = vec![Workload {
        name: RECOVERY_WORKLOAD.into(),
        plan: recovery_plan.clone(),
    }];
    if args.large > 0 {
        workloads.push(Workload {
            name: LARGE_WORKLOAD.into(),
            plan: large_plan(args.large)?,
        });
    }
    let environment = environment(&args);
    let scratch = Scratch::new()?;
    let rounds = args
        .warmup
        .checked_add(args.reps)
        .ok_or("--reps plus --warmup overflows")?;
    let mut failures = Vec::new();

    let mut recovery_runs = Vec::new();
    for round in 0..rounds {
        let repetition = round.checked_sub(args.warmup);
        match repetition {
            None => eprintln!(
                "codemode-recovery-eval: A warmup {}/{}",
                round + 1,
                args.warmup
            ),
            Some(r) => eprintln!(
                "codemode-recovery-eval: A repetition {}/{}",
                r + 1,
                args.reps
            ),
        }
        for &latency in &args.latencies {
            let (runs, crash_failures) = recovery_round(
                &scratch,
                &recovery_plan,
                latency,
                (round, repetition.unwrap_or(0)),
                &args,
            )
            .await?;
            if let Some(repetition) = repetition {
                for failure in crash_failures {
                    failures.push(format!("A {latency} ms repetition {repetition}: {failure}"));
                }
                for run in &runs {
                    for failure in &run.failures {
                        failures.push(format!(
                            "A {latency} ms repetition {repetition} {}: {failure}",
                            run.strategy.name()
                        ));
                    }
                }
                recovery_runs.extend(runs);
            }
        }
    }

    let mut overhead_runs = Vec::new();
    for round in 0..rounds {
        let repetition = round.checked_sub(args.warmup);
        match repetition {
            None => eprintln!(
                "codemode-recovery-eval: B warmup {}/{}",
                round + 1,
                args.warmup
            ),
            Some(r) => eprintln!(
                "codemode-recovery-eval: B repetition {}/{}",
                r + 1,
                args.reps
            ),
        }
        for workload in &workloads {
            for &latency in &args.latencies {
                for (order, mode) in rotation(MODES, round).enumerate() {
                    let dir = scratch.fresh("overhead")?;
                    let run = overhead_run(
                        &dir,
                        workload,
                        latency,
                        mode,
                        (repetition.unwrap_or(0), order),
                        &args.deno,
                    )
                    .await?;
                    if let Some(repetition) = repetition {
                        for failure in &run.failures {
                            failures.push(format!(
                                "B {} {latency} ms repetition {repetition} {}: {failure}",
                                workload.name,
                                mode.name()
                            ));
                        }
                        overhead_runs.push(run);
                    }
                }
            }
        }
    }

    let mut recovery = Vec::new();
    for &latency in &args.latencies {
        for strategy in STRATEGIES {
            let runs: Vec<&RecoveryRun> = recovery_runs
                .iter()
                .filter(|r| r.latency_ms == latency && r.strategy == strategy)
                .collect();
            recovery.push(recovery_aggregate(latency, strategy, &runs));
        }
    }
    let mut overhead = Vec::new();
    for workload in &workloads {
        for &latency in &args.latencies {
            for mode in MODES {
                overhead.push(overhead_aggregate(
                    &workload.name,
                    latency,
                    mode,
                    &overhead_runs,
                ));
            }
        }
    }

    print!(
        "{}",
        render_markdown(&environment, &args, &recovery, &overhead, &failures)
    );
    let checks = if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };

    if let Some(dir) = &args.out {
        let config = EvalConfig {
            backend: ExecutionBackend::Native,
            reps: args.reps,
            warmup: args.warmup,
            latencies_ms: args.latencies.clone(),
            concurrency: CONCURRENCY,
            dispatch_timeout_ms: RUN_TIMEOUT_MS,
            workloads: workloads
                .iter()
                .map(|w| (w.name.clone(), w.plan.plan().operations.len()))
                .collect(),
            faults: FAULTS
                .iter()
                .map(|(tool, issue, mode)| format!("{tool} #{issue} {mode:?}"))
                .collect(),
            percentiles: "nearest-rank",
            ordering: "round-robin strategies (A) and modes (B) per repetition; the starting one rotates each repetition",
            crash: "codemode plan run is sent SIGKILL once the fixture ledger records the held request; journal and fixture files (database and write-ahead log) are copied once per strategy",
        };
        let summary = Summary {
            environment: &environment,
            config: &config,
            recovery_runs: &recovery_runs,
            recovery: &recovery,
            overhead_runs: &overhead_runs,
            overhead: &overhead,
            failures: &failures,
        };
        let written = std::fs::create_dir_all(dir).and_then(|()| {
            let text = serde_json::to_string_pretty(&summary).map_err(std::io::Error::other)?;
            std::fs::write(dir.join("summary.json"), text + "\n")
        });
        if let Err(error) = written {
            eprintln!(
                "codemode-recovery-eval: cannot write {}: {error}",
                dir.display()
            );
            return Ok(ExitCode::from(if failures.is_empty() { 2 } else { 1 }));
        }
        eprintln!("codemode-recovery-eval: wrote {}", dir.display());
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::{nearest_rank, rotation};

    #[test]
    fn nearest_rank_percentiles() {
        let values: Vec<i64> = (1..=15).collect();
        assert_eq!(nearest_rank(&values, 50), 8);
        assert_eq!(nearest_rank(&values, 90), 14);
        assert_eq!(nearest_rank(&[7], 90), 7);
    }

    #[test]
    fn rotation_starts_one_later_each_round() {
        let second: Vec<u8> = rotation([1, 2, 3], 1).collect();
        assert_eq!(second, [2, 3, 1]);
    }
}
