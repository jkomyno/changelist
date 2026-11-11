//! The two standard MCP transports: Streamable HTTP and stdio.
//!
//! A transport sends one JSON-RPC request and returns its result, or says how
//! far the request got. That distinction matters for writes: a request that was
//! never sent did nothing, a request the server rejected did nothing, and a
//! request that was sent and not answered may have done anything.

use std::collections::HashMap;
use std::fmt;
use std::process::Stdio as ProcessStdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use super::wire::{self, Reply, RpcError};
use crate::runtime::lock;

/// Largest response body or stdio line read from a server.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// HTTP statuses a server or an intermediary sends before a tool can run.
/// Others, such as 409 or 422, can carry an application's own meaning.
const NOT_PROCESSED_STATUSES: [u16; 9] = [400, 401, 403, 404, 405, 406, 413, 415, 431];
/// Longest wait for a stdio server to exit after its input closes.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for a TCP and TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Which protocol rules a request follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Era<'a> {
    /// 2026-07-28: stateless, metadata in every request.
    Modern,
    /// The `initialize` request of an older revision.
    Handshake,
    /// An older revision after its handshake, with the negotiated version.
    Legacy(&'a str),
}

/// How far a request got when it did not return a result.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestError {
    /// Nothing was sent: the connection could not be made, or the server
    /// process had already ended.
    NotSent(String),
    /// The server answered with a JSON-RPC error.
    Rpc {
        /// The HTTP status, on Streamable HTTP.
        status: Option<u16>,
        /// The error.
        error: RpcError,
    },
    /// The server answered with an HTTP error status and no JSON-RPC error.
    Http {
        /// The status code.
        status: u16,
    },
    /// The request was sent and no response arrived.
    Lost(String),
    /// A response arrived that is not valid MCP.
    Invalid(String),
}

impl RequestError {
    /// Whether the server certainly did not act on the request: it was not
    /// sent, or it was rejected before a tool could run (HTTP status 400, 401,
    /// 403, 404, 405, 406, 413, 415, or 431, or JSON-RPC code -32700, -32601,
    /// -32020, -32021, or -32022). Every other failure leaves the effect unknown.
    #[must_use]
    pub fn not_processed(&self) -> bool {
        match self {
            Self::NotSent(_) => true,
            Self::Rpc { error, .. } => error.not_processed(),
            Self::Http { status } => NOT_PROCESSED_STATUSES.contains(status),
            Self::Lost(_) | Self::Invalid(_) => false,
        }
    }

    /// A short machine code for traces and journals.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::NotSent(_) => "not_sent".into(),
            Self::Rpc { error, .. } => format!("rpc_{}", error.code),
            Self::Http { status } => format!("http_{status}"),
            Self::Lost(_) => "no_response".into(),
            Self::Invalid(_) => "invalid_response".into(),
        }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(reason) => write!(formatter, "the request was not sent: {reason}"),
            Self::Rpc { error, .. } => write!(
                formatter,
                "the server returned error {}: {}",
                error.code,
                wire::truncate(&error.message, wire::MAX_MESSAGE_BYTES)
            ),
            Self::Http { status } => write!(formatter, "the server returned HTTP status {status}"),
            Self::Lost(reason) => write!(formatter, "no response arrived: {reason}"),
            Self::Invalid(reason) => write!(formatter, "the response is not valid MCP: {reason}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// One request to send.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub params: Value,
    /// The tool name, mirrored into `Mcp-Name` on Streamable HTTP.
    pub name: Option<&'a str>,
    /// `Mcp-Param-*` headers for Streamable HTTP.
    pub headers: Vec<(String, String)>,
    pub era: Era<'a>,
}

pub(crate) enum Transport {
    Http(Http),
    Stdio(Stdio),
}

impl Transport {
    pub(crate) async fn request(&self, request: Request<'_>) -> Result<Value, RequestError> {
        match self {
            Self::Http(http) => http.request(request).await,
            Self::Stdio(stdio) => stdio.request(request).await,
        }
    }

    pub(crate) async fn notify(
        &self,
        method: &str,
        params: Value,
        era: Era<'_>,
    ) -> Result<(), RequestError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match self {
            Self::Http(http) => http.post(&message, method, None, &[], era).await.map(drop),
            Self::Stdio(stdio) => stdio.write(&message).await,
        }
    }

    /// End the connection. A stdio server is given time to exit by itself.
    pub(crate) async fn close(&self) {
        if let Self::Stdio(stdio) = self {
            stdio.close().await;
        }
    }

    pub(crate) fn is_http(&self) -> bool {
        matches!(self, Self::Http(_))
    }
}

