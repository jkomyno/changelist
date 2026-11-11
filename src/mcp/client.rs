//! An MCP client: find out which revision a server speaks, list its tools,
//! and call them.

use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};

use super::config::{Endpoint, ServerConfig};
use super::transport::{Era, Http, Request, RequestError, Stdio, Transport};
use super::wire::{self, LEGACY_VERSIONS, MODERN_VERSION, Tool, ToolOutcome};

/// Longest wait for an older stdio server to answer the version probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Most `tools/list` pages read from one server.
const MAX_TOOL_PAGES: usize = 64;
/// Most tools kept from one server.
const MAX_TOOLS: usize = 2048;

/// Why a server could not be used.
#[derive(Debug, Clone, PartialEq)]
pub enum McpError {
    /// The transport could not be set up.
    Start(String),
    /// A request failed.
    Request {
        /// The MCP method.
        method: &'static str,
        /// How far it got.
        error: Box<RequestError>,
    },
    /// The server and this client share no protocol revision.
    Version(String),
    /// The server's answer broke the protocol.
    Protocol(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(reason) | Self::Version(reason) | Self::Protocol(reason) => {
                formatter.write_str(reason)
            }
            Self::Request { method, error } => write!(formatter, "{method}: {error}"),
        }
    }
}

impl std::error::Error for McpError {}

impl McpError {
    /// The same error with every one of `values` replaced by `[redacted]` in
    /// its text. A server can repeat a header or a query value it received in
    /// the error it answers with, also while this client connects.
    #[must_use]
    pub(crate) fn redacted(self, values: &[String]) -> Self {
        let clean = |text: String| super::config::redact(&text, values);
        match self {
            Self::Start(reason) => Self::Start(clean(reason)),
            Self::Version(reason) => Self::Version(clean(reason)),
            Self::Protocol(reason) => Self::Protocol(clean(reason)),
            Self::Request { method, error } => Self::Request {
                method,
                error: Box::new(error.redacted(values)),
            },
        }
    }
}

/// A connection to one MCP server.
pub struct McpClient {
    transport: Transport,
    /// `None` for 2026-07-28; the negotiated version for an older server.
    legacy: Option<String>,
}

