//! The two standard MCP transports: Streamable HTTP and stdio.
//!
//! A transport sends one JSON-RPC request and returns its result, or says how
//! far the request got. That distinction matters for writes: a request that was
//! never sent did nothing, a request the server rejected did nothing, and a
//! request that was sent and not answered may have done anything.
//!
//! An error's text holds the server's own words or this client's description
//! of a failure. It never holds the configured URL, a header, or an
//! environment value.

use std::collections::HashMap;
use std::fmt;
use std::process::Stdio as ProcessStdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{
    ACCEPT, CONTENT_TYPE, DATE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
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
/// Others, such as 409 or 422, can carry an application's own meaning. Status
/// 429 is [`RequestError::RateLimited`].
const NOT_PROCESSED_STATUSES: [u16; 9] = [400, 401, 403, 404, 405, 406, 413, 415, 431];
/// The HTTP status of a rate limit.
const TOO_MANY_REQUESTS: u16 = 429;
/// Longest wait for a stdio server to exit after its input closes.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for the rest of a stdio server's standard error after it exited.
const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(1);
/// Most bytes shown of one line a stdio server writes to its standard error.
const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;
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
    /// The server answered with HTTP status 429: its rate limit refused the
    /// request before acting on it.
    RateLimited {
        /// The wait its `Retry-After` header asks for, given as seconds or as
        /// an HTTP date; `None` when the header is absent or unreadable.
        retry_after: Option<Duration>,
    },
    /// The request was sent and no response arrived.
    Lost(String),
    /// A response arrived that is not valid MCP.
    Invalid(String),
}

impl RequestError {
    /// Whether the server certainly did not act on the request: it was not
    /// sent, or it was rejected before a tool could run (HTTP status 400, 401,
    /// 403, 404, 405, 406, 413, 415, 429, or 431, or JSON-RPC code -32700,
    /// -32601, -32020, -32021, or -32022). Every other failure leaves the
    /// effect unknown.
    #[must_use]
    pub fn not_processed(&self) -> bool {
        match self {
            Self::NotSent(_) | Self::RateLimited { .. } => true,
            Self::Rpc { error, .. } => error.not_processed(),
            Self::Http { status } => NOT_PROCESSED_STATUSES.contains(status),
            Self::Lost(_) | Self::Invalid(_) => false,
        }
    }

    /// The same error with every one of `values` replaced by `[redacted]` in
    /// its text, and without a JSON-RPC error's `data`. The values are the
    /// configured header, `env`, and URL query values, which a server can
    /// repeat in what it answers.
    #[must_use]
    pub(crate) fn redacted(&self, values: &[String]) -> Self {
        let clean = |text: &str| super::config::redact(text, values);
        match self {
            Self::NotSent(reason) => Self::NotSent(clean(reason)),
            Self::Rpc { status, error } => Self::Rpc {
                status: *status,
                error: RpcError {
                    code: error.code,
                    message: clean(&error.message),
                    data: None,
                },
            },
            Self::Lost(reason) => Self::Lost(clean(reason)),
            Self::Invalid(reason) => Self::Invalid(clean(reason)),
            Self::Http { .. } | Self::RateLimited { .. } => self.clone(),
        }
    }

    /// A short machine code for traces and journals: `not_sent`,
    /// `rpc_<code>`, `http_<status>`, `rate_limited`, `no_response`, or
    /// `invalid_response`.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::NotSent(_) => "not_sent".into(),
            Self::Rpc { error, .. } => format!("rpc_{}", error.code),
            Self::Http { status } => format!("http_{status}"),
            Self::RateLimited { .. } => "rate_limited".into(),
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
            Self::Http { status } => {
                write!(formatter, "the server returned HTTP status {status}")?;
                match reqwest::StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                {
                    Some(reason) => write!(formatter, " {reason}"),
                    None => Ok(()),
                }
            }
            Self::RateLimited { retry_after } => {
                formatter.write_str("the server returned HTTP status 429 Too Many Requests")?;
                match retry_after {
                    Some(wait) => write!(formatter, "; Retry-After: {} s", wait.as_secs()),
                    None => Ok(()),
                }
            }
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
                    RequestError::NotSent(describe(error))
                } else {
                    RequestError::Lost(describe(error))
                }
            })?;
        let status = response.status().as_u16();
        if status == TOO_MANY_REQUESTS {
            return Err(RequestError::RateLimited {
                retry_after: retry_after(response.headers(), SystemTime::now()),
            });
        }
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

