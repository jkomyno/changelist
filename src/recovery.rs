//! Provider-owned recovery contracts, approvals, and what may safely happen next.
//!
//! The trusted provider, never the plan or guest code, declares how each of its
//! tools can be recovered ([`RecoveryContract`]) and implements the probes
//! ([`RecoverableProvider::reconcile`]). `assess` folds an operation's journaled
//! facts into one of four distinct conclusions and the next safe step.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use crate::plan::{Digest, FrozenPlan, Operation, Scope, canonical_json};
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

/// What the journal keeps of a provider's replies, declared by the provider and
/// bound by the approval. Plans and guest code cannot select or widen it.
///
/// Every policy keeps the facts recovery needs: attempt identity and order, the
/// observed outcome and stated effect, result sizes, probe verdicts, and error
/// codes that look like machine codes: 1 to 64 ASCII letters, digits, or
/// `_ . : -`; any other code is kept as [`UNRECOGNIZED_CODE`]. Result values are
/// never kept. The journal applies the policy before anything is written, so an
/// excluded value never reaches the database or its write-ahead log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retention {
    /// `metadata/1`, the default: no provider prose. Error messages and probe
    /// evidence are dropped; only codes and verdicts remain.
    #[default]
    #[serde(rename = "metadata/1")]
    Metadata,
    /// `summary/1`: also keeps error messages and probe evidence, each truncated
    /// to 1 KiB. The provider declares that it writes these itself as short, safe
    /// explanations (a projection), never raw upstream text.
    #[serde(rename = "summary/1")]
    Summary,
}

impl Retention {
    /// The policy identifier, such as `metadata/1`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata/1",
            Self::Summary => "summary/1",
        }
    }
}

impl fmt::Display for Retention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Longest error code kept as given.
const MAX_CODE_BYTES: usize = 64;
/// The code kept in place of one that is not a machine code.
pub const UNRECOGNIZED_CODE: &str = "unrecognized_code";

/// `code` if it looks like a machine code: 1 to 64 ASCII letters, digits, or
/// `_ . : -`. Anything else, such as prose, becomes [`UNRECOGNIZED_CODE`], so a
/// code cannot carry free-form provider text into the journal or a trace.
#[must_use]
pub(crate) fn machine_code(code: &str) -> &str {
    let machine = !code.is_empty()
        && code.len() <= MAX_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte));
    if machine { code } else { UNRECOGNIZED_CODE }
}

/// The provider's identity, recovery contracts, and retention policy, fixed for an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDeclaration {
    /// Provider name; each operation's `scope.provider` must match it.
    pub provider: String,
    /// The account this provider acts as; each operation's `scope.account` must match it.
    pub account: String,
    /// Recovery contract per supported tool.
    pub contracts: BTreeMap<String, RecoveryContract>,
    /// What the journal keeps of this provider's replies; `metadata/1` when omitted.
    #[serde(default)]
    pub retention: Retention,
    /// Digest of each tool's definition as the provider saw it when it declared,
    /// for providers whose tools are defined elsewhere, such as an MCP server.
    /// An approval binds them: a definition that changed since approval refuses
    /// the session. Empty for a provider that defines its own tools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub definitions: BTreeMap<String, Digest>,
}

impl ProviderDeclaration {
    /// Check that every operation can run through this provider: same provider,
    /// same account, and a declared tool.
    ///
    /// # Errors
    /// Returns a [`DeclarationError`] naming the first operation that does not match.
    pub fn admits(&self, plan: &FrozenPlan) -> Result<(), DeclarationError> {
        for operation in &plan.plan().operations {
            let scope = &operation.scope;
            if scope.provider != self.provider || scope.account != self.account {
                return Err(DeclarationError::OtherScope {
                    operation: operation.id.clone(),
                    scope: scope.to_string(),
                    provider: self.provider.clone(),
                    account: self.account.clone(),
                });
            }
            if !self.contracts.contains_key(&operation.tool) {
                return Err(DeclarationError::UndeclaredTool {
                    operation: operation.id.clone(),
                    tool: operation.tool.clone(),
                    provider: self.provider.clone(),
                });
            }
        }
        Ok(())
    }

    /// The same declaration, keeping only the contracts and definitions of tools `plan` uses, so
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
            retention: self.retention,
            contracts: self
                .contracts
                .iter()
                .filter(|(tool, _)| used.contains(tool.as_str()))
                .map(|(tool, contract)| (tool.clone(), *contract))
                .collect(),
            definitions: self
                .definitions
                .iter()
                .filter(|(tool, _)| used.contains(tool.as_str()))
                .map(|(tool, digest)| (tool.clone(), digest.clone()))
                .collect(),
        }
    }
}

