//! Provider-owned recovery contracts, approvals, and what may safely happen next.
//!
//! The trusted provider, never the plan or guest code, declares how each of its
//! tools can be recovered ([`RecoveryContract`]) and implements the probes
//! ([`RecoverableProvider::reconcile`]). [`assess`] folds an operation's journaled
//! facts into one of four distinct conclusions and the next safe step.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::plan::{Digest, FrozenPlan, Operation, Scope};
use crate::runtime::{CallContext, ProviderError, RunCancellation};

/// How a provider can recover one tool, declared by the provider itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryContract {
    /// Whether sending the operation again is safe when its outcome is unknown.
    pub repeat: RepeatSafety,
    /// How the provider can establish an uncertain outcome.
    pub reconcile: ReconcileMethod,
}

/// Whether repeating an operation can duplicate its effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatSafety {
    /// Repeating cannot add a second effect under the provider's documented
    /// conditions, for example adding a label that a set already contains.
    Safe,
    /// Repeating can duplicate the effect, for example posting a comment.
    Unsafe,
}

/// How a provider establishes the outcome of an uncertain attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileMethod {
    /// Look up the provider's receipt for the attempt keys. The provider must also
    /// make sure an attempt it reports as not applied can never apply later.
    Receipt,
    /// Read authoritative current state. Establishes at most that the desired state
    /// holds, never who caused it.
    State,
    /// No reliable method: an uncertain outcome needs a person.
    None,
}

/// The provider's identity and recovery contracts, fixed for an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDeclaration {
    /// Provider name; each operation's `scope.provider` must match it.
    pub provider: String,
    /// The account this provider acts as; each operation's `scope.account` must match it.
    pub account: String,
    /// Recovery contract per supported tool.
    pub contracts: BTreeMap<String, RecoveryContract>,
}

impl ProviderDeclaration {
    /// Check that every operation can run through this provider: same provider,
    /// same account, and a declared tool.
    ///
    /// # Errors
    /// Returns a message naming the first operation that does not match.
    pub fn admits(&self, plan: &FrozenPlan) -> Result<(), String> {
        for operation in &plan.plan().operations {
            let scope = &operation.scope;
            if scope.provider != self.provider || scope.account != self.account {
                return Err(format!(
                    "operation {} is scoped to {}, but the provider acts as {}/{}",
                    operation.id, scope, self.provider, self.account
                ));
            }
            if !self.contracts.contains_key(&operation.tool) {
                return Err(format!(
                    "operation {} uses tool {}, which provider {} does not declare",
                    operation.id, operation.tool, self.provider
                ));
            }
        }
        Ok(())
    }

    /// The same declaration, keeping only the contracts of tools `plan` uses, so
    /// an approval does not depend on unrelated tools.
    #[must_use]
    pub fn for_plan(&self, plan: &FrozenPlan) -> Self {
        let used: std::collections::HashSet<_> = plan
            .plan()
            .operations
            .iter()
            .map(|operation| operation.tool.as_str())
            .collect();
        Self {
            provider: self.provider.clone(),
            account: self.account.clone(),
            contracts: self
                .contracts
                .iter()
                .filter(|(tool, _)| used.contains(tool.as_str()))
                .map(|(tool, contract)| (tool.clone(), *contract))
                .collect(),
        }
    }
}

/// Budgets and deadlines for executing and recovering one plan. Part of the approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    /// Maximum operations in flight at once.
    pub concurrency: usize,
    /// Maximum dispatches of one operation across all sessions, the first included.
    pub max_attempts: u32,
    /// Maximum reconciliation probes of one operation across all sessions.
    pub max_probes: u32,
    /// Deadline for one attempt; an attempt without a result by then is unknown.
    pub attempt_timeout_ms: u64,
    /// Deadline for one reconciliation probe.
    pub probe_timeout_ms: u64,
    /// Deadline for one session's dispatch phase, including guest startup.
    pub session_timeout_ms: u64,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_attempts: 3,
            max_probes: 3,
            attempt_timeout_ms: 10_000,
            probe_timeout_ms: 5_000,
            session_timeout_ms: 60_000,
        }
    }
}

