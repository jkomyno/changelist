//! Answer five questions from a journal: what was approved, what was attempted
//! and observed, what changed or is satisfied, what remains uncertain, and what
//! can safely happen next.
//!
//! Inspection prints tools, argument keys, error codes, verdicts, the approved
//! retention policy, and probe evidence the policy kept. It leaves out argument
//! values, error messages, and notes unless [`JournalInspectOptions::messages`] is
//! set; the journal holds no result values or provider prose it did not keep, so
//! `messages` cannot reveal them. In a journal with an approval that predates
//! retention policies, older facts may hold provider text, so probe evidence is
//! also printed only with `messages`.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use serde::Serialize;

use crate::executor::assess_history;
use crate::inspect::sanitize;
use crate::journal::{History, Journal, JournalError};
use crate::plan::Digest;
use crate::recovery::{
    Approval, Basis, Conclusion, FailureEffect, NextStep, Observed, Resolution, Retention,
    machine_code,
};

/// Characters of a message, evidence, or note printed before truncation.
const MAX_TEXT_CHARS: usize = 300;

/// What [`inspect_journal`] includes.
#[derive(Debug, Clone, Copy, Default)]
pub struct JournalInspectOptions {
    /// Include error messages, review and approval notes, and evidence recorded
    /// before retention policies, as far as the journal kept them.
    pub messages: bool,
}

/// A journal's answers, serializable as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct JournalInspection {
    /// The journal file.
    pub journal: String,
    /// The plan.
    pub plan: PlanSummary,
    /// The approval in force, if any.
    pub approval: Option<ApprovalSummary>,
    /// Sessions, oldest first.
    pub sessions: Vec<SessionSummary>,
    /// Operations in plan order.
    pub operations: Vec<OperationReport>,
    /// Operations per conclusion.
    pub conclusions: BTreeMap<String, usize>,
    /// Operations per next step.
    pub next: BTreeMap<String, usize>,
}

/// The plan's identity and shape.
#[derive(Debug, Clone, Serialize)]
pub struct PlanSummary {
    /// Plan digest.
    pub digest: Digest,
    /// The plan's summary line.
    pub summary: Option<String>,
    /// Operations per tool.
    pub tools: BTreeMap<String, usize>,
    /// Distinct scopes.
    pub scopes: Vec<String>,
}

/// The approval in force.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalSummary {
    /// The full approval record, including contracts and policy.
    #[serde(flatten)]
    pub approval: Approval,
    /// How many approvals were recorded for this plan.
    pub approvals: usize,
    /// Whether the approval in force binds its retention policy; false for an
    /// approval that predates retention policies, whose `retention` field is only a default.
    pub retention_bound: bool,
    /// Approvals recorded before approvals bound a retention policy. Facts from
    /// their sessions may hold provider values and messages.
    pub approvals_without_retention: usize,
}

/// One session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// Session number.
    pub session: u64,
    /// `run` or `resume`.
    pub kind: String,
    /// Executor process id.
    pub pid: u32,
    /// Start, Unix milliseconds.
    pub started_unix_ms: u64,
    /// The recorded end status, or `unfinished` when the executor stopped first.
    pub status: String,
    /// Explanation, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the dispatch run's execution record is stored.
    pub trace: bool,
    /// Attempts dispatched in this session.
    pub attempts: usize,
    /// Probes made in this session.
    pub probes: usize,
}

/// One operation's history and status.
#[derive(Debug, Clone, Serialize)]
pub struct OperationReport {
    /// Stable identity.
    pub operation: String,
    /// Provider tool.
    pub tool: String,
    /// Scope.
    pub scope: String,
    /// Top-level argument keys; values are never printed.
    pub argument_keys: Vec<String>,
    /// Number of preconditions.
    pub preconditions: usize,
    /// What is known.
    pub conclusion: Conclusion,
    /// How it is known.
    pub basis: Basis,
    /// Next safe step.
    pub next: NextStep,
    /// Why.
    pub reason: String,
    /// Attempts, oldest first.
    pub attempts: Vec<AttemptLine>,
    /// Probes, oldest first.
    pub probes: Vec<ProbeLine>,
    /// Reviews, oldest first.
    pub reviews: Vec<ReviewLine>,
}