pub(crate) struct Http {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    next_id: AtomicU64,
    /// The `Mcp-Session-Id` an older server assigned during its handshake.
    session: Mutex<Option<HeaderValue>>,
}

impl Http {
    pub(crate) fn new(url: Url, headers: &[(String, String)]) -> Result<Self, String> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("header name {name:?} is not valid"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| format!("the value of header {name:?} is not valid"))?;
            // Keeps the value out of reqwest's debug output.
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let client = reqwest::Client::builder()
            // A redirect could carry the configured credentials to another host.
            .redirect(reqwest::redirect::Policy::none())
            // So could a proxy named by the environment, in clear text for http.
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| format!("cannot build the HTTP client: {error}"))?;
        Ok(Self {
            client,
            url,
            headers: map,
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
        })
    }

    async fn request(&self, request: Request<'_>) -> Result<Value, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0", "id": id, "method": request.method, "params": request.params,
        });
        let reply = self
            .post(
                &message,
                request.method,
                request.name,
                &request.headers,
                request.era,
            )
            .await?;
        let Some((status, body)) = reply else {
            return Err(RequestError::Invalid(
                "the server accepted the request without answering it".into(),
            ));
        };
        match body {
            Ok(result) => Ok(result),
            Err(error) => Err(RequestError::Rpc {
                status: Some(status),
                error,
            }),
        }
    }

    /// POST one message. Returns the status and the reply to it, or `None` when
    /// the server accepted a notification.
    async fn post(
        &self,
        message: &Value,
        method: &str,
        name: Option<&str>,
        parameter_headers: &[(String, String)],
        era: Era<'_>,
    ) -> Result<Option<(u16, Reply)>, RequestError> {
        let id = message.get("id").and_then(Value::as_u64);
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        let mut set = |name: &str, value: &str| -> Result<(), RequestError> {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| RequestError::NotSent(format!("header {name:?} is not valid")))?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                RequestError::NotSent(format!("the value of header {name:?} is not valid"))
            })?;
            headers.insert(name, value);
            Ok(())
        };
        match era {
            Era::Modern => {
                set("MCP-Protocol-Version", wire::MODERN_VERSION)?;
                set("Mcp-Method", method)?;
                if let Some(name) = name {
                    set("Mcp-Name", &wire::header_value(name))?;
                }
                for (header, value) in parameter_headers {
                    set(header, value)?;
                }
            }
            Era::Handshake => {}
            Era::Legacy(version) => {
                set("MCP-Protocol-Version", version)?;
                if let Some(session) = lock(&self.session).clone() {
                    headers.insert("Mcp-Session-Id", session);
                }
            }
        }
        let response = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .body(message.to_string())
            .send()
            .await
            .map_err(|error| {
                // Only a failed connection proves that nothing was sent.
                if error.is_connect() {
                    RequestError::NotSent(describe(&error))
                } else {
                    RequestError::Lost(describe(&error))
                }
            })?;
        let status = response.status().as_u16();
        if era == Era::Handshake
            && let Some(session) = response.headers().get("Mcp-Session-Id")
        {
            let mut session = session.clone();
            session.set_sensitive(true);
            *lock(&self.session) = Some(session);
        }
        let stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !response.status().is_success() {
            let body = read_body(response).await.unwrap_or_default();
            let error = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| wire::response(&value))
                .and_then(|(_, reply)| reply.err());
            return Err(match error {
                Some(error) => RequestError::Rpc {
                    status: Some(status),
                    error,
                },
                None => RequestError::Http { status },
            });
        }
        let Some(id) = id else {
            return Ok(None);
        };
        if stream {
            return read_event_stream(response, id)
                .await
                .map(|reply| Some((status, reply)));
        }
        let body = read_body(response).await?;
        if body.is_empty() {
            return Ok(None);
        }
        let text = std::str::from_utf8(&body)
            .map_err(|_| RequestError::Invalid("the body is not UTF-8".into()))?;
        let value = crate::json::parse_json(text)
            .map_err(|error| RequestError::Invalid(format!("the body is not JSON: {error}")))?;
        match wire::response(&value) {
            Some((Some(found), reply)) if found == id => Ok(Some((status, reply))),
            Some((None, Err(error))) => Ok(Some((status, Err(error)))),
            _ => Err(RequestError::Invalid(
                "the body is not a response to this request".into(),
            )),
        }
    }
}

