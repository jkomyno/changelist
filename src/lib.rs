//! Turn many writes into one change list that a person reads and approves
//! once, then send exactly that list through one MCP server.
//!
//! - [`plan_file`] is the one file a person reviews and approves: the writes,
//!   the server they go to, and what was known about each tool they use. One
//!   digest names what an approval binds.
//! - [`mcp`] is the client: it lists a server's tools, decides what a plan may
//!   do with each one, and calls them.
//! - [`Journal`] and [`run_session`] send an approved plan. Every attempt is
//!   recorded before it is sent, the first write goes alone, a failed write
//!   stops the rest, and a write is sent again only when that is known to be
//!   safe.
//! - [`status`] says what happened to each write, and [`render`] turns a plan
//!   or a status into text that cannot hide or fake a line.
//! - [`run`] executes a program that proposes a change list in a fresh worker
//!   process embedding V8, named by [`RunConfig::worker`]. The worker
//!   receives no host environment, refuses every import, and exposes no
//!   filesystem, network, environment, process, or FFI APIs. Its stdout is
//!   untrusted protocol input. Rust validates every call and owns call
//!   identities, provider execution, limits, and the resulting trace. Every
//!   tool must be authorized by the host: [`RunConfig::default`] authorizes
//!   none. [`run_calls`] submits finite host-owned requests without a worker.
//!
//! The non-default `testkit` feature adds `MockProvider` with local
//! deterministic tools, and builds the offline MCP server `codemode-mcp-fixture`,
//! for tests and demos. Neither is part of the default API.

mod embedded;
mod executor;
mod journal;
mod json;
pub mod mcp;
mod plan;
pub mod plan_file;
mod plan_inspect;
mod recovery;
pub mod render;
mod runtime;
pub mod status;

pub use embedded::{EmbeddedWorkerError, run_embedded_worker};
pub use executor::{
    OperationStatus, SessionError, SessionOptions, SessionOutcome, SessionTiming, StoppedEarly,
    assess_history, run_session,
};
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
pub use plan_inspect::{
    ApprovalSummary, AttemptLine, JournalInspectOptions, JournalInspection, OperationReport,
    PlanSummary, ProbeLine, ReviewLine, SessionState, SessionSummary, inspect_journal,
};
pub use recovery::{
    Approval, Assessment, Attempt, AttemptError, Basis, Conclusion, DeclarationError,
    FailureEffect, NextStep, Observed, PolicyError, ProbeContext, ProviderDeclaration, RateLimit,
    ReconcileMethod, Reconciliation, RecoverableProvider, RecoveryContract, RecoveryPolicy,
    RepeatSafety, Resolution, Retention, UNRECOGNIZED_CODE, Verdict, approval_binding,
};
#[cfg(feature = "testkit")]
pub use runtime::MockProvider;
pub use runtime::{
    ByteTotals, CallContext, CallOutcome, CallRecord, ClockInfo, ExecutionBackend, FactSink,
    NativeCall, ProviderError, ProviderFact, ProviderFactKind, REPORT_SCHEMA_VERSION,
    RunCancellation, RunConfig, RunError, RunEvent, RunLimits, RunReport, RunStatus, ToolCall,
    ToolProvider, WorkerExit, run, run_calls,
};