impl fmt::Debug for McpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpClient")
            .field("protocol_version", &self.protocol_version())
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Reach the server and agree on a protocol revision.
    ///
    /// The client first sends `server/discover` as a 2026-07-28 request. A
    /// server that answers it is used statelessly. A server that does not is
    /// taken to be an older one, and the client falls back to its `initialize`
    /// handshake.
    ///
    /// # Errors
    /// Returns [`McpError::Start`] when the server cannot be reached or
    /// started, [`McpError::Version`] when no revision is shared, and
    /// [`McpError::Request`] when the server refuses the client, for example
    /// with HTTP status 401.
    pub async fn connect(config: &ServerConfig) -> Result<Self, McpError> {
        let transport = match &config.endpoint {
            Endpoint::Http { url, headers } => {
                Transport::Http(Http::new(url.clone(), headers).map_err(McpError::Start)?)
            }
            Endpoint::Stdio { command, args, env } => {
                let redactions = config.redactions();
                Transport::Stdio(
                    Stdio::start(command, args, env, redactions).map_err(McpError::Start)?,
                )
            }
        };
        let mut client = Self {
            transport,
            legacy: None,
        };
        let probe = tokio::time::timeout(
            PROBE_TIMEOUT,
            client.request("server/discover", json!({}), None, Vec::new()),
        )
        .await;
        match probe {
            Ok(Ok(result)) => {
                let supported = versions(result.get("supportedVersions"));
                if !supported.iter().any(|version| version == MODERN_VERSION) {
                    return Err(no_common_version(&supported));
                }
            }
            Ok(Err(RequestError::Rpc { error, .. }))
                if wire::MODERN_ERROR_CODES.contains(&error.code) =>
            {
                // A 2026-07-28 or later server that does not accept this request.
                if error.code == wire::UNSUPPORTED_VERSION {
                    let data = error.data.unwrap_or_default();
                    let supported = versions(data.get("supported"));
                    if supported
                        .iter()
                        .any(|version| LEGACY_VERSIONS.contains(&version.as_str()))
                    {
                        client.handshake().await?;
                        return Ok(client);
                    }
                    return Err(no_common_version(&supported));
                }
                return Err(McpError::Request {
                    method: "server/discover",
                    error: Box::new(RequestError::Rpc {
                        status: None,
                        error,
                    }),
                });
            }
            Ok(Err(error @ RequestError::NotSent(_))) => {
                return Err(McpError::Start(error.to_string()));
            }
            // A refusal that an `initialize` request would meet as well.
            Ok(Err(
                error @ (RequestError::Http { status: 401 | 403 }
                | RequestError::RateLimited { .. }),
            )) => {
                return Err(McpError::Request {
                    method: "server/discover",
                    error: Box::new(error),
                });
            }
            // Any other answer, or none: an older, handshake-based server.
            Ok(Err(_)) | Err(_) => client.handshake().await?,
        }
        Ok(client)
    }

    async fn handshake(&mut self) -> Result<(), McpError> {
        let failed = |error| McpError::Request {
            method: "initialize",
            error: Box::new(error),
        };
        let params = json!({
            "protocolVersion": LEGACY_VERSIONS[0],
            "capabilities": {},
            "clientInfo": wire::client_info(),
        });
        let result = self
            .transport
            .request(Request {
                method: "initialize",
                params,
                name: None,
                headers: Vec::new(),
                era: Era::Handshake,
            })
            .await
            .map_err(failed)?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !LEGACY_VERSIONS.contains(&version) {
            return Err(no_common_version(&[version.to_string()]));
        }
        self.legacy = Some(version.to_string());
        self.transport
            .notify("notifications/initialized", json!({}), Era::Legacy(version))
            .await
            .map_err(|error| McpError::Request {
                method: "notifications/initialized",
                error: Box::new(error),
            })
    }

    /// End the connection. A stdio server has its input closed and up to five
    /// seconds to exit before it is killed; a Streamable HTTP server keeps no
    /// connection state. Dropping the client without this kills a stdio server
    /// at once.
    pub async fn close(&self) {
        self.transport.close().await;
    }

    /// The protocol revision in use.
    #[must_use]
    pub fn protocol_version(&self) -> &str {
        self.legacy.as_deref().unwrap_or(MODERN_VERSION)
    }

    async fn request(
        &self,
        method: &str,
        mut params: Value,
        name: Option<&str>,
        headers: Vec<(String, String)>,
    ) -> Result<Value, RequestError> {
        let era = match &self.legacy {
            None => {
                params["_meta"] = wire::request_meta();
                Era::Modern
            }
            Some(version) => Era::Legacy(version),
        };
        self.transport
            .request(Request {
                method,
                params,
                name,
                headers,
                era,
            })
            .await
    }

    /// List every tool, following pagination.
    ///
    /// Returns the tools and a warning for each entry that was left out because
    /// its definition is not usable, as the specification requires for an
    /// invalid `x-mcp-header` annotation.
    ///
    /// # Errors
    /// Returns [`McpError::Request`] when a page cannot be read, and
    /// [`McpError::Protocol`] for a malformed list, a duplicate tool name, or
    /// a list that does not end.
    pub async fn list_tools(&self) -> Result<(Vec<Tool>, Vec<String>), McpError> {
        let mut tools: Vec<Tool> = Vec::new();
        let mut warnings = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self
                .request("tools/list", params, None, Vec::new())
                .await
                .map_err(|error| McpError::Request {
                    method: "tools/list",
                    error: Box::new(error),
                })?;
            if !matches!(
                result.get("resultType").and_then(Value::as_str),
                None | Some("complete")
            ) {
                return Err(McpError::Protocol(
                    "tools/list did not return a complete result".into(),
                ));
            }
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol("tools/list returned no tools array".into()))?;
            for entry in page {
                match Tool::parse(entry) {
                    Ok(tool) if tools.iter().any(|known| known.name == tool.name) => {
                        return Err(McpError::Protocol(format!(
                            "the server lists tool {:?} twice",
                            tool.name
                        )));
                    }
                    Ok(tool) => tools.push(tool),
                    Err(reason) => warnings.push(wire::truncate(&reason, wire::MAX_MESSAGE_BYTES)),
                }
            }
            if tools.len() > MAX_TOOLS {
                return Err(McpError::Protocol(format!(
                    "the server lists more than {MAX_TOOLS} tools"
                )));
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok((tools, warnings)),
            }
        }
        Err(McpError::Protocol(format!(
            "tools/list did not end after {MAX_TOOL_PAGES} pages"
        )))
    }

    /// Call `tool` once. There is no retry.
    ///
    /// # Errors
    /// Returns a [`RequestError`] that says how far the request got. Dropping
    /// the returned future abandons the request: on Streamable HTTP the
    /// connection closes, on stdio the server is sent `notifications/cancelled`.
    /// Neither proves that the tool had no effect.
    pub async fn call_tool(
        &self,
        tool: &Tool,
        arguments: &Value,
    ) -> Result<ToolOutcome, RequestError> {
        let headers = if self.transport.is_http() && self.legacy.is_none() {
            tool.parameter_headers(arguments)
        } else {
            Vec::new()
        };
        let params = json!({"name": tool.name, "arguments": arguments});
        let result = self
            .request("tools/call", params, Some(&tool.name), headers)
            .await?;
        ToolOutcome::parse(&result).map_err(RequestError::Invalid)
    }
}

fn versions(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect()
}

fn no_common_version(supported: &[String]) -> McpError {
    McpError::Version(format!(
        "the server supports MCP {supported:?}; this client supports {MODERN_VERSION} and {LEGACY_VERSIONS:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::RpcError;

    #[test]
    fn a_connect_time_error_never_carries_a_configured_value() {
        let values = vec![
            "api_key=QUERYSECRET9876".to_string(),
            "QUERYSECRET9876".to_string(),
            "sk-live-SECRET".to_string(),
        ];
        let echo = "invalid credentials: authorization=Bearer sk-live-SECRET target=/mcp?api_key=QUERYSECRET9876";
        let errors = [
            McpError::Request {
                method: "tools/list",
                error: Box::new(RequestError::Rpc {
                    status: Some(200),
                    error: RpcError {
                        code: -32001,
                        message: echo.into(),
                        data: None,
                    },
                }),
            },
            McpError::Start(echo.into()),
            McpError::Version(echo.into()),
            McpError::Protocol(echo.into()),
        ];
        for error in errors {
            let text = error.redacted(&values).to_string();
            assert!(
                text.ends_with(
                    "invalid credentials: authorization=Bearer [redacted] target=/mcp?[redacted]"
                ),
                "{text}"
            );
        }
    }
}