/// An error's text without its URL, which may hold a session identifier.
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "the server");
    }
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    wire::truncate(&text, wire::MAX_MESSAGE_BYTES)
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, RequestError> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(RequestError::Invalid("the response is too large".into()));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) => return Err(RequestError::Lost(describe(&error))),
        }
    }
}

/// Read a request-scoped SSE stream until the response to `id` arrives.
/// Notifications and comments on the stream are skipped.
async fn read_event_stream(
    mut response: reqwest::Response,
    id: u64,
) -> Result<Reply, RequestError> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut total = 0;
    let mut data = String::new();
    // Bytes of `buffer` already searched for a line end.
    let mut scanned = 0;
    loop {
        while let Some(found) = buffer[scanned..].iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=scanned + found).collect();
            scanned = 0;
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                // A blank line ends one event.
                if !data.is_empty() {
                    let event = std::mem::take(&mut data);
                    if let Ok(value) = crate::json::parse_json(&event)
                        && let Some((found, reply)) = wire::response(&value)
                        && (found == Some(id) || (found.is_none() && reply.is_err()))
                    {
                        return Ok(reply);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // Comment lines, `event:`, `id:`, and `retry:` carry nothing we use.
        }
        scanned = buffer.len();
        match response.chunk().await {
            Ok(Some(chunk)) => {
                total += chunk.len();
                if total > MAX_RESPONSE_BYTES {
                    return Err(RequestError::Invalid("the response is too large".into()));
                }
                buffer.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return Err(RequestError::Lost(
                    "the response stream ended without a response".into(),
                ));
            }
            Err(error) => return Err(RequestError::Lost(describe(&error))),
        }
    }
}

type Pending = Arc<Mutex<PendingState>>;

#[derive(Default)]
struct PendingState {
    waiting: HashMap<u64, oneshot::Sender<Reply>>,
    /// Set when the server's output ended; no request can be answered after it.
    closed: Option<String>,
}

/// A line for the writer task, or the order to close the server's input.
enum Outgoing {
    Line(String, Option<oneshot::Sender<Result<(), WriteError>>>),
    Close,
}

/// Why a line did not reach the server.
enum WriteError {
    /// No byte of it was written.
    NotStarted(String),
    /// The write failed after it began; the server may have read the message.
    Partial(String),
}

impl From<WriteError> for RequestError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::NotStarted(reason) => Self::NotSent(reason),
            WriteError::Partial(reason) => Self::Lost(reason),
        }
    }
}

pub(crate) struct Stdio {
    /// To the one task that writes to the server, so every line is written
    /// whole even when the request that sent it is abandoned.
    writer: mpsc::UnboundedSender<Outgoing>,
    pending: Pending,
    next_id: AtomicU64,
    // Dropping the child kills the server.
    child: tokio::sync::Mutex<Child>,
}