impl RecoveryPolicy {
    /// Check that every budget and deadline is positive.
    ///
    /// # Errors
    /// Returns a message naming the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        let fields = [
            ("concurrency", self.concurrency as u64),
            ("max_attempts", u64::from(self.max_attempts)),
            ("max_probes", u64::from(self.max_probes)),
            ("attempt_timeout_ms", self.attempt_timeout_ms),
            ("probe_timeout_ms", self.probe_timeout_ms),
            ("session_timeout_ms", self.session_timeout_ms),
        ];
        match fields.iter().find(|(_, value)| *value == 0) {
            Some((name, _)) => Err(format!("{name} must be positive")),
            None => Ok(()),
        }
    }
}

/// A person's approval of one frozen plan, provider declaration, and policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// Digest binding the plan, declaration, and policy; see [`approval_binding`].
    pub binding: Digest,
    /// The approved plan's digest.
    pub plan: Digest,
    /// The provider identity and the contracts of the tools the plan uses.
    pub declaration: ProviderDeclaration,
    /// Budgets and deadlines.
    pub policy: RecoveryPolicy,
    /// Who approved, as stated by the approver.
    pub approver: String,
    /// Optional reviewer note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Wall-clock approval time, Unix milliseconds.
    pub approved_unix_ms: u64,
}

/// The digest an approval binds to: the plan digest, the provider declaration for
/// the plan's tools, and the policy. Changing any of them changes the binding.
#[must_use]
pub fn approval_binding(
    plan: &Digest,
    declaration: &ProviderDeclaration,
    policy: &RecoveryPolicy,
) -> Digest {
    let value = serde_json::json!({
        "format": "codemode-approval/1",
        "plan": plan,
        "declaration": declaration,
        "policy": policy,
    });
    Digest::of(value.to_string().as_bytes())
}

/// One dispatch of an operation.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// The approved operation, exactly as frozen.
    pub operation: Operation,
    /// Unique key for this attempt; a provider that supports receipts records it.
    pub key: String,
    /// 1 for the first dispatch of this operation, then 2, 3, ...
    pub number: u32,
}

/// What a provider can say about the effect of a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureEffect {
    /// Refused before any effect, for example a failed precondition or a denied
    /// permission; sending it again unchanged will not help.
    Refused,
    /// Guaranteed not applied, for example rate-limited before processing; it may be sent again.
    NotApplied,
    /// The provider cannot say whether an effect occurred.
    Unknown,
}

/// A failed attempt with the provider's statement about its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptError {
    /// The provider's error.
    pub error: ProviderError,
    /// Whether an effect may have occurred.
    pub effect: FailureEffect,
}

/// The four distinct conclusions a reconciliation can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Provider evidence attributes the effect to one of our attempts.
    Applied,
    /// The desired state holds now; whether our attempt caused it is unknown.
    Satisfied,
    /// None of the probed attempts applied, and none can apply later.
    NotApplied,
    /// The outcome is still unknown.
    Unknown,
}

/// A provider's answer to a reconciliation probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// The conclusion.
    pub verdict: Verdict,
    /// A short explanation without argument or result values, for example "receipt found".
    pub evidence: String,
}

/// The deadline and cancellation signal for a probe outside a dispatch run.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    /// Absolute deadline for this probe.
    pub deadline: Instant,
    /// The session's cancellation signal.
    pub cancellation: RunCancellation,
}

/// A trusted provider whose operations can be journaled and recovered.
///
/// Credentials stay inside the implementation. Futures must yield rather than block.
pub trait RecoverableProvider: Send + Sync + 'static {
    /// Identity and recovery contracts. An approval binds to them; a change refuses execution.
    fn declaration(&self) -> ProviderDeclaration;

    /// Check that the account may still act on `scope` now. Called before each
    /// session; a persisted approval is not evidence of current authorization.
    ///
    /// # Errors
    /// Returns the provider's reason for refusing.
    fn authorize(
        &self,
        scope: &Scope,
        context: ProbeContext,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;

    /// Execute one attempt once, enforcing the operation's preconditions atomically
    /// when it applies. Receives the run's shared deadline and cancellation.
    ///
    /// # Errors
    /// Returns an [`AttemptError`] whose effect states what the provider knows.
    /// Dropping the future does not prove that no effect occurred.
    fn execute(
        &self,
        attempt: Attempt,
        context: CallContext,
    ) -> impl Future<Output = Result<Value, AttemptError>> + Send;

    /// Establish what happened to the attempts with `keys`, using the method the
    /// contract declares. Called only for tools whose method is not `none`.
    ///
    /// # Errors
    /// Returns an error when the probe itself failed; the outcome stays unknown.
    fn reconcile(
        &self,
        operation: Operation,
        keys: Vec<String>,
        context: ProbeContext,
    ) -> impl Future<Output = Result<Reconciliation, ProviderError>> + Send;
}

