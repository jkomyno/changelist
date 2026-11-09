//! Bounded answers to three questions about a finished run: why it was slow,
//! which calls ran, and which outcomes need reconciliation.
//!
//! Inspection reads only a [`RunReport`]. Timing uses Rust's own clock; provider
//! facts contribute only their Rust arrival times, never the provider's clock.
//! It never infers that one call depended on another from their order. Output
//! names tools, error codes, argument keys, and sizes; argument values, results,
//! and free-text messages are left out unless [`InspectOptions::messages`] is set.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use crate::{CallOutcome, CallRecord, ExecutionBackend, ProviderFactKind, RunReport, RunStatus};

const MAX_KEYS: usize = 6;
const MAX_KEY_CHARS: usize = 24;
const MAX_NAME_CHARS: usize = 64;
const MAX_CODE_CHARS: usize = 48;
const MAX_MESSAGE_CHARS: usize = 160;

/// How much an inspection may show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectOptions {
    /// Maximum calls listed per section; the rest are counted.
    pub limit: usize,
    /// Include provider error messages and the run diagnostic, which may contain
    /// task data. Off by default.
    pub messages: bool,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            messages: false,
        }
    }
}

/// Answers derived from one report.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    /// The report's schema version.
    pub schema_version: u32,
    /// Execution source, including the historical default of Deno.
    pub backend: ExecutionBackend,
    /// Terminal run status.
    pub status: RunStatus,
    /// Whole run, in Rust milliseconds.
    pub elapsed_ms: u64,
    /// The run diagnostic, only when messages were requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    /// Where the time went.
    pub timing: Timing,
    /// What is known about each call.
    pub calls: Calls,
    /// Calls whose external effect is uncertain.
    pub reconcile: Reconcile,
}

/// Where the run's time went, by exclusive phases that sum to `elapsed_ms`.
#[derive(Debug, Clone, Serialize)]
pub struct Timing {
    /// Phases in a fixed order, including empty ones.
    pub phases: Vec<Phase>,
    /// Sum over calls of admission to dispatch, or to settlement if never dispatched.
    pub queue_wait_ms: u64,
    /// Sum over calls of host-reported permission waits, by Rust arrival times.
    pub permission_wait_ms: u64,
    /// Short conclusions, largest effects first.
    pub findings: Vec<String>,
    /// The longest dispatched calls.
    pub slowest: Vec<CallTiming>,
    /// Dispatched calls not listed in `slowest`.
    pub slowest_omitted: usize,
}

/// One exclusive slice of the run.
#[derive(Debug, Clone, Serialize)]
pub struct Phase {
    /// Stable phase name.
    pub name: &'static str,
    /// Milliseconds spent in this phase.
    pub ms: u64,
    /// What the phase means, and what the trace cannot tell.
    pub meaning: &'static str,
}

/// One dispatched call's time, split at the milestones that were observed.
#[derive(Debug, Clone, Serialize)]
pub struct CallTiming {
    /// Host-assigned call identity.
    pub id: u64,
    /// Tool name, sanitized.
    pub tool: String,
    /// Admission to dispatch.
    pub queued_ms: Option<u64>,
    /// Dispatch to the provider's executor entry, which includes permission waits.
    pub before_executor_ms: Option<u64>,
    /// Host-reported permission request to decision.
    pub permission_ms: Option<u64>,
    /// Executor entry, or dispatch without one, to the observed result or the end.
    pub in_provider_ms: Option<u64>,
    /// Observed result to the reply written to the guest.
    pub delivery_ms: Option<u64>,
}

/// What the report shows about each call.
#[derive(Debug, Clone, Serialize)]
pub struct Calls {
    /// Calls per evidence class.
    pub counts: BTreeMap<Evidence, usize>,
    /// Calls in admission order, up to the limit.
    pub rows: Vec<CallRow>,
    /// Calls not listed.
    pub omitted: usize,
}

/// One call, without argument or result values.
#[derive(Debug, Clone, Serialize)]
pub struct CallRow {
    /// Host-assigned call identity.
    pub id: u64,
    /// Tool name, sanitized.
    pub tool: String,
    /// What is known about whether it ran.
    pub evidence: Evidence,
    /// Error code from Rust or the provider, sanitized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Error message, only when messages were requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Top-level argument keys, or the argument's JSON type.
    pub arguments: String,
    /// Serialized argument size.
    pub argument_bytes: u64,
    /// Serialized size of the observed value or error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<u64>,
}