/// One attempt.
#[derive(Debug, Clone, Serialize)]
pub struct AttemptLine {
    /// Journal fact ordinal, for ordering.
    pub ordinal: u64,
    /// Attempt number.
    pub number: u32,
    /// Session that dispatched it.
    pub session: u64,
    /// Call identity in that session's execution record.
    pub call_id: Option<u64>,
    /// Attempt key given to the provider.
    pub key: String,
    /// `succeeded`, `failed`, `timed_out`, `unobserved`, `withdrawn`, or `no result`.
    pub observed: String,
    /// Error code for a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The provider's statement about the effect, or why it is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// Result size for a success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<u64>,
    /// Error message, only with `messages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One reconciliation probe.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeLine {
    /// Journal fact ordinal, for ordering.
    pub ordinal: u64,
    /// Session that probed.
    pub session: u64,
    /// Attempt keys probed.
    pub keys: usize,
    /// The verdict, or `error`.
    pub verdict: String,
    /// The provider's evidence as kept (empty when the retention policy dropped
    /// it), or the error code; absent when hidden without `messages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Probe duration.
    pub elapsed_ms: u64,
}

/// One review.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewLine {
    /// Journal fact ordinal, for ordering.
    pub ordinal: u64,
    /// The finding.
    pub resolution: Resolution,
    /// Who.
    pub reviewer: String,
    /// When, Unix milliseconds.
    pub unix_ms: u64,
    /// Note, only with `messages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Read and answer a journal.
///
/// # Errors
/// Returns [`JournalError`] if the history cannot be read.
pub fn inspect_journal(
    journal: &Journal,
    options: &JournalInspectOptions,
) -> Result<JournalInspection, JournalError> {
    let history = journal.history()?;
    Ok(build(journal, &history, options))
}

