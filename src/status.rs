//! Where a plan file stands: what happened to each write, and what to do next.
//!
//! A [`StatusReport`] is made from a plan file and, once the plan is
//! approved, the history its journal holds. It reads no network and changes
//! nothing. The same report is the `--json` document of `codemode plan`,
//! `codemode status`, and `codemode apply`.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::executor::{OperationStatus, assess_history};
use crate::journal::History;
use crate::plan::{Digest, FrozenPlan};
use crate::plan_file::PlanFile;
use crate::recovery::{Basis, Conclusion, NextStep};

/// What happened to one write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    /// The server answered with a result.
    Applied,
    /// The server answered with an error that does not say whether the write
    /// changed anything.
    Failed,
    /// The write was sent, or may have been, and no answer came back.
    NoReply,
    /// The write was not applied and is not sent again by itself: the server
    /// refused it before running it, or every attempt is used.
    Refused,
    /// A person recorded that the write was applied.
    Resolved,
    /// A person decided that the write was not applied and must never be
    /// sent. Final.
    Skipped,
    /// The write was not sent yet, or is known not to be applied and will be
    /// sent again.
    NotSent,
}

/// What can happen next to one write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    /// Nothing: the write is applied, resolved, or skipped.
    None,
    /// `codemode apply` sends it.
    Apply,
    /// A person records what happened with `codemode resolve`.
    Resolve,
    /// Every attempt is used: the write needs a new plan.
    NewPlan,
}

/// Where the whole plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// No approval is recorded.
    NotApproved,
    /// Approved; no write was sent yet.
    Approved,
    /// A session has no recorded end: it is running, or it was killed.
    Running,
    /// Every write is applied, resolved as applied, or skipped.
    Done,
    /// A session stopped and at least one write needs a person.
    NeedsAttention,
    /// Nothing needs a person, and `codemode apply` has writes left to send.
    Incomplete,
    /// The plan file differs from the plan that was approved.
    Edited,
}

/// One write of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteStatus {
    /// The write's identity.
    pub id: String,
    /// Its tool.
    pub tool: String,
    /// What happened to it.
    pub state: WriteState,
    /// How many times it was sent, or was about to be sent.
    pub attempts: u32,
    /// The machine code of its last attempt, when that attempt did not
    /// succeed.
    pub error_code: Option<String>,
    /// The error text of its last attempt, as the journal kept it.
    pub error_text: Option<String>,
    /// Whether an answer came back for its last attempt.
    pub answered: bool,
    /// What can happen next.
    pub next: NextAction,
    /// Who resolved it, when a person did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

/// The approval in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalStatus {
    /// Who approved, as stated; the name is not authenticated.
    pub approver: String,
    /// When, Unix milliseconds.
    pub approved_unix_ms: u64,
    /// When, as `YYYY-MM-DD HH:MM UTC`.
    pub approved_at: String,
    /// Writes in flight at once.
    pub concurrency: usize,
    /// Times one write may be sent, the first included.
    pub max_attempts: u32,
    /// The plan digest the approval names; absent when the journal was not
    /// made by `codemode apply`.
    pub digest: Option<Digest>,
}

/// The server a plan goes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerStatus {
    /// The server identity.
    pub identity: String,
    /// The MCP revision the server spoke when the plan was made.
    pub protocol: String,
    /// Digest of the server file the plan was made with.
    pub config: Digest,
    /// The server file: the path the plan records, or the one a command was
    /// given.
    pub file: Option<String>,
    /// Whether that file still is the one the plan was made with.
    pub file_state: ServerFileState,
    /// The whole command and arguments of a stdio server, when the identity
    /// had to be shortened. Set by a command that read the server file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// What a command found when it looked at a plan's server file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerFileState {
    /// Nobody looked.
    Unchecked,
    /// Its bytes have the digest the plan records.
    Same,
    /// Its bytes differ from the file the plan was made with.
    Changed,
    /// It is missing or cannot be read.
    Unreadable,
}

/// Writes per state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Every write of the plan.
    pub total: usize,
    /// [`WriteState::Applied`].
    pub applied: usize,
    /// [`WriteState::Failed`].
    pub failed: usize,
    /// [`WriteState::NoReply`].
    pub no_reply: usize,
    /// [`WriteState::Refused`].
    pub refused: usize,
    /// [`WriteState::Resolved`].
    pub resolved: usize,
    /// [`WriteState::Skipped`].
    pub skipped: usize,
    /// [`WriteState::NotSent`].
    pub not_sent: usize,
}

/// Where a plan file stands. Serializes as the `--json` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// The plan file, as the person named it.
    pub file: String,
    /// The plan's digest.
    pub digest: Digest,
    /// The server.
    pub server: ServerStatus,
    /// The plan's summary.
    pub summary: Option<String>,
    /// Where the plan stands.
    pub state: PlanState,
    /// The state in a few words, such as `stopped after 1 failure`.
    pub detail: String,
    /// The approval in force, if any.
    pub approval: Option<ApprovalStatus>,
    /// Writes per state.
    pub counts: Counts,
    /// Every write, in plan order.
    pub writes: Vec<WriteStatus>,
}