/// What a report can show about whether a call ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The provider returned a value.
    Completed,
    /// The provider reported executor entry and then an error.
    FailedAfterExecutorStart,
    /// The provider reported a permission denial and an error.
    DeniedByProvider,
    /// The provider returned an error without saying whether the tool started.
    Failed,
    /// A value was observed but too large to keep or deliver.
    ResultOmitted,
    /// A provider error was observed but too large to keep or deliver.
    ErrorOmitted,
    /// Rust refused the call before dispatch.
    Rejected,
    /// The run ended before the call was dispatched.
    NotDispatched,
    /// No result; the provider reported executor entry.
    UnknownAfterExecutorStart,
    /// No result; the provider acknowledged the call but reported no executor entry.
    UnknownAfterReceipt,
    /// No result and no provider acknowledgment, which does not show it never ran.
    UnknownUnacknowledged,
    /// Queued or running; the report was taken mid-run.
    InProgress,
}

impl Evidence {
    /// Plain answer to "did it run?".
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Completed => "ran: result observed",
            Self::FailedAfterExecutorStart => {
                "ran: provider reported executor entry, then an error"
            }
            Self::DeniedByProvider => "did not run: provider reported a permission denial",
            Self::Failed => "provider returned an error; whether the tool started is not reported",
            Self::ResultOmitted => "ran: value observed but omitted as too large",
            Self::ErrorOmitted => {
                "provider error observed but omitted as too large; whether the tool started is not reported"
            }
            Self::Rejected => "did not run: refused by Rust before dispatch",
            Self::NotDispatched => "did not run: never dispatched",
            Self::UnknownAfterExecutorStart => {
                "may have run: provider reported executor entry, no result observed"
            }
            Self::UnknownAfterReceipt => {
                "may have run: provider acknowledged it, no executor entry or result reported"
            }
            Self::UnknownUnacknowledged => {
                "may have run: dispatched, no acknowledgment or result (missing evidence is not proof it did not run)"
            }
            Self::InProgress => "in progress when the report was taken",
        }
    }

    /// A short label for one call's row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "ran",
            Self::FailedAfterExecutorStart => "ran, failed",
            Self::DeniedByProvider => "denied by provider",
            Self::Failed => "provider error",
            Self::ResultOmitted => "ran, value omitted",
            Self::ErrorOmitted => "provider error, omitted",
            Self::Rejected => "rejected by Rust",
            Self::NotDispatched => "not dispatched",
            Self::UnknownAfterExecutorStart => "UNKNOWN, executor started",
            Self::UnknownAfterReceipt => "UNKNOWN, received",
            Self::UnknownUnacknowledged => "UNKNOWN, no acknowledgment",
            Self::InProgress => "in progress",
        }
    }

    fn uncertain(self) -> bool {
        matches!(
            self,
            Self::UnknownAfterExecutorStart
                | Self::UnknownAfterReceipt
                | Self::UnknownUnacknowledged
        )
    }
}

/// Calls whose effects must be checked with the provider before any retry.
#[derive(Debug, Clone, Serialize)]
pub struct Reconcile {
    /// Uncertain calls, up to the limit.
    pub items: Vec<ReconcileItem>,
    /// Uncertain calls not listed.
    pub omitted: usize,
}

/// One call to reconcile.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileItem {
    /// Host-assigned call identity.
    pub id: u64,
    /// Tool name, sanitized.
    pub tool: String,
    /// The strongest evidence available.
    pub evidence: Evidence,
    /// Why no result was observed: the run status that ended it, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Dispatch time, Rust milliseconds since run start.
    pub dispatched_ms: Option<u64>,
    /// The last provider fact and its Rust arrival time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fact: Option<(ProviderFactKind, u64)>,
    /// Top-level argument keys, or the argument's JSON type, to match against the provider.
    pub arguments: String,
    /// Serialized argument size.
    pub argument_bytes: u64,
}

