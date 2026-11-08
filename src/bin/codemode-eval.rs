//! Offline comparison of sequential, batch, and rolling dispatch.
//!
//! Every policy runs the same fixture programs from `eval/fixtures.json` against
//! the deterministic [`MockProvider`], under one shared budget. Only the dispatch
//! policy and its concurrency cap differ:
//!
//! - `sequential`: rolling dispatch with a cap of 1;
//! - `batch`: up to 4 calls start together, and the next batch waits for all of them;
//! - `rolling`: up to 4 calls, refilling a slot as soon as one frees.
//!
//! Policies are interleaved round-robin within each repetition, and the starting
//! policy rotates per repetition, so slow drift on the machine does not favor one
//! policy. Each run is checked against reviewed expectations and against policy
//! invariants replayed from the host's event order; the process exits 1 if any
//! check fails. No speedup is assumed: the summary reports measured
//! distributions next to an ideal zero-overhead model.
//!
//! ```text
//! cargo run --release --locked --bin codemode-eval -- \
//!     [--reps N] [--warmup N] [--out DIR] [--deno PATH] [--fixtures PATH]
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use codemode::{
    CallOutcome, DispatchPolicy, MockProvider, RunCancellation, RunConfig, RunReport, RunStatus,
    run,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Budget shared by every policy; a fixture may override only the timeout.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_CALLS: usize = 32;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Concurrency cap for batch and rolling; sequential always uses 1.
const PARALLEL_CAP: usize = 4;

const HELP: &str = "codemode-eval — compare dispatch policies on identical offline fixtures

Usage:
  codemode-eval [--reps N] [--warmup N] [--out DIR] [--deno PATH] [--fixtures PATH]

Options:
  --reps <n>         Measured repetitions per fixture and policy (default: 20)
  --warmup <n>       Discarded repetitions before measuring (default: 2)
  --out <dir>        Also write summary.json and traces/<fixture>-<policy>.json
  --deno <path>      Deno executable (default: deno from PATH)
  --fixtures <path>  Fixture manifest (default: eval/fixtures.json)
  -h, --help         Show this help

Prints a Markdown summary to stdout and progress to stderr. Exits 1 if any
correctness check fails and 2 on a usage, setup, or output error.
";

// ---------------------------------------------------------------------------
// Manifest

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    fixtures: Vec<Fixture>,
}

/// One guest program and its reviewed expectations.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// Unique name, also used in trace file names.
    name: String,
    /// Guest source, relative to the manifest's directory.
    file: PathBuf,
    description: String,
    /// Reviewer-facing context printed with the methodology, such as timing
    /// assumptions behind outcomes that are reported but not asserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Run deadline override; every policy uses the same value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    /// Provider latency of each call in submission order (`mock.fail` is 0).
    /// Checked against every trace, and used by the ideal schedule model.
    latencies_ms: Vec<u64>,
    /// Whether the ideal model applies: all calls are submitted together and
    /// the run does not end before they all finish.
    ideal_model: bool,
    expect: Expect,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    /// Expected run status, as serialized in the trace (for example `success`).
    status: String,
    /// `"absent"` or `{"equals": <json>}`; unchecked when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<OutputExpectation>,
    /// Exact count for each listed call outcome under every policy; unlisted
    /// outcomes are unchecked. List only counts that timing cannot change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    outcomes: BTreeMap<String, usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OutputExpectation {
    /// The report must have no `output` field.
    Absent,
    /// The report's output must equal this value exactly (`null` included).
    Equals(Value),
}

fn load_manifest(path: &Path) -> Result<(Vec<Fixture>, Vec<String>), Box<dyn Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read manifest {}: {error}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|error| format!("invalid manifest {}: {error}", path.display()))?;
    if manifest.fixtures.is_empty() {
        return Err("manifest has no fixtures".into());
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let mut names = HashSet::new();
    let mut sources = Vec::new();
    for fixture in &manifest.fixtures {
        let safe = fixture
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !safe || !names.insert(fixture.name.as_str()) {
            return Err(format!("fixture name '{}' is unsafe or repeated", fixture.name).into());
        }
        if fixture.latencies_ms.is_empty() || fixture.timeout_ms == Some(0) {
            return Err(format!(
                "fixture '{}' needs latencies and a positive timeout",
                fixture.name
            )
            .into());
        }
        let file = base.join(&fixture.file);
        sources.push(
            std::fs::read_to_string(&file)
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?,
        );
    }
    Ok((manifest.fixtures, sources))
}

// ---------------------------------------------------------------------------
// Policies

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    Sequential,
    Batch,
    Rolling,
}

const POLICIES: [Policy; 3] = [Policy::Sequential, Policy::Batch, Policy::Rolling];

