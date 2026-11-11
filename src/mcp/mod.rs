//! Use any Model Context Protocol server as a codemode provider.
//!
//! [`McpProvider`] connects to one server over Streamable HTTP or stdio, lists
//! its tools, and sorts them into three classes ([`ToolClass`]):
//!
//! - **read**: a program may call it while it investigates;
//! - **repeatable write**: a plan may hold it, and it is sent again after a lost reply;
//! - **write**: a plan may hold it, and after a lost reply a person decides.
//!
//! A tool is a read or a repeatable write only when a person trusts the
//! server's annotations ([`ServerConfig::trust_annotations`]) or names the tool
//! in [`ServerConfig::read_tools`]. Without that, every tool is a write: the
//! specification calls annotations hints that a client must not rely on unless
//! it trusts the server.
//!
//! The client speaks revision 2026-07-28 (stateless, no handshake) and falls
//! back to the `initialize` handshake of 2025-11-25, 2025-06-18, and 2025-03-26
//! for older servers. It declares no client capabilities, so a server cannot
//! ask it for elicitation, sampling, or roots; a tool that needs them fails.
//!
//! MCP gives a client no way to ask whether an earlier call took effect, so no
//! tool has a reconciliation method. An approval binds the server's identity
//! and a digest of each tool's name, argument schema, and annotations.

mod client;
mod config;
mod provider;
mod transport;
pub(crate) mod wire;

pub use client::{McpClient, McpError};
pub use config::{ConfigError, Endpoint, MAX_SERVER_CONFIG_BYTES, ServerConfig};
pub use provider::{MCP_PROVIDER, McpProvider, ToolClass};
pub use transport::RequestError;
pub use wire::{Annotations, LEGACY_VERSIONS, MODERN_VERSION, RpcError, Tool, ToolOutcome};