/// Which sections [`Inspection::render`] prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sections {
    /// "Why was this slow?"
    pub timing: bool,
    /// "Which calls ran?"
    pub calls: bool,
    /// "Which outcomes require reconciliation?"
    pub reconcile: bool,
}

impl Sections {
    /// Every section.
    pub const ALL: Self = Self {
        timing: true,
        calls: true,
        reconcile: true,
    };
}

/// Answer the three questions for `report`, within `options`.
///
/// # Examples
///
/// ```
/// use codemode::{InspectOptions, RunReport, Sections, inspect};
/// let report: RunReport = serde_json::from_str(r#"{
///   "schema_version": 2, "code": "return 1;", "status": "success",
///   "limits": {"timeout_ms": 5000, "max_calls": 32, "max_concurrency": 4,
///              "max_output_bytes": 262144, "dispatch": "rolling"},
///   "output": 1, "calls": [], "events": [], "elapsed_ms": 40, "diagnostic": null
/// }"#)?;
/// let text = inspect(&report, &InspectOptions::default()).render(Sections::ALL);
/// assert!(text.contains("status success"));
/// # Ok::<(), serde_json::Error>(())
/// ```
#[must_use]
pub fn inspect(report: &RunReport, options: &InspectOptions) -> Inspection {
    let evidence: Vec<Evidence> = report
        .calls
        .iter()
        .map(|record| classify(report, record))
        .collect();
    Inspection {
        schema_version: report.schema_version,
        backend: report.backend,
        status: report.status,
        elapsed_ms: report.elapsed_ms,
        diagnostic: report
            .diagnostic
            .as_deref()
            .filter(|_| options.messages)
            .map(|text| sanitize(text, MAX_MESSAGE_CHARS)),
        timing: timing(report, options),
        calls: calls(report, &evidence, options),
        reconcile: reconcile(report, &evidence, options),
    }
}

/// Admission time, from the call record or, in schema 1 reports, its event.
fn admitted(report: &RunReport, record: &CallRecord) -> Option<u64> {
    record.admitted_ms.or_else(|| {
        report
            .events
            .iter()
            .find(|event| event.call_id == Some(record.call.id) && event.kind == "admitted")
            .map(|event| event.at_ms)
    })
}

fn facts(report: &RunReport, call_id: u64) -> impl Iterator<Item = (ProviderFactKind, u64)> + '_ {
    report
        .provider_facts
        .iter()
        .filter(move |fact| fact.call_id == call_id)
        .map(|fact| (fact.kind, fact.received_ms))
}

fn fact_at(report: &RunReport, call_id: u64, kind: ProviderFactKind) -> Option<u64> {
    facts(report, call_id)
        .find(|(found, _)| *found == kind)
        .map(|(_, at)| at)
}

fn classify(report: &RunReport, record: &CallRecord) -> Evidence {
    let id = record.call.id;
    let has = |kind| fact_at(report, id, kind).is_some();
    match &record.outcome {
        CallOutcome::Succeeded { .. } => Evidence::Completed,
        CallOutcome::ProviderFailed { .. } if has(ProviderFactKind::ExecutorStarted) => {
            Evidence::FailedAfterExecutorStart
        }
        CallOutcome::ProviderFailed { .. } if has(ProviderFactKind::PermissionDenied) => {
            Evidence::DeniedByProvider
        }
        CallOutcome::ProviderFailed { .. } => Evidence::Failed,
        // The observed result's event says whether it was a value or an error.
        CallOutcome::ResultTooLarge
            if report
                .events
                .iter()
                .any(|event| event.call_id == Some(id) && event.kind == "provider_failed") =>
        {
            Evidence::ErrorOmitted
        }
        CallOutcome::ResultTooLarge => Evidence::ResultOmitted,
        CallOutcome::Rejected { .. } => Evidence::Rejected,
        CallOutcome::CancelledBeforeDispatch => Evidence::NotDispatched,
        CallOutcome::Unknown if has(ProviderFactKind::ExecutorStarted) => {
            Evidence::UnknownAfterExecutorStart
        }
        CallOutcome::Unknown if facts(report, id).next().is_some() => Evidence::UnknownAfterReceipt,
        CallOutcome::Unknown => Evidence::UnknownUnacknowledged,
        CallOutcome::Queued | CallOutcome::Running => Evidence::InProgress,
    }
}