/// What is known about an operation's effect, from its journaled facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// Never dispatched.
    Pending,
    /// Our operation is confirmed applied.
    Applied,
    /// The desired state is satisfied; attribution is unknown.
    Satisfied,
    /// Confirmed not applied; it may be sent again.
    NotApplied,
    /// Refused by the provider before any effect; not applied.
    Refused,
    /// The outcome remains unknown: an attempt may have reached the provider.
    Unknown,
}

impl Conclusion {
    /// The serialized name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Satisfied => "satisfied",
            Self::NotApplied => "not_applied",
            Self::Refused => "refused",
            Self::Unknown => "unknown",
        }
    }
}

/// How a conclusion was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Nothing has happened yet.
    None,
    /// An observed attempt result.
    Observed,
    /// A provider reconciliation probe.
    Reconciled,
    /// A person's recorded review.
    Review,
}

/// The next safe step for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStep {
    /// Nothing remains.
    Done,
    /// Dispatching it now is safe.
    Dispatch,
    /// Ask the provider to establish the outcome first.
    Reconcile,
    /// A person must decide; nothing will happen automatically.
    Review,
}

impl NextStep {
    /// The serialized name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Dispatch => "dispatch",
            Self::Reconcile => "reconcile",
            Self::Review => "review",
        }
    }
}

/// A person's finding after reviewing an operation outside codemode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The person confirmed the effect happened.
    Applied,
    /// The person confirmed it did not happen; it may be sent again within budget.
    NotApplied,
}

/// What a journal observed about one attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observed {
    /// The provider returned a result: our operation applied.
    Succeeded {
        /// The result, when within the journal's value budget.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        /// Serialized size of the result.
        bytes: u64,
    },
    /// The provider returned an error.
    Failed {
        /// The error, with a bounded message.
        error: ProviderError,
        /// The provider's statement about the effect.
        effect: FailureEffect,
    },
    /// The attempt deadline elapsed without a result.
    TimedOut,
    /// The session ended before a result was observed.
    Unobserved {
        /// The run status that ended it.
        reason: String,
    },
    /// The attempt was withdrawn after its intent was recorded and before it was sent.
    Withdrawn,
}

impl Observed {
    /// The effect this observation establishes.
    #[must_use]
    pub fn conclusion(&self) -> Conclusion {
        match self {
            Self::Succeeded { .. } => Conclusion::Applied,
            Self::Failed {
                effect: FailureEffect::Refused,
                ..
            } => Conclusion::Refused,
            Self::Failed {
                effect: FailureEffect::NotApplied,
                ..
            }
            | Self::Withdrawn => Conclusion::NotApplied,
            Self::Failed {
                effect: FailureEffect::Unknown,
                ..
            }
            | Self::TimedOut
            | Self::Unobserved { .. } => Conclusion::Unknown,
        }
    }
}

/// One fact about an operation, in journal order, as [`assess`] consumes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Fact<'a> {
    /// Dispatch intent for attempt `number`, durable before it could reach the provider.
    Intent {
        /// Attempt number.
        number: u32,
    },
    /// The observed result of attempt `number`.
    Observation {
        /// Attempt number.
        number: u32,
        /// What was observed.
        observed: &'a Observed,
    },
    /// A reconciliation probe, or `None` when the probe itself failed.
    Probe(Option<Verdict>),
    /// A person's review.
    Review(Resolution),
}

/// An operation's conclusion, how it was reached, and the next safe step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    /// What is known about the effect.
    pub conclusion: Conclusion,
    /// How that is known.
    pub basis: Basis,
    /// Dispatches so far.
    pub attempts: u32,
    /// Reconciliation probes so far.
    pub probes: u32,
    /// What may safely happen next.
    pub next: NextStep,
    /// Why, in one sentence.
    pub reason: String,
}

