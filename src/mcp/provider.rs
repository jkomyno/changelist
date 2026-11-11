//! An MCP server as a codemode provider: read tools for a program that
//! investigates, and write tools for an approved plan.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::client::{McpClient, McpError};
use super::config::ServerConfig;
use super::transport::RequestError;
use super::wire::{self, Tool, ToolOutcome};
use crate::plan::{Operation, Scope};
use crate::recovery::{
    Attempt, AttemptError, FailureEffect, ProbeContext, ProviderDeclaration, ReconcileMethod,
    Reconciliation, RecoverableProvider, RecoveryContract, RepeatSafety, Retention,
};
use crate::runtime::{CallContext, ProviderError, ToolCall, ToolProvider};

/// The provider name in every scope and declaration of an MCP server.
pub const MCP_PROVIDER: &str = "mcp";

/// Longest wait for a server to agree on a version and list its tools.
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What codemode may do with a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    /// A program may call it while it investigates. It cannot be in a plan.
    Read,
    /// A plan may hold it. After a lost reply it is sent again, because the
    /// trusted server declares that repeating it has no further effect.
    RepeatableWrite,
    /// A plan may hold it. After a lost reply it is not sent again: a person
    /// decides.
    Write,
}

struct Inner {
    client: McpClient,
    identity: String,
    tools: BTreeMap<String, (Tool, ToolClass)>,
    warnings: Vec<String>,
}

/// One MCP server, connected, with its tools classified.
#[derive(Clone)]
pub struct McpProvider(Arc<Inner>);

impl std::fmt::Debug for McpProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpProvider")
            .field("identity", &self.0.identity)
            .field("tools", &self.0.tools.len())
            .finish_non_exhaustive()
    }
}

impl McpProvider {
    /// Connect to the server and list its tools once.
    ///
    /// # Errors
    /// Returns an [`McpError`] when the server cannot be reached, shares no
    /// protocol revision, returns an unusable tool list, or does not finish
    /// within 60 seconds, and when `read_tools` names a tool the server does
    /// not list.
    pub async fn connect(config: &ServerConfig) -> Result<Self, McpError> {
        let setup = async {
            let client = McpClient::connect(config).await?;
            let listed = client.list_tools().await?;
            Ok::<_, McpError>((client, listed))
        };
        let (client, (listed, warnings)) = tokio::time::timeout(SETUP_TIMEOUT, setup)
            .await
            .map_err(|_| {
                McpError::Start(format!(
                    "the server did not finish version negotiation and tools/list within {} seconds",
                    SETUP_TIMEOUT.as_secs()
                ))
            })??;
        for name in &config.read_tools {
            if !listed.iter().any(|tool| &tool.name == name) {
                return Err(McpError::Protocol(format!(
                    "\"read_tools\" names {name:?}, which the server does not list"
                )));
            }
        }
        let tools = listed
            .into_iter()
            .map(|tool| {
                let class = classify(&tool, config);
                (tool.name.clone(), (tool, class))
            })
            .collect();
        Ok(Self(Arc::new(Inner {
            client,
            identity: config.identity(),
            tools,
            warnings,
        })))
    }

    /// The name an approval binds as the account; see [`ServerConfig::identity`].
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.0.identity
    }

    /// End the connection; see [`McpClient::close`].
    pub async fn close(&self) {
        self.0.client.close().await;
    }

    /// The protocol revision in use.
    #[must_use]
    pub fn protocol_version(&self) -> &str {
        self.0.client.protocol_version()
    }

    /// Every usable tool with its class, in name order.
    pub fn tools(&self) -> impl Iterator<Item = (&Tool, ToolClass)> {
        self.0.tools.values().map(|(tool, class)| (tool, *class))
    }

    /// One tool and its class.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<(&Tool, ToolClass)> {
        self.0.tools.get(name).map(|(tool, class)| (tool, *class))
    }

    /// Why listed tools were left out, one line per tool.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.0.warnings
    }

    /// The names of the tools a program may call while it investigates.
    #[must_use]
    pub fn read_tools(&self) -> Vec<String> {
        self.tools()
            .filter(|(_, class)| *class == ToolClass::Read)
            .map(|(tool, _)| tool.name.clone())
            .collect()
    }

    /// The scope of every operation that acts through this server.
    #[must_use]
    pub fn scope(&self) -> Scope {
        Scope {
            provider: MCP_PROVIDER.into(),
            account: self.0.identity.clone(),
            destination: None,
        }
    }
}

/// Annotations count only when a person said this server is trusted.
fn classify(tool: &Tool, config: &ServerConfig) -> ToolClass {
    let hints = tool.annotations;
    let trusted = config.trust_annotations;
    if config.read_tools.contains(&tool.name) || (trusted && hints.read_only_hint == Some(true)) {
        ToolClass::Read
    } else if trusted && hints.idempotent_hint == Some(true) {
        ToolClass::RepeatableWrite
    } else {
        ToolClass::Write
    }
}