const PHASES: [(&str, &str); 7] = [
    (
        "startup",
        "before the guest's first frame: process start, guest compilation, and its first computation",
    ),
    (
        "guest",
        "no call queued or running: guest computation, reply handling, or the gap between dependent steps; the trace cannot tell which",
    ),
    ("tools", "calls running and none waiting to start"),
    (
        "queued_at_cap",
        "calls waiting because every concurrency slot was busy",
    ),
    (
        "queued_by_policy",
        "calls waiting while a slot was free: a batch barrier, or dispatch latency",
    ),
    (
        "draining",
        "the guest had finished; Rust was collecting calls it had already dispatched",
    ),
    (
        "shutdown",
        "after the guest finished or the run stopped: terminating the guest and finishing the report",
    ),
];

fn run_event(report: &RunReport, kinds: &[&str]) -> Option<u64> {
    report
        .events
        .iter()
        .find(|event| event.call_id.is_none() && kinds.contains(&event.kind.as_str()))
        .map(|event| event.at_ms)
}

/// One call's state changes, in Rust milliseconds since run start.
struct Span {
    admitted: Option<u64>,
    left_queue: Option<u64>,
    started: Option<u64>,
    finished: Option<u64>,
}

fn timing(report: &RunReport, options: &InspectOptions) -> Timing {
    let end = report.elapsed_ms;
    let guest_finished = run_event(report, &["guest_finished"]);
    let native = report.backend == ExecutionBackend::Native;
    let stop_events: &[&str] = if native {
        &[
            "deadline_reached",
            "cancellation_observed",
            "execution_finished",
        ]
    } else {
        &["deadline_reached", "cancellation_observed"]
    };
    let stopped = run_event(report, stop_events);
    let spans: Vec<Span> = report
        .calls
        .iter()
        .map(|r| Span {
            admitted: admitted(report, r),
            left_queue: r.started_ms.or(r.finished_ms),
            started: r.started_ms,
            finished: r.finished_ms,
        })
        .collect();
    let first_frame = if native {
        0
    } else {
        spans
            .iter()
            .filter_map(|span| span.admitted)
            .chain(guest_finished)
            .chain(stopped)
            .min()
            .unwrap_or(end)
            .min(end)
    };
    let cap = report.limits.max_concurrency;
    let mut points: Vec<u64> = vec![0, first_frame, end];
    points.extend(guest_finished);
    points.extend(stopped);
    for span in &spans {
        points.extend(
            [span.admitted, span.started, span.finished]
                .into_iter()
                .flatten(),
        );
    }
    points.retain(|at| *at <= end);
    points.sort_unstable();
    points.dedup();
    let mut ms = [0u64; 7];
    for window in points.windows(2) {
        let (at, next) = (window[0], window[1]);
        let queued = spans
            .iter()
            .filter(|span| {
                span.admitted.is_some_and(|a| a <= at)
                    && span.left_queue.is_none_or(|left| at < left)
            })
            .count();
        let active = spans
            .iter()
            .filter(|span| {
                span.started.is_some_and(|s| s <= at) && span.finished.is_none_or(|f| at < f)
            })
            .count();
        let done = guest_finished.is_some_and(|g| g <= at) || stopped.is_some_and(|s| s <= at);
        let phase = if at < first_frame {
            0
        } else if done {
            if active > 0 && stopped.is_none_or(|s| at < s) {
                5
            } else {
                6
            }
        } else if queued > 0 && active >= cap {
            3
        } else if queued > 0 {
            4
        } else if active > 0 {
            2
        } else {
            1
        };
        ms[phase] = ms[phase].saturating_add(next - at);
    }
    let phases: Vec<Phase> = if native {
        [
            ("scheduler", ms[0].saturating_add(ms[1]), "host admission and scheduling with no provider call active"),
            ("tools", ms[2], "provider calls running and none waiting to start"),
            ("queued_at_cap", ms[3], "calls waiting because every concurrency slot was busy"),
            ("queued_by_policy", ms[4], "calls waiting while a slot was free: a batch barrier, or dispatch latency"),
            ("shutdown", ms[5].saturating_add(ms[6]), "after native execution stopped: settling calls, stopping provider tasks, and finishing the report"),
        ].into_iter().map(|(name, ms, meaning)| Phase { name, ms, meaning }).collect()
    } else {
        PHASES
            .iter()
            .zip(ms)
            .map(|((name, meaning), ms)| Phase { name, ms, meaning })
            .collect()
    };

    let mut slowest: Vec<CallTiming> = report
        .calls
        .iter()
        .filter(|r| r.started_ms.is_some())
        .map(|r| call_timing(report, r))
        .collect();
    let span = |t: &CallTiming| {
        t.in_provider_ms
            .unwrap_or(0)
            .saturating_add(t.before_executor_ms.unwrap_or(0))
    };
    slowest.sort_by(|a, b| span(b).cmp(&span(a)).then(a.id.cmp(&b.id)));
    let slowest_omitted = slowest.len().saturating_sub(options.limit);
    slowest.truncate(options.limit);

    let queue_wait_ms = report
        .calls
        .iter()
        .filter_map(|r| {
            Some(
                r.started_ms
                    .or(r.finished_ms)?
                    .saturating_sub(admitted(report, r)?),
            )
        })
        .fold(0, u64::saturating_add);
    let permission_wait_ms = report
        .calls
        .iter()
        .filter_map(|r| permission_ms(report, r))
        .fold(0, u64::saturating_add);
    let findings = findings(
        report,
        &phases,
        queue_wait_ms,
        permission_wait_ms,
        slowest.first(),
    );
    Timing {
        phases,
        queue_wait_ms,
        permission_wait_ms,
        findings,
        slowest,
        slowest_omitted,
    }
}