fn build(
    journal: &Journal,
    history: &History,
    options: &JournalInspectOptions,
) -> JournalInspection {
    let plan = journal.plan();
    let mut tools = BTreeMap::new();
    let mut scopes = Vec::new();
    for operation in &plan.plan().operations {
        *tools.entry(operation.tool.clone()).or_insert(0) += 1;
        let scope = operation.scope.to_string();
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    let sessions = history
        .sessions
        .iter()
        .map(|session| SessionSummary {
            session: session.session,
            kind: session.kind.clone(),
            pid: session.pid,
            started_unix_ms: session.started_unix_ms,
            status: session
                .end
                .as_ref()
                .map_or_else(|| "unfinished".into(), |end| end.status.clone()),
            detail: session.end.as_ref().and_then(|end| end.detail.clone()),
            trace: session.end.as_ref().is_some_and(|end| end.has_trace),
            attempts: history
                .attempts
                .iter()
                .filter(|attempt| attempt.session == session.session)
                .count(),
            probes: history
                .probes
                .iter()
                .filter(|probe| probe.session == session.session)
                .count(),
        })
        .collect();
    let without_retention = history
        .approvals
        .iter()
        .filter(|approval| approval.predates_retention())
        .count();
    // Evidence kept under an explicit retention policy is the provider's declared
    // projection; older evidence was never checked against one.
    let show_evidence = options.messages || without_retention == 0;
    let statuses = assess_history(plan, history, &HashSet::new());
    let mut conclusions = BTreeMap::new();
    let mut next = BTreeMap::new();
    let operations = plan
        .plan()
        .operations
        .iter()
        .zip(statuses)
        .map(|(operation, status)| {
            let assessment = status.assessment;
            *conclusions
                .entry(assessment.conclusion.as_str().to_string())
                .or_insert(0) += 1;
            *next
                .entry(assessment.next.as_str().to_string())
                .or_insert(0) += 1;
            let attempts = history
                .attempts
                .iter()
                .filter(|attempt| attempt.operation == operation.id)
                .map(|attempt| {
                    let mut line = AttemptLine {
                        ordinal: attempt.ordinal,
                        number: attempt.number,
                        session: attempt.session,
                        call_id: attempt.call_id,
                        key: attempt.key.clone(),
                        observed: "no result".into(),
                        code: None,
                        effect: Some(
                            "unknown: no result recorded, so it may have reached the provider"
                                .into(),
                        ),
                        result_bytes: None,
                        message: None,
                    };
                    let message = |text: &str| {
                        (options.messages && !text.is_empty()).then(|| text.to_string())
                    };
                    if let Some(observation) = &attempt.observation {
                        line.effect = None;
                        match &observation.observed {
                            Observed::Succeeded { bytes, .. } => {
                                line.observed = "succeeded".into();
                                line.result_bytes = Some(*bytes);
                            }
                            Observed::Failed { error, effect } => {
                                line.observed = "failed".into();
                                line.code = Some(machine_code(&error.code).to_string());
                                line.effect = Some(
                                    match effect {
                                        FailureEffect::Refused => "refused",
                                        FailureEffect::NotApplied => "not_applied",
                                        FailureEffect::Unknown => "unknown",
                                    }
                                    .into(),
                                );
                                line.message = message(&error.message);
                            }
                            Observed::TimedOut => {
                                line.observed = "timed_out".into();
                                line.effect = Some("unknown".into());
                            }
                            Observed::Unobserved { reason } => {
                                line.observed = "unobserved".into();
                                line.effect = Some(format!("unknown (run ended: {reason})"));
                            }
                            Observed::Withdrawn => {
                                line.observed = "withdrawn".into();
                                line.effect = Some("not_applied (never sent)".into());
                            }
                        }
                    }
                    line
                })
                .collect();
            let probes = history
                .probes
                .iter()
                .filter(|probe| probe.operation == operation.id)
                .map(|probe| match &probe.outcome {
                    Ok(reconciliation) => ProbeLine {
                        ordinal: probe.ordinal,
                        session: probe.session,
                        keys: probe.keys.len(),
                        verdict: reconciliation.verdict.to_string(),
                        evidence: show_evidence.then(|| reconciliation.evidence.clone()),
                        elapsed_ms: probe.elapsed_ms,
                    },
                    Err(error) => ProbeLine {
                        ordinal: probe.ordinal,
                        session: probe.session,
                        keys: probe.keys.len(),
                        verdict: "error".into(),
                        evidence: Some(machine_code(&error.code).to_string()),
                        elapsed_ms: probe.elapsed_ms,
                    },
                })
                .collect();
            let reviews = history
                .reviews
                .iter()
                .filter(|review| review.operation == operation.id)
                .map(|review| ReviewLine {
                    ordinal: review.ordinal,
                    resolution: review.resolution,
                    reviewer: review.reviewer.clone(),
                    unix_ms: review.unix_ms,
                    note: review.note.clone().filter(|_| options.messages),
                })
                .collect();
            OperationReport {
                operation: operation.id.clone(),
                tool: operation.tool.clone(),
                scope: operation.scope.to_string(),
                argument_keys: operation
                    .arguments
                    .as_object()
                    .map(|map| map.keys().cloned().collect())
                    .unwrap_or_default(),
                preconditions: operation.preconditions.len(),
                conclusion: assessment.conclusion,
                basis: assessment.basis,
                next: assessment.next,
                reason: assessment.reason,
                attempts,
                probes,
                reviews,
            }
        })
        .collect();
    JournalInspection {
        journal: journal.path().display().to_string(),
        plan: PlanSummary {
            digest: plan.digest().clone(),
            summary: plan.plan().summary.clone(),
            tools,
            scopes,
        },
        approval: history.approvals.last().map(|approval| ApprovalSummary {
            // A reviewer note is private text, like an error message.
            approval: Approval {
                note: approval.note.clone().filter(|_| options.messages),
                ..approval.clone()
            },
            approvals: history.approvals.len(),
            retention_bound: !approval.predates_retention(),
            approvals_without_retention: without_retention,
        }),
        sessions,
        operations,
        conclusions,
        next,
    }
}

/// Format Unix milliseconds as an ISO 8601 UTC time to the second.
fn utc(unix_ms: u64) -> String {
    let seconds = unix_ms / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let rest = seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn describe(conclusion: Conclusion, basis: Basis) -> String {
    let how = match basis {
        Basis::None => "",
        Basis::Observed => " (observed)",
        Basis::Reconciled => " (reconciled)",
        Basis::Review => " (reviewed)",
    };
    format!("{}{how}", conclusion.as_str())
}

impl JournalInspection {
    /// Human-readable answers.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let plan = &self.plan;
        let tools: Vec<String> = plan
            .tools
            .iter()
            .map(|(tool, count)| format!("{tool} x{count}"))
            .collect();
        let _ = writeln!(out, "## What was approved?");
        let _ = writeln!(
            out,
            "plan {}{}: {} operations ({})",
            plan.digest,
            plan.summary
                .as_ref()
                .map(|summary| format!(" \"{summary}\""))
                .unwrap_or_default(),
            self.operations.len(),
            tools.join(", ")
        );
        let _ = writeln!(out, "scopes: {}", plan.scopes.join("; "));
        match &self.approval {
            None => {
                let _ = writeln!(out, "not approved: nothing may be dispatched");
            }
            Some(summary) => {
                let approval = &summary.approval;
                let policy = &approval.policy;
                let _ = writeln!(
                    out,
                    "approved by {} at {} (approval {} of {}), binding {}",
                    approval.approver,
                    utc(approval.approved_unix_ms),
                    summary.approvals,
                    summary.approvals,
                    approval.binding.short()
                );
                let _ = writeln!(
                    out,
                    "provider {} as {}; policy: concurrency {}, attempts {}, probes {}, \
                     attempt deadline {} ms, probe deadline {} ms",
                    approval.declaration.provider,
                    approval.declaration.account,
                    policy.concurrency,
                    policy.max_attempts,
                    policy.max_probes,
                    policy.attempt_timeout_ms,
                    policy.probe_timeout_ms
                );
                if approval.predates_retention() {
                    let _ = writeln!(
                        out,
                        "retention: none bound (this approval predates retention policies)"
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "retention {}: {}",
                        approval.declaration.retention,
                        match approval.declaration.retention {
                            Retention::Metadata => {
                                "outcomes, effects, sizes, verdicts, and error codes; no provider text"
                            }
                            Retention::Summary => {
                                "also the provider's error messages and probe evidence, up to 1 KiB each"
                            }
                        }
                    );
                }
                if summary.approvals_without_retention > 0 {
                    let _ = writeln!(
                        out,
                        "warning: {} approval(s) predate retention policies; facts recorded \
                         under them may hold provider values and messages, and the files may \
                         still contain them. Their evidence is shown only with --messages",
                        summary.approvals_without_retention
                    );
                }
                for (tool, contract) in &approval.declaration.contracts {
                    let _ = writeln!(
                        out,
                        "contract {tool}: repeat {}, reconcile by {}",
                        serde_json::to_value(contract.repeat)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_default(),
                        serde_json::to_value(contract.reconcile)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_default()
                    );
                }
            }
        }

        let _ = writeln!(out, "\n## What was attempted and observed?");
        for session in &self.sessions {
            let _ = write!(
                out,
                "session {} {} (pid {}) started {}: {}",
                session.session,
                session.kind,
                session.pid,
                utc(session.started_unix_ms),
                session.status
            );
            if session.status == "unfinished" {
                let _ = write!(out, " - the executor stopped without recording an end");
            }
            if let Some(detail) = &session.detail {
                let _ = write!(out, " - {detail}");
            }
            let _ = write!(
                out,
                "; {} attempts, {} probes",
                session.attempts, session.probes
            );
            if session.trace {
                let _ = write!(
                    out,
                    "; trace: codemode plan trace --session {}",
                    session.session
                );
            }
            let _ = writeln!(out);
        }
        if self.sessions.is_empty() {
            let _ = writeln!(out, "no sessions yet");
        }
        for operation in &self.operations {
            let keys = if operation.argument_keys.is_empty() {
                String::new()
            } else {
                format!(" {{{}}}", operation.argument_keys.join(", "))
            };
            let _ = writeln!(
                out,
                "{} {}{}: {}",
                operation.operation,
                operation.tool,
                keys,
                describe(operation.conclusion, operation.basis)
            );
            let mut lines: Vec<(u64, String)> = Vec::new();
            for attempt in &operation.attempts {
                let mut out = String::new();
                let call = attempt
                    .call_id
                    .map(|id| format!(" call {id}"))
                    .unwrap_or_default();
                let _ = write!(
                    out,
                    "  attempt {} (session {}{call}): {}",
                    attempt.number, attempt.session, attempt.observed
                );
                if let Some(code) = &attempt.code {
                    let _ = write!(out, " {code}");
                }
                if let Some(effect) = &attempt.effect {
                    let _ = write!(out, "; effect {effect}");
                }
                if let Some(bytes) = attempt.result_bytes {
                    let _ = write!(out, ", {bytes} B result");
                }
                if let Some(message) = &attempt.message {
                    let _ = write!(out, ": {}", sanitize(message, MAX_TEXT_CHARS));
                }
                lines.push((attempt.ordinal, out));
            }
            for probe in &operation.probes {
                let mut out = String::new();
                let evidence = match probe.evidence.as_deref() {
                    None => "evidence hidden; use --messages".to_string(),
                    Some("") => "evidence not retained".to_string(),
                    Some(evidence) => sanitize(evidence, MAX_TEXT_CHARS),
                };
                let _ = write!(
                    out,
                    "  reconciled in session {}: {} - {evidence} ({} ms)",
                    probe.session, probe.verdict, probe.elapsed_ms
                );
                lines.push((probe.ordinal, out));
            }
            for review in &operation.reviews {
                let mut out = String::new();
                let note = review
                    .note
                    .as_ref()
                    .map(|note| format!(": {}", sanitize(note, MAX_TEXT_CHARS)))
                    .unwrap_or_default();
                let _ = write!(
                    out,
                    "  reviewed by {} at {}: {}{note}",
                    review.reviewer,
                    utc(review.unix_ms),
                    match review.resolution {
                        Resolution::Applied => "applied",
                        Resolution::NotApplied => "not applied",
                    }
                );
                lines.push((review.ordinal, out));
            }
            lines.sort_by_key(|(ordinal, _)| *ordinal);
            for (_, line) in lines {
                let _ = writeln!(out, "{line}");
            }
        }

        let _ = writeln!(out, "\n## What changed or is currently satisfied?");
        let changed: Vec<&OperationReport> = self
            .operations
            .iter()
            .filter(|operation| operation.conclusion == Conclusion::Applied)
            .collect();
        let satisfied: Vec<&OperationReport> = self
            .operations
            .iter()
            .filter(|operation| operation.conclusion == Conclusion::Satisfied)
            .collect();
        let _ = writeln!(
            out,
            "applied: the provider confirmed our operation ({}): {}",
            changed.len(),
            names(&changed)
        );
        let _ = writeln!(
            out,
            "satisfied, attribution unknown ({}): {}",
            satisfied.len(),
            names(&satisfied)
        );

        let _ = writeln!(out, "\n## What remains uncertain?");
        let uncertain: Vec<&OperationReport> = self
            .operations
            .iter()
            .filter(|operation| operation.conclusion == Conclusion::Unknown)
            .collect();
        if uncertain.is_empty() {
            let _ = writeln!(out, "nothing: every attempt's effect is established");
        }
        for operation in uncertain {
            let _ = writeln!(
                out,
                "{} ({}): {} attempt(s), {} probe(s)",
                operation.operation,
                operation.tool,
                operation.attempts.len(),
                operation.probes.len()
            );
        }

        let _ = writeln!(out, "\n## What can safely happen next, and why?");
        for operation in &self.operations {
            if operation.next == NextStep::Done {
                continue;
            }
            let _ = writeln!(
                out,
                "{} {}: {}",
                operation.operation,
                operation.next.as_str(),
                operation.reason
            );
        }
        if self.next.get("done") == Some(&self.operations.len()) {
            let _ = writeln!(out, "nothing: every operation is applied or satisfied");
        }
        out
    }
}

fn names(operations: &[&OperationReport]) -> String {
    if operations.is_empty() {
        return "none".into();
    }
    operations
        .iter()
        .map(|operation| operation.operation.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
