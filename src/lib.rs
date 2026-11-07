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

mod bridge;
mod runtime;

pub use bridge::{BRIDGE_FRAME_BYTES, BRIDGE_PROTOCOL_VERSION, BridgeError, serve_bridge};
pub use runtime::{
    CallContext, CallOutcome, CallRecord, MockProvider, ProviderError, RunCancellation, RunConfig,
    RunError, RunEvent, RunReport, RunStatus, ToolCall, ToolProvider, run,
};