fn permission_ms(report: &RunReport, record: &CallRecord) -> Option<u64> {
    let id = record.call.id;
    let asked = fact_at(report, id, ProviderFactKind::PermissionRequested)?;
    let decided = fact_at(report, id, ProviderFactKind::PermissionGranted)
        .or_else(|| fact_at(report, id, ProviderFactKind::PermissionDenied))
        .or(record.finished_ms)?;
    Some(decided.saturating_sub(asked))
}

fn call_timing(report: &RunReport, record: &CallRecord) -> CallTiming {
    let started = record.started_ms;
    let executor = fact_at(report, record.call.id, ProviderFactKind::ExecutorStarted);
    let observed = matches!(
        record.outcome,
        CallOutcome::Succeeded { .. }
            | CallOutcome::ProviderFailed { .. }
            | CallOutcome::ResultTooLarge
    );
    CallTiming {
        id: record.call.id,
        tool: sanitize(&record.call.tool, MAX_NAME_CHARS),
        queued_ms: started
            .zip(admitted(report, record))
            .map(|(s, a)| s.saturating_sub(a)),
        before_executor_ms: executor.zip(started).map(|(e, s)| e.saturating_sub(s)),
        permission_ms: permission_ms(report, record),
        in_provider_ms: record
            .finished_ms
            .zip(executor.or(started))
            .map(|(f, s)| f.saturating_sub(s)),
        delivery_ms: record
            .delivered_ms
            .zip(record.finished_ms)
            .filter(|_| observed)
            .map(|(d, f)| d.saturating_sub(f)),
    }
}

fn percent(part: u64, whole: u64) -> u128 {
    (u128::from(part) * 100)
        .checked_div(u128::from(whole))
        .unwrap_or(0)
}

fn findings(
    report: &RunReport,
    phases: &[Phase],
    queue_wait_ms: u64,
    permission_wait_ms: u64,
    slowest: Option<&CallTiming>,
) -> Vec<String> {
    let total = report.elapsed_ms;
    let mut ranked: Vec<&Phase> = phases.iter().filter(|phase| phase.ms > 0).collect();
    ranked.sort_by_key(|phase| std::cmp::Reverse(phase.ms));
    let mut out: Vec<String> = ranked
        .iter()
        .take(2)
        .map(|phase| {
            format!(
                "{}% of the run ({} ms) was {}: {}.",
                percent(phase.ms, total),
                phase.ms,
                phase.name,
                phase.meaning
            )
        })
        .collect();
    let at_cap = phases
        .iter()
        .find(|p| p.name == "queued_at_cap")
        .map_or(0, |p| p.ms);
    if at_cap > 0 {
        out.push(format!(
            "Calls waited behind the concurrency cap of {} for {at_cap} ms of the run; queue waits summed over calls are {queue_wait_ms} ms.",
            report.limits.max_concurrency
        ));
    }
    if permission_wait_ms > 0 {
        out.push(format!(
            "The provider reported {permission_wait_ms} ms of permission waits, summed over calls and timed by their arrival in Rust."
        ));
    }
    if let Some(call) = slowest {
        out.push(format!(
            "Longest call: #{} {} spent {} ms in the provider.",
            call.id,
            call.tool,
            call.in_provider_ms
                .unwrap_or(0)
                .saturating_add(call.before_executor_ms.unwrap_or(0))
        ));
    }
    out
}