fn provider_error(code: impl Into<String>, message: impl std::fmt::Display) -> ProviderError {
    ProviderError {
        code: code.into(),
        message: wire::truncate(&message.to_string(), wire::MAX_MESSAGE_BYTES),
    }
}

/// Wait for `call`, the deadline, or cancellation, whichever comes first.
/// `None` means the call was abandoned; its effect is then unknown.
async fn bounded<T>(
    context: &CallContext,
    call: impl Future<Output = T>,
) -> Result<T, ProviderError> {
    tokio::select! {
        outcome = call => Ok(outcome),
        () = tokio::time::sleep_until(context.deadline) => {
            Err(provider_error("deadline", "the deadline passed before the server answered"))
        }
        () = context.cancellation.cancelled() => {
            Err(provider_error("cancelled", "cancelled before the server answered"))
        }
    }
}

/// Calls from a program that investigates: read tools only.
impl ToolProvider for McpProvider {
    async fn execute(&self, call: ToolCall, context: CallContext) -> Result<Value, ProviderError> {
        let Some((tool, ToolClass::Read)) = self.tool(&call.tool) else {
            return Err(provider_error(
                "not_a_read_tool",
                format!("{:?} is not a read tool of this server", call.tool),
            ));
        };
        match bounded(&context, self.0.client.call_tool(tool, &call.arguments)).await? {
            Ok(ToolOutcome::Value(value)) => Ok(value),
            Ok(ToolOutcome::ToolError(message)) => Err(provider_error("tool_error", message)),
            Ok(ToolOutcome::InputRequired) => Err(input_required()),
            Err(error) => Err(provider_error(error.code(), &error)),
        }
    }
}

fn input_required() -> ProviderError {
    provider_error(
        "input_required",
        "the server asked for input (elicitation, sampling, or roots) that codemode cannot give",
    )
}

/// Attempts of an approved plan: write tools only.
impl RecoverableProvider for McpProvider {
    fn declaration(&self) -> ProviderDeclaration {
        let mut contracts = BTreeMap::new();
        let mut definitions = BTreeMap::new();
        for (tool, class) in self.tools() {
            let repeat = match class {
                ToolClass::Read => continue,
                ToolClass::RepeatableWrite => RepeatSafety::Safe,
                ToolClass::Write => RepeatSafety::Unsafe,
            };
            // MCP has no way to ask whether an earlier call took effect.
            let contract = RecoveryContract {
                repeat,
                reconcile: ReconcileMethod::None,
            };
            contracts.insert(tool.name.clone(), contract);
            definitions.insert(tool.name.clone(), tool.definition_digest());
        }
        ProviderDeclaration {
            provider: MCP_PROVIDER.into(),
            account: self.0.identity.clone(),
            contracts,
            retention: Retention::Metadata,
            definitions,
        }
    }

    async fn authorize(&self, scope: &Scope, _context: ProbeContext) -> Result<(), ProviderError> {
        if scope.provider != MCP_PROVIDER || scope.account != self.0.identity {
            return Err(provider_error(
                "other_server",
                "the scope names another server",
            ));
        }
        if scope.destination.is_some() {
            return Err(provider_error(
                "destination_unsupported",
                "an MCP server gives codemode no way to restrict a call to a destination",
            ));
        }
        Ok(())
    }

    async fn execute(&self, attempt: Attempt, context: CallContext) -> Result<Value, AttemptError> {
        let refused = |error| AttemptError {
            error,
            effect: FailureEffect::Refused,
        };
        let unknown = |error| AttemptError {
            error,
            effect: FailureEffect::Unknown,
        };
        let operation = &attempt.operation;
        let tool = match self.tool(&operation.tool) {
            Some((tool, ToolClass::RepeatableWrite | ToolClass::Write)) => tool,
            _ => {
                return Err(refused(provider_error(
                    "not_a_write_tool",
                    format!("{:?} is not a write tool of this server", operation.tool),
                )));
            }
        };
        if !operation.preconditions.is_empty() {
            return Err(refused(provider_error(
                "preconditions_unsupported",
                "an MCP server cannot check a precondition when it applies a call",
            )));
        }
        let call = self.0.client.call_tool(tool, &operation.arguments);
        match bounded(&context, call).await.map_err(unknown)? {
            Ok(ToolOutcome::Value(value)) => Ok(value),
            // A tool error does not say whether part of the work was done.
            Ok(ToolOutcome::ToolError(message)) => {
                Err(unknown(provider_error("tool_error", message)))
            }
            Ok(ToolOutcome::InputRequired) => Err(unknown(input_required())),
            Err(error) => Err(AttemptError {
                effect: match &error {
                    RequestError::NotSent(_) => FailureEffect::NotApplied,
                    error if error.not_processed() => FailureEffect::Refused,
                    _ => FailureEffect::Unknown,
                },
                error: provider_error(error.code(), &error),
            }),
        }
    }

    async fn reconcile(
        &self,
        _operation: Operation,
        _keys: Vec<String>,
        _context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        // Never called: every contract declares `reconcile: none`.
        Err(provider_error(
            "reconcile_unsupported",
            "an MCP server offers no reconciliation",
        ))
    }
}