/// Fold an operation's facts, in journal order, into its conclusion and next step.
///
/// An attempt that may have reached the provider stays unknown until an observed
/// result, a reconciliation, or a review settles it. A later refusal does not
/// settle an earlier unknown attempt. `probed_this_session` prevents probing the
/// same operation twice in one session.
#[must_use]
pub fn assess(
    facts: &[Fact<'_>],
    contract: RecoveryContract,
    policy: &RecoveryPolicy,
    probed_this_session: bool,
) -> Assessment {
    let mut conclusion = Conclusion::Pending;
    let mut basis = Basis::None;
    let mut attempts = 0;
    let mut probes = 0;
    for fact in facts {
        match *fact {
            Fact::Intent { number } => {
                attempts = attempts.max(number);
                conclusion = Conclusion::Unknown;
                basis = Basis::None;
            }
            Fact::Observation { observed, .. } => {
                basis = Basis::Observed;
                match observed.conclusion() {
                    Conclusion::Applied => conclusion = Conclusion::Applied,
                    Conclusion::Unknown => conclusion = Conclusion::Unknown,
                    // Not applied by this attempt; an earlier unknown one may have applied.
                    settled => {
                        conclusion = if unsettled_before(facts, fact) {
                            Conclusion::Unknown
                        } else {
                            settled
                        };
                    }
                }
            }
            Fact::Probe(verdict) => {
                probes += 1;
                let Some(verdict) = verdict else { continue };
                basis = Basis::Reconciled;
                conclusion = match verdict {
                    Verdict::Applied => Conclusion::Applied,
                    Verdict::Satisfied => Conclusion::Satisfied,
                    Verdict::NotApplied => Conclusion::NotApplied,
                    Verdict::Unknown => Conclusion::Unknown,
                };
            }
            Fact::Review(resolution) => {
                basis = Basis::Review;
                conclusion = match resolution {
                    Resolution::Applied => Conclusion::Applied,
                    Resolution::NotApplied => Conclusion::NotApplied,
                };
            }
        }
    }
    let (next, reason) = next_step(
        conclusion,
        contract,
        policy,
        attempts,
        probes,
        probed_this_session,
    );
    Assessment {
        conclusion,
        basis,
        attempts,
        probes,
        next,
        reason,
    }
}

/// Whether an attempt before the one `current` observes ended unknown and was
/// not settled before `current`.
fn unsettled_before(facts: &[Fact<'_>], current: &Fact<'_>) -> bool {
    let Fact::Observation { number, .. } = current else {
        return false;
    };
    let mut unsettled = false;
    for fact in facts {
        if std::ptr::eq(fact, current) {
            break;
        }
        match fact {
            Fact::Intent { number: n } if n < number => unsettled = true,
            Fact::Observation {
                number: n,
                observed,
            } if n < number => {
                unsettled = observed.conclusion() == Conclusion::Unknown;
            }
            Fact::Probe(Some(verdict)) if *verdict != Verdict::Unknown => unsettled = false,
            Fact::Review(_) => unsettled = false,
            _ => {}
        }
    }
    unsettled
}

fn next_step(
    conclusion: Conclusion,
    contract: RecoveryContract,
    policy: &RecoveryPolicy,
    attempts: u32,
    probes: u32,
    probed_this_session: bool,
) -> (NextStep, String) {
    let budget_left = attempts < policy.max_attempts;
    match conclusion {
        Conclusion::Pending => (NextStep::Dispatch, "not yet attempted".into()),
        Conclusion::Applied => (NextStep::Done, "our operation is confirmed applied".into()),
        Conclusion::Satisfied => (
            NextStep::Done,
            "the desired state holds; whether our attempt caused it is unknown".into(),
        ),
        Conclusion::Refused => (
            NextStep::Review,
            "the provider refused it before any effect; sending it again unchanged will not help"
                .into(),
        ),
        Conclusion::NotApplied if budget_left => (
            NextStep::Dispatch,
            format!(
                "confirmed not applied; attempt {} of {} is allowed",
                attempts + 1,
                policy.max_attempts
            ),
        ),
        Conclusion::NotApplied => (
            NextStep::Review,
            format!(
                "confirmed not applied, but all {} attempts are used",
                policy.max_attempts
            ),
        ),
        Conclusion::Unknown => {
            let can_probe = contract.reconcile != ReconcileMethod::None
                && probes < policy.max_probes
                && !probed_this_session;
            if can_probe {
                let method = match contract.reconcile {
                    ReconcileMethod::Receipt => "its receipt",
                    _ => "authoritative state",
                };
                return (
                    NextStep::Reconcile,
                    format!("an attempt may have reached the provider; check {method} first"),
                );
            }
            if contract.repeat == RepeatSafety::Safe && budget_left {
                return (
                    NextStep::Dispatch,
                    format!(
                        "outcome unknown, but the provider declares repeating it safe; attempt {} of {}",
                        attempts + 1,
                        policy.max_attempts
                    ),
                );
            }
            let reason = match (contract.repeat, contract.reconcile) {
                (RepeatSafety::Unsafe, ReconcileMethod::None) => {
                    "outcome unknown: the provider offers no reconciliation and repeating it may duplicate the effect"
                        .to_string()
                }
                (RepeatSafety::Unsafe, _) if probed_this_session => {
                    "outcome unknown after reconciliation, and repeating it may duplicate the effect"
                        .to_string()
                }
                (RepeatSafety::Unsafe, _) => format!(
                    "outcome unknown after all {} reconciliation probes, and repeating it may duplicate the effect",
                    policy.max_probes
                ),
                (RepeatSafety::Safe, _) => format!(
                    "outcome unknown and all {} attempts are used",
                    policy.max_attempts
                ),
            };
            (NextStep::Review, reason)
        }
    }
}

impl Verdict {
    const ALL: [Self; 4] = [
        Self::Applied,
        Self::Satisfied,
        Self::NotApplied,
        Self::Unknown,
    ];

    /// The serialized name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Satisfied => "satisfied",
            Self::NotApplied => "not_applied",
            Self::Unknown => "unknown",
        }
    }

    /// The verdict with serialized name `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|verdict| verdict.as_str() == name)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Resolution {
    /// The serialized name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
        }
    }

    /// The resolution with serialized name `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Applied, Self::NotApplied]
            .into_iter()
            .find(|resolution| resolution.as_str() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECEIPT: RecoveryContract = RecoveryContract {
        repeat: RepeatSafety::Safe,
        reconcile: ReconcileMethod::Receipt,
    };
    const NONE: RecoveryContract = RecoveryContract {
        repeat: RepeatSafety::Unsafe,
        reconcile: ReconcileMethod::None,
    };

    fn step(facts: &[Fact<'_>], contract: RecoveryContract) -> (Conclusion, NextStep) {
        let assessment = assess(facts, contract, &RecoveryPolicy::default(), false);
        (assessment.conclusion, assessment.next)
    }

    #[test]
    fn a_missing_acknowledgment_or_generic_error_stays_unknown() {
        let generic = Observed::Failed {
            error: ProviderError {
                code: "network".into(),
                message: "reset".into(),
            },
            effect: FailureEffect::Unknown,
        };
        for facts in [
            vec![Fact::Intent { number: 1 }],
            vec![
                Fact::Intent { number: 1 },
                Fact::Observation {
                    number: 1,
                    observed: &generic,
                },
            ],
        ] {
            assert_eq!(
                step(&facts, RECEIPT),
                (Conclusion::Unknown, NextStep::Reconcile)
            );
            assert_eq!(step(&facts, NONE), (Conclusion::Unknown, NextStep::Review));
        }
    }

    #[test]
    fn a_later_refusal_does_not_settle_an_earlier_unknown_attempt() {
        let refused = Observed::Failed {
            error: ProviderError {
                code: "precondition_failed".into(),
                message: "closed".into(),
            },
            effect: FailureEffect::Refused,
        };
        let facts = [
            Fact::Intent { number: 1 },
            Fact::Probe(Some(Verdict::Unknown)),
            Fact::Intent { number: 2 },
            Fact::Observation {
                number: 2,
                observed: &refused,
            },
        ];
        assert_eq!(step(&facts, RECEIPT).0, Conclusion::Unknown);
        let settled = [
            Fact::Intent { number: 1 },
            Fact::Probe(Some(Verdict::NotApplied)),
            Fact::Intent { number: 2 },
            Fact::Observation {
                number: 2,
                observed: &refused,
            },
        ];
        assert_eq!(
            step(&settled, RECEIPT),
            (Conclusion::Refused, NextStep::Review)
        );
    }

    #[test]
    fn budgets_bound_retries_and_probes() {
        let policy = RecoveryPolicy {
            max_attempts: 2,
            max_probes: 1,
            ..RecoveryPolicy::default()
        };
        let facts = [
            Fact::Intent { number: 1 },
            Fact::Probe(Some(Verdict::Unknown)),
            Fact::Intent { number: 2 },
        ];
        let assessment = assess(&facts, RECEIPT, &policy, false);
        assert_eq!(assessment.next, NextStep::Review);
        assert!(
            assessment.reason.contains("all 2 attempts"),
            "{assessment:?}"
        );
    }
}
