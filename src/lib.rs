//! Execute bounded TypeScript programs with host-authorized tool calls.
//!
//! The Deno subprocess receives no host environment and denies guest filesystem,
//! network, environment, and process permissions. Its stdout is untrusted protocol
//! input; Rust validates every call and owns call identities, provider execution,
//! limits, and the resulting trace.
//! Built-in and inline data modules and raw child stdio remain accessible.
//!
//! [`run`] takes any [`ToolProvider`]. [`serve_bridge`] supplies one whose calls
//! are executed by a parent host, such as an agent extension, over a versioned
//! stdio protocol; the host keeps its permissions, approvals, and credentials.
//! [`serve_plan_bridge`] does the same for an approved plan's session: the host
//! executes and reconciles operations while Rust keeps the journal.
//!
//! [`fixture`] is an offline issue tracker with its own side-effect ledger, for
//! demos, tests, and baselines of recoverable operations.

mod bridge;
mod executor;
pub mod fixture;
mod inspect;
mod journal;
mod plan;
mod plan_bridge;
mod plan_inspect;
mod recovery;
mod runtime;

pub use bridge::{BRIDGE_FRAME_BYTES, BRIDGE_PROTOCOL_VERSION, BridgeError, serve_bridge};
pub use executor::{
    OperationStatus, SessionError, SessionOptions, SessionOutcome, SessionTiming, assess_history,
    dispatcher_program, run_session,
};
pub use inspect::{
    CallRow, CallTiming, Calls, Evidence, InspectOptions, Inspection, Phase, Reconcile,
    ReconcileItem, Sections, Timing, inspect,
};
pub use journal::{
    AttemptRecord, Durability, History, JOURNAL_FORMAT_VERSION, Journal, JournalError,
    ObservationRecord, ProbeRecord, ReviewRecord, SessionEnd, SessionKind, SessionRecord,
    UNANSWERED,
};
pub use plan::{
    Digest, FrozenPlan, MAX_ARGUMENT_BYTES, MAX_OPERATIONS, MAX_PLAN_BYTES, Operation,
    PLAN_FORMAT_VERSION, Plan, PlanError, Scope,
};
pub use plan_bridge::{PLAN_BRIDGE_PROTOCOL_VERSION, PlanBridgeError, serve_plan_bridge};
pub use plan_inspect::{
    ApprovalSummary, AttemptLine, JournalInspectOptions, JournalInspection, OperationReport,
    PlanSummary, ProbeLine, ReviewLine, SessionSummary, inspect_journal,
};
pub use recovery::{
    Approval, Assessment, Attempt, AttemptError, Basis, Conclusion, DeclarationError, Fact,
    FailureEffect, NextStep, Observed, PolicyError, ProbeContext, ProviderDeclaration,
    ReconcileMethod, Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy,
    RepeatSafety, Resolution, Verdict, approval_binding, assess,
};
pub use runtime::{
    BridgeTotals, ByteTotals, CallContext, CallOutcome, CallRecord, ClockInfo, DispatchPolicy,
    FactSink, MockProvider, PreparedGuest, ProviderError, ProviderFact, ProviderFactKind,
    REPORT_SCHEMA_VERSION, RunCancellation, RunConfig, RunError, RunEvent, RunLimits, RunReport,
    RunStatus, ToolCall, ToolProvider, run, run_prepared,
};