fn calls(report: &RunReport, evidence: &[Evidence], options: &InspectOptions) -> Calls {
    let mut counts = BTreeMap::new();
    for kind in evidence {
        *counts.entry(*kind).or_insert(0) += 1;
    }
    let rows = report
        .calls
        .iter()
        .zip(evidence)
        .take(options.limit)
        .map(|(record, evidence)| {
            let error = match &record.outcome {
                CallOutcome::ProviderFailed { error } | CallOutcome::Rejected { error } => {
                    Some(error)
                }
                _ => None,
            };
            CallRow {
                id: record.call.id,
                tool: sanitize(&record.call.tool, MAX_NAME_CHARS),
                evidence: *evidence,
                error_code: error.map(|error| sanitize(&error.code, MAX_CODE_CHARS)),
                message: error
                    .filter(|_| options.messages)
                    .map(|error| sanitize(&error.message, MAX_MESSAGE_CHARS)),
                arguments: shape(&record.call.arguments),
                argument_bytes: record.argument_bytes,
                result_bytes: record.result_bytes,
            }
        })
        .collect();
    Calls {
        counts,
        rows,
        omitted: report.calls.len().saturating_sub(options.limit),
    }
}

fn reconcile(report: &RunReport, evidence: &[Evidence], options: &InspectOptions) -> Reconcile {
    let uncertain: Vec<(&CallRecord, Evidence)> = report
        .calls
        .iter()
        .zip(evidence.iter().copied())
        .filter(|(_, evidence)| evidence.uncertain())
        .collect();
    let items = uncertain
        .iter()
        .take(options.limit)
        .map(|(record, evidence)| ReconcileItem {
            id: record.call.id,
            tool: sanitize(&record.call.tool, MAX_NAME_CHARS),
            evidence: *evidence,
            reason: report
                .events
                .iter()
                .rfind(|event| event.call_id == Some(record.call.id) && event.kind == "unknown")
                .and_then(|event| event.reason.as_deref())
                .map(|reason| sanitize(reason, MAX_CODE_CHARS)),
            dispatched_ms: record.started_ms,
            last_fact: facts(report, record.call.id).last(),
            arguments: shape(&record.call.arguments),
            argument_bytes: record.argument_bytes,
        })
        .collect();
    Reconcile {
        items,
        omitted: uncertain.len().saturating_sub(options.limit),
    }
}

/// Top-level keys of an object argument, or the JSON type of anything else.
fn shape(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<String> = map
                .keys()
                .take(MAX_KEYS)
                .map(|k| sanitize(k, MAX_KEY_CHARS))
                .collect();
            if map.len() > MAX_KEYS {
                keys.push(format!("+{} more", map.len() - MAX_KEYS));
            }
            format!("{{{}}}", keys.join(", "))
        }
        Value::Array(items) => format!("array[{}]", items.len()),
        Value::String(_) => "string".into(),
        Value::Number(_) => "number".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Null => "null".into(),
    }
}