/// An error's text without its URL, which may hold a session identifier or
/// a key in its query.
fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    wire::truncate(&text, wire::MAX_MESSAGE_BYTES)
}

/// The wait a `Retry-After` header asks for: a number of seconds, or an HTTP
/// date. A date is read against the response's own `Date` header, so that a
/// difference between the two clocks does not matter, or against `now` when
/// the response has none.
fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let text = |name: HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
    let value = text(RETRY_AFTER)?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let until = http_date(value)?;
    let from = text(DATE).and_then(http_date).unwrap_or_else(|| {
        now.duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    });
    Some(Duration::from_secs(until.saturating_sub(from)))
}

/// Seconds since the Unix epoch of an HTTP date in its one current form,
/// such as `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(text: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = text.split_ascii_whitespace();
    let (_weekday, day, month, year, time, zone) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if zone != "GMT" || parts.next().is_some() {
        return None;
    }
    let day: u64 = day.parse().ok().filter(|day| (1..=31).contains(day))?;
    let month = MONTHS.iter().position(|name| *name == month)? as u64 + 1;
    let year: u64 = year
        .parse()
        .ok()
        .filter(|year| (1970..=9999).contains(year))?;
    let mut clock = time.split(':').map(|part| part.parse::<u64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm),
    // with the year counted from March so that a leap day ends it.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
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
            Err(error) => return Err(RequestError::Lost(describe(error))),
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
            Err(error) => return Err(RequestError::Lost(describe(error))),
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
    /// The task that relays the server's standard error.
    diagnostics: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Stdio {
    /// Start the server. What it writes to its standard error is read as it
    /// comes, so the server never waits for a reader, and each line is
    /// written to this process's standard error with `redactions` replaced
    /// and unsafe characters escaped.
    pub(crate) fn start(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        redactions: Vec<String>,
    ) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(ProcessStdio::piped())
            .stdout(ProcessStdio::piped())
            .stderr(ProcessStdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start {command:?}: {error}"))?;
        let stdin = child.stdin.take().ok_or("the server has no stdin")?;
        let stdout = child.stdout.take().ok_or("the server has no stdout")?;
        let stderr = child.stderr.take().ok_or("the server has no stderr")?;
        let diagnostics = tokio::spawn(relay_diagnostics(stderr, redactions));
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
            diagnostics: tokio::sync::Mutex::new(Some(diagnostics)),
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
        // The server's last words, when it ended by itself. A process it
        // left behind may hold the pipe open, so the wait is short.
        if let Some(diagnostics) = self.diagnostics.lock().await.take() {
            let _ = tokio::time::timeout(DIAGNOSTICS_TIMEOUT, diagnostics).await;
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

/// Read a stdio server's standard error to its end and write each line to
/// this process's standard error as `server: <line>`. A line is cut at 4 KiB,
/// `redactions` are replaced, and unsafe characters are escaped, so that a
/// server cannot draw on the terminal or show a configured value.
async fn relay_diagnostics(stderr: tokio::process::ChildStderr, redactions: Vec<String>) {
    let mut reader = BufReader::new(stderr);
    let mut line: Vec<u8> = Vec::new();
    // Whether the rest of the current line is being dropped.
    let mut overflow = false;
    loop {
        let (consumed, ended) = match reader.fill_buf().await {
            Ok([]) | Err(_) => break,
            Ok(available) => {
                let end = available.iter().position(|byte| *byte == b'\n');
                let part = &available[..end.unwrap_or(available.len())];
                let room = MAX_DIAGNOSTIC_BYTES.saturating_sub(line.len());
                if part.len() > room {
                    overflow = true;
                }
                line.extend_from_slice(&part[..part.len().min(room)]);
                (end.map_or(available.len(), |end| end + 1), end.is_some())
            }
        };
        reader.consume(consumed);
        if ended {
            print_diagnostic(&line, overflow, &redactions);
            line.clear();
            overflow = false;
        }
    }
    if !line.is_empty() {
        print_diagnostic(&line, overflow, &redactions);
    }
}

/// One line of a server's standard error, as this process shows it.
fn diagnostic_line(line: &[u8], cut: bool, redactions: &[String]) -> String {
    let text = String::from_utf8_lossy(line);
    let text = super::config::redact(text.trim_end_matches('\r'), redactions);
    format!(
        "server: {}{}",
        crate::render::escape(&text),
        if cut { " ..." } else { "" }
    )
}

fn print_diagnostic(line: &[u8], cut: bool, redactions: &[String]) {
    eprintln!("{}", diagnostic_line(line, cut, redactions));
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
    fn a_configured_value_is_removed_from_every_error_text() {
        let values = vec![
            "Bearer tok-0123456789".to_string(),
            "tok-0123456789".to_string(),
        ];
        let rpc = RequestError::Rpc {
            status: Some(401),
            error: RpcError {
                code: -32001,
                message: "invalid credentials: authorization=Bearer tok-0123456789".into(),
                data: Some(json!({"echo": "tok-0123456789"})),
            },
        };
        let clean = rpc.redacted(&values);
        assert_eq!(
            clean.to_string(),
            "the server returned error -32001: invalid credentials: authorization=[redacted]"
        );
        assert!(
            !format!("{clean:?}").contains("tok-0123456789"),
            "{clean:?}"
        );
        for error in [
            RequestError::NotSent("refused tok-0123456789".into()),
            RequestError::Lost("reset after tok-0123456789".into()),
            RequestError::Invalid("got tok-0123456789".into()),
        ] {
            let text = error.redacted(&values).to_string();
            assert!(text.ends_with("[redacted]"), "{text}");
        }
        let plain = RequestError::Http { status: 500 };
        assert_eq!(plain.redacted(&values), plain);
    }

    #[test]
    fn a_servers_diagnostic_line_is_escaped_redacted_and_cut() {
        let values = vec!["env-secret-value".to_string()];
        assert_eq!(diagnostic_line(b"ready\r", false, &values), "server: ready");
        assert_eq!(
            diagnostic_line(
                b"\x1b[2Jtoken=env-secret-value \xe2\x80\xae",
                false,
                &values
            ),
            "server: \\u{1b}[2Jtoken=[redacted] \\u{202e}"
        );
        assert_eq!(diagnostic_line(b"long", true, &values), "server: long ...");
        // Bytes that are not UTF-8 do not stop the line.
        assert!(diagnostic_line(b"bad \xff byte", false, &values).starts_with("server: bad "));
    }

    #[tokio::test]
    async fn a_servers_standard_error_is_read_to_its_end_in_capped_lines() {
        // More than a pipe holds, in one line without an end: the server
        // would block if nobody read it.
        let script = "head -c 300000 /dev/zero | tr '\\0' x >&2; echo done >&2; cat";
        let server = Stdio::start("sh", &["-c".into(), script.into()], &[], Vec::new())
            .expect("the server starts");
        let diagnostics = server
            .diagnostics
            .lock()
            .await
            .take()
            .expect("the relay task");
        // The script reaches `cat` only after all of its standard error was
        // taken, and ends when its input closes. Had nobody read, the server
        // would still be writing, and closing would wait until it is killed.
        let started = std::time::Instant::now();
        server.close().await;
        assert!(
            started.elapsed() < SHUTDOWN_TIMEOUT - Duration::from_secs(1),
            "the server was blocked on its standard error"
        );
        tokio::time::timeout(Duration::from_secs(10), diagnostics)
            .await
            .expect("the relay ends with the server")
            .expect("the relay does not panic");
    }

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
        for status in [408, 409, 422, 424, 425, 500, 502, 503] {
            assert!(!RequestError::Http { status }.not_processed(), "{status}");
        }
        // A rate limit refuses the request before the tool runs.
        assert!(RequestError::RateLimited { retry_after: None }.not_processed());
        assert!(!RequestError::Lost(String::new()).not_processed());
        assert!(!RequestError::Invalid(String::new()).not_processed());
    }

    #[test]
    fn a_code_says_whether_the_server_answered() {
        use crate::recovery::answered_with_error;
        let rpc = RequestError::Rpc {
            status: Some(200),
            error: RpcError {
                code: -32603,
                message: String::new(),
                data: None,
            },
        };
        let cases = [
            (RequestError::NotSent(String::new()), "not_sent", false),
            (RequestError::Lost(String::new()), "no_response", false),
            (rpc, "rpc_-32603", true),
            (RequestError::Http { status: 502 }, "http_502", true),
            (
                RequestError::RateLimited { retry_after: None },
                "rate_limited",
                true,
            ),
            (
                RequestError::Invalid(String::new()),
                "invalid_response",
                true,
            ),
        ];
        for (error, code, answered) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(answered_with_error(code), answered, "{code}");
        }
        // Codes the provider gives when it stops waiting or sends nothing.
        for code in [
            "deadline",
            "cancelled",
            "not_a_write_tool",
            "preconditions_unsupported",
        ] {
            assert!(!answered_with_error(code), "{code}");
        }
        for code in ["tool_error", "input_required"] {
            assert!(answered_with_error(code), "{code}");
        }
    }

    #[test]
    fn retry_after_is_read_as_seconds_or_as_an_http_date() {
        let wait = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, HeaderValue::from_static(value));
            }
            // 1994-11-06T08:49:37Z on this machine's clock.
            let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
            retry_after(&headers, now).map(|wait| wait.as_secs())
        };
        assert_eq!(wait(&[]), None);
        assert_eq!(wait(&[("retry-after", "0")]), Some(0));
        assert_eq!(wait(&[("retry-after", " 120 ")]), Some(120));
        assert_eq!(wait(&[("retry-after", "soon")]), None);
        assert_eq!(wait(&[("retry-after", "-1")]), None);
        // Against this machine's clock when the response has no Date.
        assert_eq!(
            wait(&[("retry-after", "Sun, 06 Nov 1994 08:49:47 GMT")]),
            Some(10)
        );
        // Against the server's clock when it has one.
        assert_eq!(
            wait(&[
                ("retry-after", "Sun, 06 Nov 1994 08:50:37 GMT"),
                ("date", "Sun, 06 Nov 1994 08:50:07 GMT"),
            ]),
            Some(30)
        );
        // A date that has passed asks for no wait.
        assert_eq!(
            wait(&[("retry-after", "Sun, 06 Nov 1994 08:00:00 GMT")]),
            Some(0)
        );
    }

    #[test]
    fn http_dates_are_read_exactly() {
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        // A leap day, and the day after it.
        assert_eq!(
            http_date("Thu, 29 Feb 2024 12:00:00 GMT"),
            Some(1_709_208_000)
        );
        assert_eq!(
            http_date("Fri, 01 Mar 2024 00:00:00 GMT"),
            Some(1_709_251_200)
        );
        for invalid in [
            "",
            "Sun, 06 Nov 1994 08:49:37",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
        ] {
            assert_eq!(http_date(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn a_rate_limit_or_a_status_is_described_without_the_request() {
        assert_eq!(
            RequestError::Http { status: 503 }.to_string(),
            "the server returned HTTP status 503 Service Unavailable"
        );
        assert_eq!(
            RequestError::Http { status: 599 }.to_string(),
            "the server returned HTTP status 599"
        );
        let limited = RequestError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(limited.code(), "rate_limited");
        assert_eq!(
            limited.to_string(),
            "the server returned HTTP status 429 Too Many Requests; Retry-After: 7 s"
        );
    }
}