/// Budgets for executing and recovering one plan. Part of the approval.
///
/// The session deadline is not here: each `run` or `resume` chooses its own
/// (see [`SessionOptions`](crate::SessionOptions)), because stopping earlier can
/// only reduce effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    /// Maximum operations in flight at once.
    pub concurrency: usize,
    /// Maximum dispatches of one operation across all sessions, the first included.
    pub max_attempts: u32,
    /// Maximum reconciliation probes of one operation across all sessions.
    pub max_probes: u32,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_attempts: 3,
            max_probes: 3,
        }
    }
}

impl RecoveryPolicy {
    /// Check that every budget is positive.
    ///
    /// # Errors
    /// Returns a [`PolicyError`] naming the first field that is zero.
    pub fn validate(&self) -> Result<(), PolicyError> {
        let fields = [
            ("concurrency", self.concurrency as u64),
            ("max_attempts", u64::from(self.max_attempts)),
            ("max_probes", u64::from(self.max_probes)),
        ];
        match fields.iter().find(|(_, value)| *value == 0) {
            Some((field, _)) => Err(PolicyError { field }),
            None => Ok(()),
        }
    }
}

/// Why a provider declaration cannot run a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationError {
    /// An operation is scoped to another provider or account.
    OtherScope {
        /// The operation's identity.
        operation: String,
        /// The operation's scope, as `provider/account on destination`.
        scope: String,
        /// The provider the declaration names.
        provider: String,
        /// The account the provider acts as.
        account: String,
    },
    /// An operation uses a tool the provider does not declare.
    UndeclaredTool {
        /// The operation's identity.
        operation: String,
        /// The undeclared tool.
        tool: String,
        /// The provider the declaration names.
        provider: String,
    },
}

impl fmt::Display for DeclarationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherScope {
                operation,
                scope,
                provider,
                account,
            } => write!(
                formatter,
                "operation {operation} is scoped to {scope}, but the provider acts as {provider}/{account}"
            ),
            Self::UndeclaredTool {
                operation,
                tool,
                provider,
            } => write!(
                formatter,
                "operation {operation} uses tool {tool}, which provider {provider} does not declare"
            ),
        }
    }
}

impl std::error::Error for DeclarationError {}

/// A policy field that must be positive is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyError {
    /// The field's name, as serialized.
    pub field: &'static str,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} must be positive", self.field)
    }
}

impl std::error::Error for PolicyError {}

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
    /// Concurrency and the attempt and probe budgets.
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
/// the plan's tools (including its retention policy), and the policy. Changing
/// any of them changes the binding.
#[must_use]
pub fn approval_binding(
    plan: &Digest,
    declaration: &ProviderDeclaration,
    policy: &RecoveryPolicy,
) -> Digest {
    let value = serde_json::json!({
        "format": "codemode-approval/3",
        "plan": plan,
        "declaration": declaration,
        "policy": policy,
    });
    Digest::of(canonical_json(value).as_bytes())
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
    /// A short explanation without argument or result values, for example "receipt
    /// found". Kept only under [`Retention::Summary`].
    pub evidence: String,
}

/// The deadline and cancellation signal for an authorization check or a probe
/// outside a dispatch run.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    /// The session's absolute deadline.
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
    /// when it applies. Receives the session's deadline and cancellation.
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
        /// Serialized size of the result; the result itself is never kept.
        bytes: u64,
    },
    /// The provider returned an error.
    Failed {
        /// The error, with a bounded message.
        error: ProviderError,
        /// The provider's statement about the effect.
        effect: FailureEffect,
    },
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
            | Self::Unobserved { .. } => Conclusion::Unknown,
        }
    }
}