impl Stdio {
    pub(crate) fn start(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(ProcessStdio::piped())
            .stdout(ProcessStdio::piped())
            .stderr(ProcessStdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start {command:?}: {error}"))?;
        let stdin = child.stdin.take().ok_or("the server has no stdin")?;
        let stdout = child.stdout.take().ok_or("the server has no stdout")?;
        let pending = Pending::default();
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            let reason = loop {
                line.clear();
                let mut limited = (&mut reader).take(MAX_RESPONSE_BYTES as u64 + 1);
                match limited.read_until(b'\n', &mut line).await {
                    Ok(0) => break "the server closed its output".to_string(),
                    Ok(_) if line.len() > MAX_RESPONSE_BYTES => {
                        break "the server wrote a line that is too large".to_string();
                    }
                    Ok(_) => {}
                    Err(error) => break format!("cannot read the server's output: {error}"),
                }
                let Ok(text) = std::str::from_utf8(&line) else {
                    continue;
                };
                let Ok(value) = crate::json::parse_json(text.trim()) else {
                    continue;
                };
                if let Some((Some(id), reply)) = wire::response(&value)
                    && let Some(waiter) = lock(&reader_pending).waiting.remove(&id)
                {
                    let _ = waiter.send(reply);
                }
            };
            let mut state = lock(&reader_pending);
            state.closed = Some(reason);
            // Dropping the senders tells every waiter that no reply will come.
            state.waiting.clear();
        });
        let (writer, lines) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(stdin, lines));
        Ok(Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            child: tokio::sync::Mutex::new(child),
        })
    }

    /// Shut the server down as the specification asks: close its input, wait
    /// for it to exit, and kill it only if it does not.
    async fn close(&self) {
        let _ = self.writer.send(Outgoing::Close);
        let mut child = self.child.lock().await;
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
        }
    }

    async fn write(&self, message: &Value) -> Result<(), RequestError> {
        let closed = || WriteError::NotStarted("the server's input is closed".into());
        let (written, result) = oneshot::channel();
        self.writer
            .send(Outgoing::Line(line_of(message), Some(written)))
            .map_err(|_| closed())?;
        Ok(result.await.map_err(|_| closed())??)
    }

    async fn request(&self, request: Request<'_>) -> Result<Value, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0", "id": id, "method": request.method, "params": request.params,
        });
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = lock(&self.pending);
            if let Some(reason) = &state.closed {
                return Err(RequestError::NotSent(reason.clone()));
            }
            state.waiting.insert(id, sender);
        }
        // Armed before the write: once the line is queued the server may get
        // the request, so abandoning it from here on must tell the server.
        let mut guard = CancelOnDrop {
            id: Some(id),
            writer: self.writer.clone(),
            pending: Arc::clone(&self.pending),
        };
        if let Err(error) = self.write(&message).await {
            guard.id = None;
            lock(&self.pending).waiting.remove(&id);
            return Err(error);
        }
        let reply = receiver.await;
        guard.id = None;
        match reply {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(RequestError::Rpc {
                status: None,
                error,
            }),
            Err(_) => Err(RequestError::Lost(
                lock(&self.pending)
                    .closed
                    .clone()
                    .unwrap_or_else(|| "the server stopped answering".into()),
            )),
        }
    }
}

fn line_of(message: &Value) -> String {
    let mut line = message.to_string();
    line.push('\n');
    line
}

/// The only writer to the server's input. A line is written whole or the
/// input is given up: after a failed write nothing more is written, because
/// the server may hold a partial line.
async fn write_lines(mut stdin: ChildStdin, mut lines: mpsc::UnboundedReceiver<Outgoing>) {
    let mut broken: Option<String> = None;
    while let Some(Outgoing::Line(line, written)) = lines.recv().await {
        let result = match &broken {
            Some(reason) => Err(WriteError::NotStarted(reason.clone())),
            None => {
                let outcome = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.flush().await
                };
                outcome.await.map_err(|error| {
                    let reason = format!("cannot write to the server: {error}");
                    broken = Some(reason.clone());
                    WriteError::Partial(reason)
                })
            }
        };
        if let Some(written) = written {
            let _ = written.send(result);
        }
    }
    // `Close`, or every sender is gone: dropping the input tells the server to exit.
}

/// Sends `notifications/cancelled` when a request's future is dropped before
/// its reply arrives, as the stdio transport requires. The notification is
/// queued behind the request's own line. The effect of the cancelled request
/// stays unknown.
struct CancelOnDrop {
    id: Option<u64>,
    writer: mpsc::UnboundedSender<Outgoing>,
    pending: Pending,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        lock(&self.pending).waiting.remove(&id);
        let message = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "the client stopped waiting"},
        });
        let _ = self.writer.send(Outgoing::Line(line_of(&message), None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_request_the_server_did_not_act_on_counts_as_not_processed() {
        let rpc = |code| RequestError::Rpc {
            status: None,
            error: RpcError {
                code,
                message: String::new(),
                data: None,
            },
        };
        assert!(RequestError::NotSent(String::new()).not_processed());
        for code in [-32700, -32601, -32020, -32021, -32022] {
            assert!(rpc(code).not_processed(), "{code}");
        }
        // A handler can raise these after it began its work.
        for code in [-32600, -32602, -32603, -32000, 1] {
            assert!(!rpc(code).not_processed(), "{code}");
        }
        for status in [400, 401, 403, 404, 405, 406, 413, 415, 431] {
            assert!(RequestError::Http { status }.not_processed(), "{status}");
        }
        for status in [408, 409, 422, 424, 425, 429, 500, 502, 503] {
            assert!(!RequestError::Http { status }.not_processed(), "{status}");
        }
        assert!(!RequestError::Lost(String::new()).not_processed());
        assert!(!RequestError::Invalid(String::new()).not_processed());
    }
}