/// The digest an approval recorded for its plan file, kept in the approval's
/// note.
#[must_use]
pub fn approved_digest(history: &History) -> Option<Digest> {
    let note = history.approvals.last()?.note.clone()?;
    Digest::try_from(note).ok()
}

impl StatusReport {
    /// The report of `plan`, named `file`, with the `history` of its journal
    /// when it has one.
    ///
    /// Before an approval every write is [`WriteState::NotSent`]. When the
    /// journal's approval names another digest than `plan` has now, the state
    /// is [`PlanState::Edited`] and no write is listed, because the journal
    /// describes another plan.
    ///
    /// ```
    /// use codemode::plan_file::PlanFile;
    /// use codemode::status::{PlanState, StatusReport, WriteState};
    ///
    /// let plan = PlanFile::parse(r#"{
    ///   "codemode_plan": 1,
    ///   "server": {"identity": "https://mcp.example.com/mcp", "protocol": "2025-11-25", "trust_annotations": false,
    ///              "config": "sha256:1111111111111111111111111111111111111111111111111111111111111111"},
    ///   "tools": {
    ///     "notes.put": {"class": "write", "definition": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "destructive": false, "annotated": false}
    ///   },
    ///   "operations": [
    ///     {"id": "put-a", "tool": "notes.put", "arguments": {"id": "a", "text": "one"}}
    ///   ]
    /// }"#)?;
    /// let report = StatusReport::new("notes.plan.json", &plan, &plan.frozen()?, None);
    /// assert_eq!(report.state, PlanState::NotApproved);
    /// assert_eq!(report.digest, plan.digest());
    /// assert_eq!(report.writes[0].state, WriteState::NotSent);
    /// assert_eq!(report.exit_code(), 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn new(
        file: &str,
        plan: &PlanFile,
        frozen: &FrozenPlan,
        history: Option<&History>,
    ) -> Self {
        let digest = plan.digest();
        let mut report = Self {
            file: file.to_string(),
            digest: digest.clone(),
            server: ServerStatus {
                identity: plan.server.identity.clone(),
                protocol: plan.server.protocol.clone(),
                config: plan.server.config.clone(),
                file: plan.server.file.clone(),
                file_state: ServerFileState::Unchecked,
                command: None,
            },
            summary: plan.summary.clone(),
            state: PlanState::NotApproved,
            detail: "not approved".into(),
            approval: None,
            counts: Counts::default(),
            writes: Vec::new(),
        };
        let approved = history.and_then(|history| {
            let approval = history.approvals.last()?;
            Some((history, approval))
        });
        let Some((history, approval)) = approved else {
            report.writes = plan
                .writes
                .iter()
                .map(|write| WriteStatus {
                    id: write.id.clone(),
                    tool: write.tool.clone(),
                    state: WriteState::NotSent,
                    attempts: 0,
                    error_code: None,
                    error_text: None,
                    answered: false,
                    next: NextAction::Apply,
                    resolved_by: None,
                })
                .collect();
            report.count();
            return report;
        };
        report.approval = Some(ApprovalStatus {
            approver: approval.approver.clone(),
            approved_unix_ms: approval.approved_unix_ms,
            approved_at: utc_minute(approval.approved_unix_ms),
            concurrency: approval.policy.concurrency,
            max_attempts: approval.policy.max_attempts,
            digest: approved_digest(history),
        });
        if approved_digest(history).as_ref() != Some(&digest) {
            report.state = PlanState::Edited;
            report.detail = "edited after approval".into();
            return report;
        }
        let reviewers: HashMap<&str, &str> = history
            .reviews
            .iter()
            .map(|review| (review.operation.as_str(), review.reviewer.as_str()))
            .collect();
        report.writes = assess_history(frozen, history, &HashSet::new())
            .iter()
            .map(|status| write_status(status, &reviewers))
            .collect();
        report.count();
        (report.state, report.detail) = stage(&report.counts, &report.writes, history);
        report
    }

    fn count(&mut self) {
        let mut counts = Counts {
            total: self.writes.len(),
            ..Counts::default()
        };
        for write in &self.writes {
            *match write.state {
                WriteState::Applied => &mut counts.applied,
                WriteState::Failed => &mut counts.failed,
                WriteState::NoReply => &mut counts.no_reply,
                WriteState::Refused => &mut counts.refused,
                WriteState::Resolved => &mut counts.resolved,
                WriteState::Skipped => &mut counts.skipped,
                WriteState::NotSent => &mut counts.not_sent,
            } += 1;
        }
        self.counts = counts;
    }

    /// Whether `codemode apply` has a write to send.
    #[must_use]
    pub fn work_remains(&self) -> bool {
        self.writes
            .iter()
            .any(|write| write.next == NextAction::Apply)
    }

    /// The process exit status after `codemode apply`: 0 when every write is
    /// applied, resolved as applied, or skipped, otherwise 3.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.state == PlanState::Done { 0 } else { 3 }
    }
}