/// One fact about an operation, in journal order, as [`assess`] consumes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fact<'a> {
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
    /// A reconciliation probe, charged when it started, with its verdict, or `None`
    /// when the probe failed or its answer was never recorded.
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
/// An attempt that may have reached the provider stays unknown until its own
/// observed result, a reconciliation, or a review settles it. A later attempt's
/// refusal or non-application never settles an earlier unknown attempt. A
/// `state` reconciliation can only show that the desired state holds, so its
/// `applied` counts as `satisfied` and its `not_applied` as `unknown`.
/// `probed_this_session` prevents probing the same operation twice in one session.
#[must_use]
pub(crate) fn assess(
    facts: &[Fact<'_>],
    contract: RecoveryContract,
    policy: &RecoveryPolicy,
    probed_this_session: bool,
) -> Assessment {
    let mut conclusion = Conclusion::Pending;
    let mut basis = Basis::None;
    let mut attempts = 0;
    let mut probes = 0;
    // Attempts whose outcome is unknown and not yet settled.
    let mut open = std::collections::BTreeSet::new();
    for fact in facts {
        match *fact {
            Fact::Intent { number } => {
                attempts = attempts.max(number);
                open.insert(number);
                conclusion = Conclusion::Unknown;
                basis = Basis::None;
            }
            Fact::Observation { number, observed } => {
                basis = Basis::Observed;
                conclusion = match observed.conclusion() {
                    Conclusion::Applied => {
                        open.clear();
                        Conclusion::Applied
                    }
                    Conclusion::Unknown => Conclusion::Unknown,
                    settled => {
                        open.remove(&number);
                        if open.is_empty() {
                            settled
                        } else {
                            Conclusion::Unknown
                        }
                    }
                };
            }
            Fact::Probe(verdict) => {
                probes += 1;
                let Some(verdict) = verdict else { continue };
                let verdict = match (contract.reconcile, verdict) {
                    (ReconcileMethod::State, Verdict::Applied) => Verdict::Satisfied,
                    (ReconcileMethod::State, Verdict::NotApplied) => Verdict::Unknown,
                    (_, verdict) => verdict,
                };
                basis = Basis::Reconciled;
                if verdict != Verdict::Unknown {
                    open.clear();
                }
                conclusion = match verdict {
                    Verdict::Applied => Conclusion::Applied,
                    Verdict::Satisfied => Conclusion::Satisfied,
                    Verdict::NotApplied => Conclusion::NotApplied,
                    Verdict::Unknown => Conclusion::Unknown,
                };
            }
            Fact::Review(resolution) => {
                basis = Basis::Review;
                open.clear();
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
    fn only_short_ascii_codes_are_machine_codes() {
        let longest = "a".repeat(64);
        for code in [
            "rate_limited",
            "github_status_404",
            "v1.2:x-y",
            longest.as_str(),
        ] {
            assert_eq!(machine_code(code), code);
        }
        let too_long = "a".repeat(65);
        for code in ["", "two words", "token=abc", "é", too_long.as_str()] {
            assert_eq!(machine_code(code), UNRECOGNIZED_CODE, "{code:?}");
        }
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

    #[test]
    fn a_later_attempt_settles_only_itself() {
        let not_applied = Observed::Failed {
            error: ProviderError {
                code: "rate_limited".into(),
                message: "slow down".into(),
            },
            effect: FailureEffect::NotApplied,
        };
        let refused = Observed::Failed {
            error: ProviderError {
                code: "precondition_failed".into(),
                message: "closed".into(),
            },
            effect: FailureEffect::Refused,
        };
        let repeat = RecoveryContract {
            repeat: RepeatSafety::Safe,
            reconcile: ReconcileMethod::None,
        };
        // Attempt 1 may have applied; attempts 2 and 3 say nothing about it.
        let facts = [
            Fact::Intent { number: 1 },
            Fact::Observation {
                number: 1,
                observed: &Observed::Unobserved {
                    reason: "deadline".into(),
                },
            },
            Fact::Intent { number: 2 },
            Fact::Observation {
                number: 2,
                observed: &not_applied,
            },
            Fact::Intent { number: 3 },
            Fact::Observation {
                number: 3,
                observed: &refused,
            },
        ];
        assert_eq!(
            step(&facts, repeat),
            (Conclusion::Unknown, NextStep::Review)
        );
    }

    #[test]
    fn a_state_reading_never_proves_our_effect_or_its_absence() {
        let state = RecoveryContract {
            repeat: RepeatSafety::Unsafe,
            reconcile: ReconcileMethod::State,
        };
        let not_applied = [
            Fact::Intent { number: 1 },
            Fact::Probe(Some(Verdict::NotApplied)),
        ];
        let assessment = assess(&not_applied, state, &RecoveryPolicy::default(), true);
        assert_eq!(
            (assessment.conclusion, assessment.next),
            (Conclusion::Unknown, NextStep::Review)
        );
        let applied = [
            Fact::Intent { number: 1 },
            Fact::Probe(Some(Verdict::Applied)),
        ];
        assert_eq!(step(&applied, state).0, Conclusion::Satisfied);
    }
}
