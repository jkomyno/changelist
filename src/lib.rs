//! Execute bounded TypeScript programs or finite host-owned tool calls.
//!
//! [`run`] executes each program in a fresh worker process embedding V8, named by
//! [`RunConfig::worker`]. The worker receives no host environment, refuses every
//! import, and exposes no filesystem, network, environment, process, or FFI APIs.
//! Its stdout is untrusted protocol input; Rust validates every call and owns call
//! identities, provider execution, limits, and the resulting trace.
//!
//! [`run_calls`] submits finite host-owned requests without a guest process.
//! [`run`] takes any [`ToolProvider`]. [`serve_bridge`] supplies one whose calls
//! are executed by a parent host, such as an agent extension, over a versioned
//! stdio protocol; the host keeps its permissions, approvals, and credentials.
//! [`serve_plan_bridge`] does the same for an approved plan's session: the host
//! executes and reconciles operations while Rust keeps the journal.
//!
//! Every tool must be authorized by the host: [`RunConfig::default`] authorizes none.
//!
//! The non-default `testkit` feature adds `fixture`, an offline issue tracker
//! with its own side-effect ledger, `MockProvider`, local deterministic tools, and
//! `journal_commits`, a count of journal write transactions, for tests, demos, and
//! evaluations. None is part of the default API.

mod bridge;
mod embedded;
mod executor;
#[cfg(feature = "testkit")]
pub mod fixture;
mod inspect;
mod journal;
mod json;
pub mod mcp;
mod plan;
mod plan_bridge;
mod plan_inspect;
mod recovery;
mod runtime;

pub use bridge::{BRIDGE_FRAME_BYTES, BRIDGE_PROTOCOL_VERSION, BridgeError, serve_bridge};
pub use embedded::{EmbeddedWorkerError, run_embedded_worker};
pub use executor::{
    OperationStatus, SessionError, SessionOptions, SessionOutcome, SessionTiming, assess_history,
    run_session,
};
pub use inspect::{
    CallRow, CallTiming, Calls, Evidence, InspectOptions, Inspection, Phase, Reconcile,
    ReconcileItem, Sections, Timing, inspect,
};
#[cfg(feature = "testkit")]
pub use journal::journal_commits;
pub use journal::{
    AttemptRecord, Durability, History, JOURNAL_FORMAT_VERSION, Journal, JournalError,
    JournalReader, ObservationRecord, ProbeRecord, ReviewRecord, SessionEnd, SessionKind,
    SessionRecord, UNANSWERED,
};
pub use json::{parse_json, value as deserialize_json};
pub use plan::{
    Digest, FrozenPlan, MAX_ARGUMENT_BYTES, MAX_OPERATIONS, MAX_PLAN_BYTES, Operation,
    PLAN_FORMAT_VERSION, Plan, PlanError, Scope,
};
pub use plan_bridge::{PLAN_BRIDGE_PROTOCOL_VERSION, PlanBridgeError, serve_plan_bridge};
pub use plan_inspect::{
    ApprovalSummary, AttemptLine, JournalInspectOptions, JournalInspection, OperationReport,
    PlanSummary, ProbeLine, ReviewLine, SessionState, SessionSummary, inspect_journal,
};
pub use recovery::{
    Approval, Assessment, Attempt, AttemptError, Basis, Conclusion, DeclarationError,
    FailureEffect, NextStep, Observed, PolicyError, ProbeContext, ProviderDeclaration,
    ReconcileMethod, Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy,
    RepeatSafety, Resolution, Retention, UNRECOGNIZED_CODE, Verdict, approval_binding,
};
#[cfg(feature = "testkit")]
pub use runtime::MockProvider;
pub use runtime::{
    BridgeTotals, ByteTotals, CallContext, CallOutcome, CallRecord, ClockInfo, ExecutionBackend,
    FactSink, NativeCall, ProviderError, ProviderFact, ProviderFactKind, REPORT_SCHEMA_VERSION,
    RunCancellation, RunConfig, RunError, RunEvent, RunLimits, RunReport, RunStatus, ToolCall,
    ToolProvider, WorkerExit, run, run_calls,
};