fn write_status(status: &OperationStatus, reviewers: &HashMap<&str, &str>) -> WriteStatus {
    let assessment = &status.assessment;
    let last = status.last_attempt.as_ref();
    let answered = last.is_some_and(|attempt| attempt.answered);
    let reviewed = assessment.basis == Basis::Review;
    let (state, next) = match (assessment.conclusion, assessment.next) {
        (Conclusion::Applied | Conclusion::Satisfied, _) if reviewed => {
            (WriteState::Resolved, NextAction::None)
        }
        (Conclusion::Applied | Conclusion::Satisfied, _) => (WriteState::Applied, NextAction::None),
        (Conclusion::Skipped, _) => (WriteState::Skipped, NextAction::None),
        (Conclusion::Pending, _) | (Conclusion::NotApplied, NextStep::Dispatch) => {
            (WriteState::NotSent, NextAction::Apply)
        }
        // Known not to be applied, and every attempt is used.
        (Conclusion::NotApplied, _) => (WriteState::Refused, NextAction::NewPlan),
        (Conclusion::Refused, _) => (WriteState::Refused, NextAction::Resolve),
        (Conclusion::Unknown, next) => (
            if answered {
                WriteState::Failed
            } else {
                WriteState::NoReply
            },
            if next == NextStep::Review {
                NextAction::Resolve
            } else {
                NextAction::Apply
            },
        ),
    };
    // A write a person resolved starts again or is settled: its earlier
    // error is no longer what stands.
    let failed = !matches!(state, WriteState::Applied | WriteState::Resolved) && !reviewed;
    WriteStatus {
        id: status.operation.clone(),
        tool: status.tool.clone(),
        state,
        attempts: assessment.attempts,
        error_code: last
            .filter(|_| failed)
            .and_then(|attempt| attempt.code.clone()),
        error_text: last
            .filter(|_| failed)
            .and_then(|attempt| attempt.message.clone()),
        answered,
        next,
        resolved_by: reviewers
            .get(status.operation.as_str())
            .filter(|_| reviewed)
            .map(|name| (*name).to_string()),
    }
}

/// The plan's state and a few words for it.
fn stage(counts: &Counts, writes: &[WriteStatus], history: &History) -> (PlanState, String) {
    let applied = counts.applied + counts.resolved;
    if applied + counts.skipped == counts.total {
        let detail = if counts.skipped == 0 {
            "done: every write is applied".to_string()
        } else {
            format!(
                "done: {} applied, {} skipped",
                grouped(applied),
                grouped(counts.skipped)
            )
        };
        return (PlanState::Done, detail);
    }
    let Some(last) = history.sessions.last() else {
        return (PlanState::Approved, "approved, not started".into());
    };
    let Some(end) = &last.end else {
        return (
            PlanState::Running,
            "running, or interrupted before it could record its end".into(),
        );
    };
    let failures = counts.failed + counts.no_reply + counts.refused;
    let waiting = writes
        .iter()
        .filter(|write| matches!(write.next, NextAction::Resolve | NextAction::NewPlan))
        .count();
    let state = if waiting > 0 {
        PlanState::NeedsAttention
    } else {
        PlanState::Incomplete
    };
    let ended_by = |word: &str| {
        end.status == "interrupted" && end.detail.as_deref().is_some_and(|d| d.contains(word))
    };
    let detail = if ended_by("deadline") {
        "stopped at the deadline".to_string()
    } else if ended_by("cancelled") {
        "cancelled".to_string()
    } else if failures > 0 && counts.not_sent > 0 {
        format!("stopped after {}", plural(failures, "failure"))
    } else if failures > 0 {
        format!("finished with {}", plural(failures, "failure"))
    } else {
        "stopped with writes left to send".to_string()
    };
    (state, detail)
}

/// `count` and `noun`, with an `s` unless `count` is 1.
#[must_use]
pub fn plural(count: usize, noun: &str) -> String {
    format!(
        "{} {noun}{}",
        grouped(count),
        if count == 1 { "" } else { "s" }
    )
}

/// `count` with a comma between groups of three digits.
#[must_use]
pub fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Unix milliseconds as `YYYY-MM-DD HH:MM UTC`.
#[must_use]
pub fn utc_minute(unix_ms: u64) -> String {
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
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_times_read_well() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(plural(1, "write"), "1 write");
        assert_eq!(plural(5000, "write"), "5,000 writes");
        assert_eq!(utc_minute(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_minute(1_794_398_400_000), "2026-11-11 12:00 UTC");
    }
}