/// One line of at most `max` characters with control characters replaced.
pub(crate) fn sanitize(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

impl Inspection {
    /// Plain text with at most `limit` calls per section and counted omissions.
    #[must_use]
    pub fn render(&self, sections: Sections) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "status {} in {} ms (report schema {}, Rust monotonic ms since run start)",
            self.status.as_str(),
            self.elapsed_ms,
            self.schema_version
        );
        let backend = match self.backend {
            ExecutionBackend::Native => "native",
            ExecutionBackend::Deno => "deno",
            ExecutionBackend::EmbeddedV8 => "embedded_v8",
        };
        let _ = writeln!(out, "backend {backend}");
        if self.schema_version < 2 {
            let _ = writeln!(
                out,
                "schema 1 report: no delivery times, byte counts, run-level events, or provider facts; startup and shutdown cannot be separated"
            );
        }
        if let Some(diagnostic) = &self.diagnostic {
            let _ = writeln!(out, "diagnostic: {diagnostic}");
        }
        if sections.timing {
            let timing = &self.timing;
            let _ = writeln!(out, "\nWhy was this slow?");
            for finding in &timing.findings {
                let _ = writeln!(out, "  - {finding}");
            }
            let phases: Vec<String> = timing
                .phases
                .iter()
                .map(|phase| format!("{} {}", phase.name, phase.ms))
                .collect();
            let _ = writeln!(out, "  phases (ms): {}", phases.join(", "));
            for call in &timing.slowest {
                let part = |label: &str, value: Option<u64>| {
                    value.map(|ms| format!(" {label} {ms}")).unwrap_or_default()
                };
                let _ = writeln!(
                    out,
                    "  #{} {}:{}{}{}{}{}",
                    call.id,
                    call.tool,
                    part("queued", call.queued_ms),
                    part("before-executor", call.before_executor_ms),
                    part("permission", call.permission_ms),
                    part("in-provider", call.in_provider_ms),
                    part("delivery", call.delivery_ms),
                );
            }
            if timing.slowest_omitted > 0 {
                let _ = writeln!(out, "  … {} more dispatched calls", timing.slowest_omitted);
            }
        }
        if sections.calls {
            let _ = writeln!(out, "\nWhich calls ran?");
            for (evidence, count) in &self.calls.counts {
                let _ = writeln!(out, "  {count} × {}", evidence.describe());
            }
            for row in &self.calls.rows {
                let code = row
                    .error_code
                    .as_deref()
                    .map(|c| format!(" [{c}]"))
                    .unwrap_or_default();
                let message = row
                    .message
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default();
                let result = row
                    .result_bytes
                    .map(|b| format!(", result {b} B"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "  #{} {} {} — {}{code}{message} (args {} B{result})",
                    row.id,
                    row.tool,
                    row.arguments,
                    row.evidence.label(),
                    row.argument_bytes,
                );
            }
            if self.calls.omitted > 0 {
                let _ = writeln!(out, "  … {} more calls", self.calls.omitted);
            }
        }
        if sections.reconcile {
            let _ = writeln!(out, "\nWhich outcomes require reconciliation?");
            if self.reconcile.items.is_empty() && self.reconcile.omitted == 0 {
                let _ = writeln!(out, "  none: every dispatched call has an observed outcome");
            } else {
                let _ = writeln!(
                    out,
                    "  These calls were dispatched without an observed result. Check the provider for their effects before retrying them."
                );
            }
            for item in &self.reconcile.items {
                let fact = item
                    .last_fact
                    .map(|(kind, at)| format!("; last host fact {} at {at} ms", fact_name(kind)))
                    .unwrap_or_default();
                let reason = item
                    .reason
                    .as_deref()
                    .map(|r| format!("; ended by {r}"))
                    .unwrap_or_default();
                let dispatched = item
                    .dispatched_ms
                    .map_or_else(String::new, |at| format!("; dispatched at {at} ms"));
                let _ = writeln!(
                    out,
                    "  #{} {} {} — {}{reason}{dispatched}{fact}",
                    item.id,
                    item.tool,
                    item.arguments,
                    item.evidence.label(),
                );
            }
            if self.reconcile.omitted > 0 {
                let _ = writeln!(out, "  … {} more uncertain calls", self.reconcile.omitted);
            }
        }
        out
    }
}

fn fact_name(kind: ProviderFactKind) -> &'static str {
    match kind {
        ProviderFactKind::Received => "received",
        ProviderFactKind::PermissionRequested => "permission_requested",
        ProviderFactKind::PermissionGranted => "permission_granted",
        ProviderFactKind::PermissionDenied => "permission_denied",
        ProviderFactKind::ExecutorStarted => "executor_started",
    }
}