impl Policy {
    fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Batch => "batch",
            Self::Rolling => "rolling",
        }
    }

    /// Sequential is not a separate scheduler: it is rolling dispatch with one slot.
    fn dispatch(self) -> DispatchPolicy {
        match self {
            Self::Sequential | Self::Rolling => DispatchPolicy::Rolling,
            Self::Batch => DispatchPolicy::Batch,
        }
    }

    fn cap(self) -> usize {
        match self {
            Self::Sequential => 1,
            Self::Batch | Self::Rolling => PARALLEL_CAP,
        }
    }

    fn config(self, fixture: &Fixture, deno: &Path) -> RunConfig {
        RunConfig {
            timeout_ms: fixture.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            max_calls: MAX_CALLS,
            max_concurrency: self.cap(),
            max_output_bytes: MAX_OUTPUT_BYTES,
            deno: deno.to_owned(),
            dispatch: self.dispatch(),
            ..RunConfig::default()
        }
    }
}

/// Policy order for one repetition: round-robin, starting one later each time.
fn rotation(repetition: usize) -> impl Iterator<Item = Policy> {
    (0..POLICIES.len()).map(move |offset| POLICIES[(repetition + offset) % POLICIES.len()])
}

/// Ideal makespan with zero overhead, from manifest latencies alone. A model of
/// the scheduling policy, not a measurement: it ignores startup, I/O, and timers.
fn ideal_makespan(latencies: &[u64], policy: Policy) -> u64 {
    let cap = policy.cap();
    match policy.dispatch() {
        // Each chunk of `cap` calls waits for its slowest member.
        DispatchPolicy::Batch => latencies
            .chunks(cap)
            .map(|chunk| chunk.iter().copied().max().unwrap_or(0))
            .sum(),
        // Greedy list scheduling: each call, in submission order, takes the slot
        // that frees first. With one slot this is the sum of latencies.
        DispatchPolicy::Rolling => {
            let mut free_at = vec![0u64; cap];
            for latency in latencies {
                if let Some(slot) = free_at.iter_mut().min() {
                    *slot += latency;
                }
            }
            free_at.into_iter().max().unwrap_or(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Trace invariants

/// Run-level events, which name no call.
const RUN_EVENTS: [&str; 6] = [
    "guest_spawned",
    "guest_finished",
    "deadline_reached",
    "cancellation_observed",
    "guest_terminated",
    "run_finished",
];

/// Events after which a call holds no outcome other than its final one.
const TERMINAL_EVENTS: [&str; 5] = [
    "succeeded",
    "provider_failed",
    "rejected",
    "cancelled_before_dispatch",
    "unknown",
];

fn outcome_name(outcome: &CallOutcome) -> &'static str {
    match outcome {
        CallOutcome::Queued => "queued",
        CallOutcome::Running => "running",
        CallOutcome::Succeeded { .. } => "succeeded",
        CallOutcome::ProviderFailed { .. } => "provider_failed",
        CallOutcome::Rejected { .. } => "rejected",
        CallOutcome::CancelledBeforeDispatch => "cancelled_before_dispatch",
        CallOutcome::ResultTooLarge => "result_too_large",
        CallOutcome::Unknown => "unknown",
    }
}

/// The status name exactly as it appears in the serialized trace.
fn status_name(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_default()
}

/// Cross-check events against call records so that the replay below cannot
/// pass vacuously: every call is admitted once, dispatched once exactly when it
/// has a start time, and ends with one terminal event matching its outcome.
fn check_call_events(report: &RunReport, failures: &mut Vec<String>) {
    let mut seen: HashMap<(u64, &str), usize> = HashMap::new();
    for event in &report.events {
        match event.call_id {
            Some(id) => *seen.entry((id, event.kind.as_str())).or_default() += 1,
            None if !RUN_EVENTS.contains(&event.kind.as_str()) => {
                failures.push(format!("event {} ({}) has no call", event.id, event.kind));
            }
            None => {}
        }
    }
    let count = |id: u64, kind: &str| seen.get(&(id, kind)).copied().unwrap_or(0);
    let finished = report
        .events
        .iter()
        .filter(|e| e.kind == "run_finished")
        .count();
    if finished != 1
        || report
            .events
            .last()
            .is_none_or(|e| e.kind != "run_finished")
    {
        failures.push("run_finished is not the single last event".into());
    }
    let known: HashSet<u64> = report.calls.iter().map(|r| r.call.id).collect();
    if seen.keys().any(|(id, _)| !known.contains(id)) {
        failures.push("an event names a call that has no record".into());
    }
    for record in &report.calls {
        let id = record.call.id;
        let outcome = outcome_name(&record.outcome);
        if count(id, "admitted") != 1 {
            failures.push(format!(
                "call {id} has {} admitted events",
                count(id, "admitted")
            ));
        }
        let dispatched = usize::from(record.started_ms.is_some());
        if count(id, "dispatched") != dispatched {
            failures.push(format!(
                "call {id} dispatch events disagree with started_ms"
            ));
        }
        let terminal: Vec<&str> = TERMINAL_EVENTS
            .into_iter()
            .filter(|kind| count(id, kind) > 0)
            .collect();
        let total: usize = terminal.iter().map(|kind| count(id, kind)).sum();
        let matches = match record.outcome {
            CallOutcome::Queued | CallOutcome::Running => false,
            // An oversized result keeps the event kind of the observed result.
            CallOutcome::ResultTooLarge => {
                matches!(terminal[..], ["succeeded"] | ["provider_failed"])
            }
            _ => terminal[..] == [outcome],
        };
        if total != 1 || !matches || record.finished_ms.is_none() {
            failures.push(format!(
                "call {id} ({outcome}) has terminal events {terminal:?}"
            ));
        }
        if matches!(
            record.outcome,
            CallOutcome::Succeeded { .. } | CallOutcome::ProviderFailed { .. }
        ) && !matches!((record.started_ms, record.finished_ms), (Some(s), Some(f)) if s <= f)
        {
            failures.push(format!("call {id} lacks started_ms <= finished_ms"));
        }
    }
}

/// Replay events in order with a set of active calls and a queue of admitted,
/// not-yet-dispatched calls, checking the policy's dispatch rule. Returns the
/// peak number of active calls.
///
/// - Rolling (and sequential): whenever a slot is free and a call is queued,
///   the next event must dispatch, or settle a call without dispatch (rejection,
///   or the cancellation sweep when the guest finished or the run stopped).
/// - Batch: a batch starts only when no call is active and launches
///   `min(cap, queued)` calls in one uninterrupted burst; when nothing is active
///   and calls are queued, the next event must start a batch, admit another
///   already-written call into it, or settle a call.
fn replay_dispatch(report: &RunReport, policy: Policy, failures: &mut Vec<String>) -> usize {
    let cap = policy.cap();
    let batch = policy.dispatch() == DispatchPolicy::Batch;
    let mut queued: Vec<u64> = Vec::new();
    let mut active: HashSet<u64> = HashSet::new();
    let mut peak = 0;
    // Event id that left a free slot with queued work, obliging the next event.
    let mut obliged_by: Option<u64> = None;
    // Current batch burst: (expected size, dispatched so far).
    let mut burst: Option<(usize, usize)> = None;
    for event in &report.events {
        let kind = event.kind.as_str();
        let id = event.call_id.unwrap_or(0);
        if let Some(previous) = obliged_by.take() {
            // A run that stops instead of dispatching first records why.
            let allowed = matches!(
                kind,
                "dispatched"
                    | "rejected"
                    | "cancelled_before_dispatch"
                    | "unknown"
                    | "deadline_reached"
                    | "cancellation_observed"
            ) || (batch && kind == "admitted");
            if !allowed {
                failures.push(format!(
                    "free slot left idle after event {previous}: next event {} is {kind}",
                    event.id
                ));
            }
        }
        if kind != "dispatched" {
            if let Some((expected, launched)) = burst.take() {
                // Only a deadline or cancellation sweep may cut a burst short.
                let cut_short =
                    launched < expected && matches!(kind, "unknown" | "cancelled_before_dispatch");
                if launched != expected && !cut_short {
                    failures.push(format!(
                        "batch launched {launched} calls, expected {expected}"
                    ));
                }
            }
        }
        match kind {
            "admitted" => queued.push(id),
            "dispatched" => {
                if batch && burst.is_none() {
                    if !active.is_empty() {
                        failures.push(format!("batch refilled mid-batch at event {}", event.id));
                    }
                    burst = Some((cap.min(queued.len()), 0));
                }
                if let Some((_, launched)) = &mut burst {
                    *launched += 1;
                }
                if !remove(&mut queued, id) {
                    failures.push(format!("call {id} dispatched without being queued"));
                }
                active.insert(id);
                peak = peak.max(active.len());
            }
            "rejected" | "cancelled_before_dispatch" => {
                if !remove(&mut queued, id) {
                    failures.push(format!(
                        "call {id} settled without dispatch but was not queued"
                    ));
                }
            }
            "succeeded" | "provider_failed" | "provider_task_failed" => {
                if !active.remove(&id) {
                    failures.push(format!("call {id} finished without being active"));
                }
            }
            // `unknown` may follow `provider_task_failed`, which already ended activity.
            "unknown" => {
                active.remove(&id);
            }
            "run_finished" if !active.is_empty() || !queued.is_empty() => {
                failures.push("calls were active or queued at run_finished".into());
            }
            _ => {}
        }
        let slot_free = if batch {
            active.is_empty()
        } else {
            active.len() < cap
        };
        if slot_free && !queued.is_empty() {
            obliged_by = Some(event.id);
        }
    }
    if peak > cap {
        failures.push(format!("peak active {peak} exceeds cap {cap}"));
    }
    peak
}

fn remove(queue: &mut Vec<u64>, id: u64) -> bool {
    let position = queue.iter().position(|queued| *queued == id);
    position.map(|index| queue.remove(index)).is_some()
}

// ---------------------------------------------------------------------------
// Per-run metrics

#[derive(Debug, Serialize)]
struct RunMetrics {
    fixture: String,
    policy: Policy,
    /// Measured repetition index, starting at 0 after warmup.
    repetition: usize,
    /// Position of this policy within its repetition's rotation.
    order: usize,
    status: String,
    /// Whole run as reported by the host, including Deno startup.
    elapsed_ms: u64,
    /// Run start to the first admitted call.
    startup_ms: Option<u64>,
    /// Earliest dispatch to the last observed (succeeded or provider_failed)
    /// result; absent when the run hit its deadline or any call is unknown.
    tool_span_ms: Option<u64>,
    /// Per-run sum over all calls of (dispatch, or settlement without dispatch,
    /// minus admission).
    queue_wait_sum_ms: u64,
    /// Most calls simultaneously active, replayed from host events.
    peak_active: usize,
    outcomes: BTreeMap<String, usize>,
    correct: bool,
    failures: Vec<String>,
}

fn measure(
    report: &RunReport,
    fixture: &Fixture,
    policy: Policy,
    repetition: usize,
    order: usize,
) -> RunMetrics {
    let mut failures = Vec::new();
    check_call_events(report, &mut failures);
    let peak_active = replay_dispatch(report, policy, &mut failures);

    let admitted_at: HashMap<u64, u64> = report
        .events
        .iter()
        .filter(|e| e.kind == "admitted")
        .filter_map(|e| Some((e.call_id?, e.at_ms)))
        .collect();
    let queue_wait_sum_ms = report
        .calls
        .iter()
        .filter_map(|record| {
            let left_queue = record.started_ms.or(record.finished_ms)?;
            Some(left_queue.saturating_sub(*admitted_at.get(&record.call.id)?))
        })
        .sum();

    let mut outcomes = BTreeMap::new();
    for record in &report.calls {
        *outcomes
            .entry(outcome_name(&record.outcome).to_owned())
            .or_insert(0) += 1;
    }

    // `started_ms` is taken before the provider future starts, so for observed
    // calls it is a safe lower bound for comparison with the ideal model.
    let any_unknown = outcomes.contains_key("unknown");
    let first_start = report.calls.iter().filter_map(|r| r.started_ms).min();
    let last_observed = report
        .calls
        .iter()
        .filter(|r| {
            matches!(
                r.outcome,
                CallOutcome::Succeeded { .. } | CallOutcome::ProviderFailed { .. }
            )
        })
        .filter_map(|r| r.finished_ms)
        .max();
    let tool_span_ms = if report.status == RunStatus::Deadline || any_unknown {
        None
    } else {
        first_start
            .zip(last_observed)
            .map(|(first, last)| last.saturating_sub(first))
    };

    // The manifest must describe the program that actually ran.
    let observed: Vec<Option<u64>> = report
        .calls
        .iter()
        .map(|record| match record.call.tool.as_str() {
            "mock.wait" => record.call.arguments.get("ms").and_then(Value::as_u64),
            "mock.fail" => Some(0),
            _ => None,
        })
        .collect();
    let expected: Vec<Option<u64>> = fixture.latencies_ms.iter().copied().map(Some).collect();
    if observed != expected {
        failures.push(format!(
            "admitted call latencies {observed:?} differ from manifest {:?}",
            fixture.latencies_ms
        ));
    }

    if fixture.ideal_model {
        let model = ideal_makespan(&fixture.latencies_ms, policy);
        if let Some(span) = tool_span_ms.filter(|span| *span < model) {
            failures.push(format!(
                "tool span {span} ms is below the ideal model {model} ms"
            ));
        }
        // Calls submitted together must all be admitted before a batch forms.
        if policy == Policy::Batch {
            let first_dispatch = report.events.iter().position(|e| e.kind == "dispatched");
            let last_admission = report.events.iter().rposition(|e| e.kind == "admitted");
            if let (Some(dispatch), Some(admission)) = (first_dispatch, last_admission) {
                if admission > dispatch {
                    failures.push("a call was admitted after the first batch formed".into());
                }
            }
        }
    }

    // Reviewed expectations.
    let status = status_name(report.status);
    if status != fixture.expect.status {
        failures.push(format!(
            "status {status}, expected {}",
            fixture.expect.status
        ));
    }
    match &fixture.expect.output {
        Some(OutputExpectation::Absent) if report.output.is_some() => {
            failures.push("output is present, expected absent".into());
        }
        Some(OutputExpectation::Equals(value)) if report.output.as_ref() != Some(value) => {
            failures.push(format!("output {:?} differs from expected", report.output));
        }
        _ => {}
    }
    for (name, count) in &fixture.expect.outcomes {
        let actual = outcomes.get(name).copied().unwrap_or(0);
        if actual != *count {
            failures.push(format!("{actual} {name} calls, expected {count}"));
        }
    }

    RunMetrics {
        fixture: fixture.name.clone(),
        policy,
        repetition,
        order,
        status,
        elapsed_ms: report.elapsed_ms,
        startup_ms: admitted_at.values().min().copied(),
        tool_span_ms,
        queue_wait_sum_ms,
        peak_active,
        outcomes,
        correct: failures.is_empty(),
        failures,
    }
}

// ---------------------------------------------------------------------------
// Aggregation

#[derive(Debug, Serialize)]
struct Stats {
    min: u64,
    p50: u64,
    p90: u64,
    max: u64,
    mean: f64,
}

/// Nearest-rank percentile: the smallest value with at least `p`% of samples at
/// or below it. `sorted` must be non-empty and ascending.
fn nearest_rank(sorted: &[u64], p: usize) -> u64 {
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn stats(values: impl IntoIterator<Item = u64>) -> Option<Stats> {
    let mut sorted: Vec<u64> = values.into_iter().collect();
    sorted.sort_unstable();
    let (&min, &max) = (sorted.first()?, sorted.last()?);
    Some(Stats {
        min,
        p50: nearest_rank(&sorted, 50),
        p90: nearest_rank(&sorted, 90),
        max,
        mean: sorted.iter().sum::<u64>() as f64 / sorted.len() as f64,
    })
}

#[derive(Debug, Serialize)]
struct OutcomeGroup {
    outcomes: BTreeMap<String, usize>,
    runs: usize,
}

#[derive(Debug, Serialize)]
struct Aggregate {
    fixture: String,
    policy: Policy,
    dispatch: DispatchPolicy,
    max_concurrency: usize,
    timeout_ms: u64,
    /// Ideal zero-overhead makespan (model, not measurement), when it applies.
    ideal_model_ms: Option<u64>,
    runs: usize,
    correct_runs: usize,
    elapsed_ms: Option<Stats>,
    tool_span_ms: Option<Stats>,
    startup_ms: Option<Stats>,
    queue_wait_sum_ms: Option<Stats>,
    peak_active_max: usize,
    /// Distinct outcome-count maps and how many runs produced each.
    outcome_distribution: Vec<OutcomeGroup>,
    /// Repetition whose trace was saved: the nearest-rank median by tool span,
    /// then elapsed (elapsed alone when no run has a tool span).
    median_trace_repetition: Option<usize>,
}

/// Aggregate one fixture × policy and pick its representative trace.
fn aggregate<'a>(
    fixture: &Fixture,
    policy: Policy,
    runs: &[(&'a RunMetrics, &'a RunReport)],
) -> (Aggregate, Option<&'a RunReport>) {
    let mut distribution: Vec<OutcomeGroup> = Vec::new();
    for (metrics, _) in runs {
        match distribution
            .iter_mut()
            .find(|group| group.outcomes == metrics.outcomes)
        {
            Some(group) => group.runs += 1,
            None => distribution.push(OutcomeGroup {
                outcomes: metrics.outcomes.clone(),
                runs: 1,
            }),
        }
    }
    distribution.sort_by_key(|group| std::cmp::Reverse(group.runs));

    // Nearest-rank median by tool span, then elapsed (which alone decides when
    // no run has a tool span).
    let mut sorted = runs.to_vec();
    sorted.sort_by_key(|(m, _)| (m.tool_span_ms.unwrap_or(u64::MAX), m.elapsed_ms));
    let median = (!sorted.is_empty()).then(|| sorted[sorted.len().div_ceil(2) - 1]);

    let aggregate = Aggregate {
        fixture: fixture.name.clone(),
        policy,
        dispatch: policy.dispatch(),
        max_concurrency: policy.cap(),
        timeout_ms: fixture.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        ideal_model_ms: fixture
            .ideal_model
            .then(|| ideal_makespan(&fixture.latencies_ms, policy)),
        runs: runs.len(),
        correct_runs: runs.iter().filter(|(metrics, _)| metrics.correct).count(),
        elapsed_ms: stats(runs.iter().map(|(m, _)| m.elapsed_ms)),
        tool_span_ms: stats(runs.iter().filter_map(|(m, _)| m.tool_span_ms)),
        startup_ms: stats(runs.iter().filter_map(|(m, _)| m.startup_ms)),
        queue_wait_sum_ms: stats(runs.iter().map(|(m, _)| m.queue_wait_sum_ms)),
        peak_active_max: runs.iter().map(|(m, _)| m.peak_active).max().unwrap_or(0),
        outcome_distribution: distribution,
        median_trace_repetition: median.map(|(metrics, _)| metrics.repetition),
    };
    (aggregate, median.map(|(_, report)| report))
}

// ---------------------------------------------------------------------------
// Markdown report

fn outcome_text(outcomes: &BTreeMap<String, usize>) -> String {
    let parts: Vec<String> = outcomes
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect();
    parts.join(", ")
}

/// `min | p50 | p90 | max | mean` table cells, or dashes without samples.
fn stats_cells(stats: Option<&Stats>) -> String {
    stats.map_or("— | — | — | — | —".into(), |s| {
        format!(
            "{} | {} | {} | {} | {:.1}",
            s.min, s.p50, s.p90, s.max, s.mean
        )
    })
}

fn render_markdown(
    environment: &Environment,
    reps: usize,
    warmup: usize,
    fixtures: &[Fixture],
    aggregates: &[Aggregate],
    failures: &[&RunMetrics],
) -> String {
    let mut out = String::new();
    let commit = match (&environment.git_commit, environment.git_dirty) {
        (Some(commit), Some(true)) => format!("{commit} (tracked changes uncommitted)"),
        (Some(commit), _) => commit.clone(),
        (None, _) => "unknown".into(),
    };
    let _ = writeln!(out, "# Dispatch policy evaluation\n");
    let _ = writeln!(
        out,
        "- Build: commit {commit}, {} profile, codemode {}.",
        environment.build_profile, environment.codemode_version
    );
    let _ = writeln!(
        out,
        "- Environment: {} {}, {}, {} logical CPUs, {}.",
        environment.os,
        environment.arch,
        environment
            .cpu_model
            .as_deref()
            .unwrap_or("CPU model unknown"),
        environment
            .logical_cpus
            .map_or("unknown".into(), |n| n.to_string()),
        environment.deno_version
    );
    let _ = writeln!(
        out,
        "- Repetitions: {reps} measured per fixture and policy, after {warmup} discarded warmup repetitions. Policies run round-robin within each repetition; the starting policy rotates every repetition."
    );
    let _ = writeln!(
        out,
        "- Shared budget for every policy: timeout {DEFAULT_TIMEOUT_MS} ms unless a fixture overrides it, max_calls {MAX_CALLS}, max_output_bytes {MAX_OUTPUT_BYTES}."
    );
    let overrides: Vec<String> = fixtures
        .iter()
        .filter_map(|f| f.timeout_ms.map(|ms| format!("{} {ms} ms", f.name)))
        .collect();
    if !overrides.is_empty() {
        let _ = writeln!(
            out,
            "- Timeout overrides (same for every policy): {}.",
            overrides.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "- Concurrency cap: sequential 1 (rolling dispatch with one slot), batch {PARALLEL_CAP}, rolling {PARALLEL_CAP}."
    );
    let _ = writeln!(
        out,
        "- *Tool span*: earliest dispatch to the last observed result; shown only for runs that did not hit the deadline and have no unknown calls. *Elapsed*: the whole run, including Deno startup. *Queue wait Σ*: per-run sum over all calls of the time from admission to dispatch, or to settlement for calls never dispatched."
    );
    let _ = writeln!(
        out,
        "- Percentiles use the nearest-rank method. Timestamps are whole milliseconds (floored) and Tokio sleeps overshoot by about 1 ms, so differences of 2 ms or less are not resolvable."
    );
    let _ = writeln!(
        out,
        "- *Model* is an ideal zero-overhead makespan computed from manifest latencies; it is a model, not a measurement, shown only where every call is submitted together and the run does not end early. *p50 − model* includes timer rounding and overshoot, not only host overhead."
    );
    for fixture in fixtures {
        if let Some(note) = &fixture.note {
            let _ = writeln!(out, "- Note on {}: {note}", fixture.name);
        }
    }

    let _ = writeln!(out, "\n## Tool span (ms)\n");
    let _ = writeln!(
        out,
        "| fixture | policy | cap | model | min | p50 | p90 | max | mean | p50 − model |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for a in aggregates {
        let model = a.ideal_model_ms.map_or("—".into(), |ms| ms.to_string());
        let gap = match (&a.tool_span_ms, a.ideal_model_ms) {
            (Some(s), Some(ms)) => format!("{:+}", i128::from(s.p50) - i128::from(ms)),
            _ => "—".into(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {model} | {} | {gap} |",
            a.fixture,
            a.policy.name(),
            a.max_concurrency,
            stats_cells(a.tool_span_ms.as_ref())
        );
    }

    let _ = writeln!(out, "\n## Elapsed (ms)\n");
    let _ = writeln!(
        out,
        "| fixture | policy | timeout | min | p50 | p90 | max | mean | startup p50 / max | queue wait Σ p50 |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for a in aggregates {
        let startup = a
            .startup_ms
            .as_ref()
            .map_or("—".into(), |s| format!("{} / {}", s.p50, s.max));
        let queue = a
            .queue_wait_sum_ms
            .as_ref()
            .map_or("—".into(), |s| s.p50.to_string());
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {startup} | {queue} |",
            a.fixture,
            a.policy.name(),
            a.timeout_ms,
            stats_cells(a.elapsed_ms.as_ref())
        );
    }

    let _ = writeln!(out, "\n## Correctness and outcomes\n");
    let _ = writeln!(
        out,
        "| fixture | policy | correct | peak active | call outcomes (runs) |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---|");
    for a in aggregates {
        let groups: Vec<String> = a
            .outcome_distribution
            .iter()
            .map(|g| format!("{} ({}/{})", outcome_text(&g.outcomes), g.runs, a.runs))
            .collect();
        let _ = writeln!(
            out,
            "| {} | {} | {}/{} | {} | {} |",
            a.fixture,
            a.policy.name(),
            a.correct_runs,
            a.runs,
            a.peak_active_max,
            groups.join("; ")
        );
    }

    if failures.is_empty() {
        let _ = writeln!(out, "\nAll correctness checks passed.");
    } else {
        let _ = writeln!(out, "\n## Correctness failures\n");
        for m in failures {
            let _ = writeln!(
                out,
                "- {} / {} repetition {}: {}",
                m.fixture,
                m.policy.name(),
                m.repetition,
                m.failures.join("; ")
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Environment, arguments, and output files

#[derive(Debug, Serialize)]
struct Environment {
    os: &'static str,
    arch: &'static str,
    cpu_model: Option<String>,
    logical_cpus: Option<usize>,
    codemode_version: &'static str,
    build_profile: &'static str,
    /// Commit of the source checkout this binary was built from, when `git` works.
    git_commit: Option<String>,
    /// Whether tracked files differ from that commit (untracked files ignored).
    git_dirty: Option<bool>,
    deno_version: String,
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

fn environment(deno: &Path) -> Environment {
    let source = env!("CARGO_MANIFEST_DIR");
    let deno_version = Command::new(deno)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.lines().next().map(str::to_owned)
        })
        .unwrap_or_else(|| "deno version unavailable".into());
    Environment {
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
        deno_version,
    }
}

#[derive(Debug, Serialize)]
struct EvalConfig {
    reps: usize,
    warmup: usize,
    manifest: PathBuf,
    default_timeout_ms: u64,
    max_calls: usize,
    max_output_bytes: usize,
    percentiles: &'static str,
    ordering: &'static str,
}

#[derive(Debug, Serialize)]
struct Summary<'a> {
    environment: &'a Environment,
    config: &'a EvalConfig,
    fixtures: &'a [Fixture],
    runs: Vec<&'a RunMetrics>,
    aggregates: &'a [Aggregate],
}

struct Args {
    reps: usize,
    warmup: usize,
    out: Option<PathBuf>,
    deno: PathBuf,
    fixtures: PathBuf,
}

/// Returns `None` when help was requested.
fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        reps: 20,
        warmup: 2,
        out: None,
        deno: PathBuf::from("deno"),
        fixtures: PathBuf::from("eval/fixtures.json"),
    };
    let mut input = std::env::args_os().skip(1);
    while let Some(flag) = input.next() {
        let flag = flag.to_string_lossy().into_owned();
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--reps" | "--warmup" | "--out" | "--deno" | "--fixtures" => {}
            _ => return Err(format!("unknown option '{flag}'; use --help for usage")),
        }
        let value = input
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        let number = || {
            value
                .to_str()
                .and_then(|text| text.parse::<usize>().ok())
                .ok_or_else(|| format!("{flag} requires a nonnegative integer"))
        };
        match flag.as_str() {
            "--reps" => args.reps = number()?,
            "--warmup" => args.warmup = number()?,
            "--out" => args.out = Some(PathBuf::from(value)),
            "--deno" => args.deno = PathBuf::from(value),
            _ => args.fixtures = PathBuf::from(value),
        }
    }
    if args.reps == 0 {
        return Err("--reps must be at least 1".into());
    }
    Ok(Some(args))
}

fn write_outputs(
    dir: &Path,
    summary: &Summary<'_>,
    traces: &[(String, &RunReport)],
) -> Result<(), Box<dyn Error>> {
    let trace_dir = dir.join("traces");
    std::fs::create_dir_all(&trace_dir)?;
    std::fs::write(
        dir.join("summary.json"),
        serde_json::to_string_pretty(summary)? + "\n",
    )?;
    for (name, report) in traces {
        std::fs::write(
            trace_dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(report)? + "\n",
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main loop

#[tokio::main]
async fn main() -> ExitCode {
    match evaluate().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("codemode-eval: {error}");
            ExitCode::from(2)
        }
    }
}

/// Run every fixture under every policy. Exit 0 when all checks pass, 1 when any
/// fails (even if writing results also fails), and 2 on a setup or output error.
async fn evaluate() -> Result<ExitCode, Box<dyn Error>> {
    let Some(args) = parse_args()? else {
        print!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    };
    let (fixtures, sources) = load_manifest(&args.fixtures)?;
    let rounds = args
        .warmup
        .checked_add(args.reps)
        .ok_or("--reps plus --warmup overflows")?;
    let expected_runs = args
        .reps
        .checked_mul(fixtures.len() * POLICIES.len())
        .ok_or("--reps is too large")?;
    let environment = environment(&args.deno);

    // Warmup repetitions run first and are discarded. Within a repetition every
    // fixture runs under every policy, in that repetition's rotated order.
    let mut measured: Vec<(RunMetrics, RunReport)> = Vec::new();
    for round in 0..rounds {
        let repetition = round.checked_sub(args.warmup);
        match repetition {
            None => eprintln!("codemode-eval: warmup {}/{}", round + 1, args.warmup),
            Some(r) => eprintln!("codemode-eval: repetition {}/{}", r + 1, args.reps),
        }
        for (fixture, source) in fixtures.iter().zip(&sources) {
            for (order, policy) in rotation(round).enumerate() {
                let report = run(
                    source,
                    policy.config(fixture, &args.deno),
                    MockProvider,
                    RunCancellation::default(),
                )
                .await
                .map_err(|error| format!("{} / {}: {error}", fixture.name, policy.name()))?;
                if let Some(repetition) = repetition {
                    let metrics = measure(&report, fixture, policy, repetition, order);
                    measured.push((metrics, report));
                }
            }
        }
    }
    if measured.len() != expected_runs {
        return Err(format!("measured {} runs, expected {expected_runs}", measured.len()).into());
    }

    let mut aggregates = Vec::new();
    let mut traces = Vec::new();
    for fixture in &fixtures {
        for policy in POLICIES {
            let runs: Vec<(&RunMetrics, &RunReport)> = measured
                .iter()
                .filter(|(m, _)| m.fixture == fixture.name && m.policy == policy)
                .map(|(m, r)| (m, r))
                .collect();
            let (aggregate, median) = aggregate(fixture, policy, &runs);
            if let Some(report) = median {
                traces.push((format!("{}-{}", fixture.name, policy.name()), report));
            }
            aggregates.push(aggregate);
        }
    }

    let failures: Vec<&RunMetrics> = measured
        .iter()
        .map(|(m, _)| m)
        .filter(|m| !m.correct)
        .collect();
    print!(
        "{}",
        render_markdown(
            &environment,
            args.reps,
            args.warmup,
            &fixtures,
            &aggregates,
            &failures
        )
    );
    let checks = if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };

    if let Some(dir) = &args.out {
        let config = EvalConfig {
            reps: args.reps,
            warmup: args.warmup,
            manifest: args.fixtures.clone(),
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_calls: MAX_CALLS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            percentiles: "nearest-rank",
            ordering: "round-robin policies per repetition; starting policy rotates each repetition",
        };
        let summary = Summary {
            environment: &environment,
            config: &config,
            fixtures: &fixtures,
            runs: measured.iter().map(|(m, _)| m).collect(),
            aggregates: &aggregates,
        };
        if let Err(error) = write_outputs(dir, &summary, &traces) {
            eprintln!("codemode-eval: cannot write {}: {error}", dir.display());
            return Ok(ExitCode::from(if failures.is_empty() { 2 } else { 1 }));
        }
        eprintln!("codemode-eval: wrote {}", dir.display());
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::{Policy, ideal_makespan, nearest_rank};

    #[test]
    fn ideal_model_matches_hand_computed_schedules() {
        let mixed = [200, 25, 25, 25, 200, 25, 25, 25];
        assert_eq!(ideal_makespan(&mixed, Policy::Sequential), 550);
        assert_eq!(ideal_makespan(&mixed, Policy::Batch), 400);
        assert_eq!(ideal_makespan(&mixed, Policy::Rolling), 225);
        let equal = [50; 8];
        assert_eq!(ideal_makespan(&equal, Policy::Batch), 100);
        assert_eq!(ideal_makespan(&equal, Policy::Rolling), 100);
    }

    #[test]
    fn nearest_rank_percentiles() {
        let values: Vec<u64> = (1..=20).collect();
        assert_eq!(nearest_rank(&values, 50), 10);
        assert_eq!(nearest_rank(&values, 90), 18);
        assert_eq!(nearest_rank(&[7], 90), 7);
    }
}
