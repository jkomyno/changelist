//! Offline Model Context Protocol (MCP) server for tests. Requires the `testkit` feature.
//!
//! It speaks MCP revision `2026-07-28` ("modern": stateless, every request carries its
//! protocol version in `params._meta`) and, on request, revision `2025-11-25` ("legacy":
//! `initialize` handshake, HTTP sessions). It serves a fixed set of note-taking tools,
//! records every applied effect in an optional ledger, and injects faults on demand.
//!
//! ```text
//! codemode-mcp-fixture (--stdio | --http <addr-file>) [--era modern|legacy|dual] [--state <file>]
//!                      [--ledger <file>] [--sse] [--require-header <Name>=<value>]
//!                      [--page-size <n>] [--fault <tool>=<kind>[,<kind>...]]...
//! ```
//!
//! # Flags
//!
//! - `--stdio`: newline-delimited JSON-RPC on stdin/stdout; exits at stdin EOF after
//!   finishing delayed calls. Only MCP messages go to stdout; diagnostics go to stderr.
//! - `--http <addr-file>`: binds `127.0.0.1:0`, atomically writes
//!   `http://127.0.0.1:<port>/mcp` (no trailing newline) to `<addr-file>`, serves until
//!   killed. One request per connection, `Connection: close`, `Content-Length` bodies only.
//! - `--era modern|legacy|dual` (default `modern`). In `dual`, a request whose
//!   `params._meta` has `io.modelcontextprotocol/protocolVersion` is served by modern
//!   rules; everything else (including `initialize`) by legacy rules.
//! - `--state <file>`: notes store `{"notes":{"<id>":"<text>"}}`, loaded if present and
//!   rewritten atomically after every effect. Without it, notes live in memory.
//! - `--ledger <file>`: appends `{"seq":n,"tool":"<name>","arguments":{...}}` per applied
//!   effect, synced before the reply is sent; `seq` continues from existing lines.
//!   Effect tools are `notes.put`, `notes.append`, `notes.delete`, `plain.unannotated`;
//!   a call is ledgered whenever it executes successfully (deleting a missing note too).
//! - `--sse` (HTTP only): `200` replies to requests use `text/event-stream`: a
//!   `: keep-alive` comment, a `notifications/progress` event if the request set
//!   `_meta.progressToken`, then `event: message` with the response.
//! - `--require-header Name=value` (HTTP only): any request lacking that exact header
//!   value gets `401` with an empty body, before any other check.
//! - `--page-size n`: `tools/list` page size (default: one page). Cursors are base64 of
//!   the decimal offset; any other cursor is `-32602`.
//! - `--fault <tool>=<kinds>`: applies to the next `tools/call` of `<tool>` that passes
//!   protocol validation, then clears unless `sticky`. Repeating it for a tool replaces
//!   the earlier spec. At most one action kind per spec, plus `delay-ms` and `sticky`.
//!
//! # Tools (in `tools/list` order)
//!
//! | Tool | Arguments | Annotations | Effect / result |
//! |---|---|---|---|
//! | `notes.list` | `{}` | readOnly | `{"notes":[{"id","text"}]}` by id; has `outputSchema` |
//! | `notes.get` | `id` | readOnly | `{"id","text"}`, or `isError` "note not found" |
//! | `notes.put` | `id`, `text` | readOnly false, destructive false, idempotent | sets note |
//! | `notes.append` | `id`, `text` | readOnly false, destructive false | appends `text\n` |
//! | `notes.delete` | `id` | readOnly false, destructive, idempotent | `{"id","deleted":true}` |
//! | `raw.text` | `{}` | readOnly | text `plain words, not JSON`, no `structuredContent` |
//! | `raw.json_text` | `{}` | readOnly | text `{"answer":42}`, no `structuredContent` |
//! | `region.echo` | `region` (`x-mcp-header: Region`), `query` | readOnly | `{"region","query"}` |
//! | `plain.unannotated` | `value` | none | appends `value\n` to note `unannotated`; `{"ok":true}` |
//!
//! All properties are required strings and schemas set `additionalProperties: false`.
//! Results with `structuredContent` also carry it serialized in one text block. Invalid
//! arguments give an `isError` result and no effect. An unknown tool is `-32602`.
//!
//! # Faults
//!
//! - `drop-reply`: apply the effect, send nothing (HTTP: close without a response).
//! - `no-effect-drop`: no effect, send nothing.
//! - `delay-ms:<n>`: sleep n ms, then proceed. A cancelled call (stdio
//!   `notifications/cancelled`, HTTP client disconnect) still applies its effect but
//!   sends nothing.
//! - `is-error`: no effect; result `isError: true`, text `upstream said no`.
//! - `rpc-error`: no effect; JSON-RPC `-32603` `internal error`.
//! - `http-500`: no effect; HTTP `500` with a plain-text body (stdio: same as `rpc-error`).
//! - `input-required`: no effect; an `input_required` result asking for `confirm`.
//! - `sticky`: keep the fault for every later call.
//!
//! # Status codes (HTTP)
//!
//! - Any era: `401` missing required header, `404` path other than `/mcp`, `403`
//!   non-loopback `Origin`, `411` no `Content-Length`, `413` body over 1 MiB, `400`
//!   `-32700` malformed JSON or `-32600` invalid JSON-RPC, `202` empty for notifications.
//! - Modern: GET/DELETE `405`; `400` `-32602` missing `_meta` fields; `400` `-32022`
//!   unsupported version; `406` `Accept` lacking `application/json` or
//!   `text/event-stream`; `400` `-32020` missing or mismatching `MCP-Protocol-Version`,
//!   `Mcp-Method`, `Mcp-Name`, or `Mcp-Param-Region`; `404` `-32601` unknown method
//!   (including `initialize`, whose message names the supported versions). Other
//!   JSON-RPC errors (unknown tool, bad cursor, `-32603`) use `200`.
//! - Legacy: `initialize` sets `Mcp-Session-Id`. Without a session header: `400`
//!   `-32600` "server not initialized" while no session was ever minted, else `400`
//!   with an empty body. Unknown session id: `404` empty. `MCP-Protocol-Version`, when
//!   present, must be `2025-11-25` (`400` `-32600`). DELETE with a known session ends
//!   it (`200`). JSON-RPC errors after initialization use `200`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";
const SERVER_NAME: &str = "codemode-mcp-fixture";
const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 64 * 1024;

const USAGE: &str = "usage: codemode-mcp-fixture (--stdio | --http <addr-file>) \
[--era modern|legacy|dual] [--state <file>] [--ledger <file>] [--sse] \
[--require-header <Name>=<value>] [--page-size <n>] [--fault <tool>=<kind>[,<kind>...]]...";

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    Modern,
    Legacy,
    Dual,
}

#[derive(Debug)]
enum Mode {
    Stdio,
    Http(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultAction {
    DropReply,
    NoEffectDrop,
    IsError,
    RpcError,
    Http500,
    InputRequired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fault {
    delay_ms: Option<u64>,
    action: Option<FaultAction>,
    sticky: bool,
}

#[derive(Debug)]
struct Config {
    mode: Mode,
    era: Era,
    state: Option<PathBuf>,
    ledger: Option<PathBuf>,
    sse: bool,
    /// Lowercase header name and exact value.
    require_header: Option<(String, String)>,
    page_size: Option<usize>,
    faults: HashMap<String, Fault>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut mode = None;
    let mut config = Config {
        mode: Mode::Stdio,
        era: Era::Modern,
        state: None,
        ledger: None,
        sse: false,
        require_header: None,
        page_size: None,
        faults: HashMap::new(),
    };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("`{flag}` needs a value"));
        match flag.as_str() {
            "--stdio" => set_mode(&mut mode, Mode::Stdio)?,
            "--http" => set_mode(&mut mode, Mode::Http(value()?.into()))?,
            "--era" => {
                config.era = match value()?.as_str() {
                    "modern" => Era::Modern,
                    "legacy" => Era::Legacy,
                    "dual" => Era::Dual,
                    other => return Err(format!("unknown era `{other}`")),
                }
            }
            "--state" => config.state = Some(value()?.into()),
            "--ledger" => config.ledger = Some(value()?.into()),
            "--sse" => config.sse = true,
            "--require-header" => {
                let spec = value()?;
                let (name, expected) = spec
                    .split_once('=')
                    .filter(|(name, _)| !name.is_empty())
                    .ok_or_else(|| {
                        format!("`--require-header` expects Name=value, got `{spec}`")
                    })?;
                config.require_header = Some((name.to_ascii_lowercase(), expected.to_owned()));
            }
            "--page-size" => {
                let raw = value()?;
                let size = raw.parse::<usize>().ok().filter(|size| *size > 0);
                config.page_size = Some(size.ok_or_else(|| format!("invalid page size `{raw}`"))?);
            }
            "--fault" => {
                let (tool, fault) = parse_fault(&value()?)?;
                config.faults.insert(tool, fault);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    config.mode = mode.ok_or("one of `--stdio` or `--http <addr-file>` is required")?;
    if matches!(config.mode, Mode::Stdio) && (config.sse || config.require_header.is_some()) {
        return Err("`--sse` and `--require-header` apply only to `--http`".into());
    }
    Ok(config)
}

fn set_mode(slot: &mut Option<Mode>, mode: Mode) -> Result<(), String> {
    if slot.replace(mode).is_some() {
        return Err("give exactly one of `--stdio` or `--http`".into());
    }
    Ok(())
}

fn parse_fault(spec: &str) -> Result<(String, Fault), String> {
    let (tool, kinds) = spec
        .split_once('=')
        .ok_or_else(|| format!("`--fault` expects <tool>=<kinds>, got `{spec}`"))?;
    if find_tool(tool).is_none() {
        return Err(format!("`--fault` names unknown tool `{tool}`"));
    }
    let mut fault = Fault::default();
    for kind in kinds.split(',') {
        let action = match kind {
            "drop-reply" => FaultAction::DropReply,
            "no-effect-drop" => FaultAction::NoEffectDrop,
            "is-error" => FaultAction::IsError,
            "rpc-error" => FaultAction::RpcError,
            "http-500" => FaultAction::Http500,
            "input-required" => FaultAction::InputRequired,
            "sticky" => {
                fault.sticky = true;
                continue;
            }
            other => {
                let ms = other
                    .strip_prefix("delay-ms:")
                    .and_then(|ms| ms.parse().ok())
                    .ok_or_else(|| format!("unknown fault kind `{other}`"))?;
                fault.delay_ms = Some(ms);
                continue;
            }
        };
        if fault.action.replace(action).is_some() {
            return Err(format!("`--fault {spec}` names more than one action"));
        }
    }
    Ok((tool.to_owned(), fault))
}

// ---------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------

struct Prop {
    name: &'static str,
    header: Option<&'static str>,
}

const fn arg(name: &'static str) -> Prop {
    Prop { name, header: None }
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    props: &'static [Prop],
    annotations: Option<&'static [(&'static str, bool)]>,
    /// Whether a successful call changes state and is ledgered.
    effect: bool,
}

const READ_ONLY: &[(&str, bool)] = &[("readOnlyHint", true)];

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "notes.list",
        description: "List all notes sorted by id.",
        props: &[],
        annotations: Some(READ_ONLY),
        effect: false,
    },
    ToolDef {
        name: "notes.get",
        description: "Read one note.",
        props: &[arg("id")],
        annotations: Some(READ_ONLY),
        effect: false,
    },
    ToolDef {
        name: "notes.put",
        description: "Set a note's text.",
        props: &[arg("id"), arg("text")],
        annotations: Some(&[
            ("readOnlyHint", false),
            ("destructiveHint", false),
            ("idempotentHint", true),
        ]),
        effect: true,
    },
    ToolDef {
        name: "notes.append",
        description: "Append a line to a note, creating it if needed.",
        props: &[arg("id"), arg("text")],
        annotations: Some(&[("readOnlyHint", false), ("destructiveHint", false)]),
        effect: true,
    },
    ToolDef {
        name: "notes.delete",
        description: "Delete a note; deleting a missing note succeeds.",
        props: &[arg("id")],
        annotations: Some(&[
            ("readOnlyHint", false),
            ("destructiveHint", true),
            ("idempotentHint", true),
        ]),
        effect: true,
    },
    ToolDef {
        name: "raw.text",
        description: "Return plain text without structured content.",
        props: &[],
        annotations: Some(READ_ONLY),
        effect: false,
    },
    ToolDef {
        name: "raw.json_text",
        description: "Return JSON as text without structured content.",
        props: &[],
        annotations: Some(READ_ONLY),
        effect: false,
    },
    ToolDef {
        name: "region.echo",
        description: "Echo a region and query; the region is mirrored in a header.",
        props: &[
            Prop {
                name: "region",
                header: Some("Region"),
            },
            arg("query"),
        ],
        annotations: Some(READ_ONLY),
        effect: false,
    },
    ToolDef {
        name: "plain.unannotated",
        description: "Append a line to the `unannotated` note.",
        props: &[arg("value")],
        annotations: None,
        effect: true,
    },
];

fn find_tool(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|tool| tool.name == name)
}

fn tool_json(def: &ToolDef) -> Value {
    let mut properties = Map::new();
    for prop in def.props {
        let mut schema = json!({ "type": "string" });
        if let Some(header) = prop.header {
            schema["x-mcp-header"] = json!(header);
        }
        properties.insert(prop.name.to_owned(), schema);
    }
    let required: Vec<&str> = def.props.iter().map(|prop| prop.name).collect();
    let mut tool = json!({
        "name": def.name,
        "description": def.description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    });
    if let Some(annotations) = def.annotations {
        let map: Map<String, Value> = annotations
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::Bool(*value)))
            .collect();
        tool["annotations"] = Value::Object(map);
    }
    if def.name == "notes.list" {
        tool["outputSchema"] = json!({
            "type": "object",
            "properties": {
                "notes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "id": { "type": "string" }, "text": { "type": "string" } },
                        "required": ["id", "text"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["notes"],
            "additionalProperties": false,
        });
    }
    tool
}

/// Checks `args` against the tool's schema: an object of exactly the declared strings.
fn validate_args<'a>(def: &ToolDef, args: &'a Value) -> Result<&'a Map<String, Value>, String> {
    let map = args.as_object().ok_or("arguments must be an object")?;
    if let Some(extra) = map
        .keys()
        .find(|key| !def.props.iter().any(|p| p.name == *key))
    {
        return Err(format!("unexpected property `{extra}`"));
    }
    for prop in def.props {
        match map.get(prop.name) {
            None => return Err(format!("missing required property `{}`", prop.name)),
            Some(Value::String(_)) => {}
            Some(_) => return Err(format!("property `{}` must be a string", prop.name)),
        }
    }
    Ok(map)
}

#[derive(Debug, PartialEq)]
struct ToolOutput {
    structured: Option<Value>,
    text: String,
    is_error: bool,
}

impl ToolOutput {
    fn structured(value: Value) -> Self {
        Self {
            text: value.to_string(),
            structured: Some(value),
            is_error: false,
        }
    }

    fn text(text: &str) -> Self {
        Self {
            structured: None,
            text: text.to_owned(),
            is_error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            structured: None,
            text: text.into(),
            is_error: true,
        }
    }
}

/// Runs a validated tool call against the notes.
fn run_tool(
    notes: &mut BTreeMap<String, String>,
    name: &str,
    args: &Map<String, Value>,
) -> ToolOutput {
    let get = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    match name {
        "notes.list" => {
            let list: Vec<Value> = notes
                .iter()
                .map(|(id, text)| json!({ "id": id, "text": text }))
                .collect();
            ToolOutput::structured(json!({ "notes": list }))
        }
        "notes.get" => match notes.get(get("id")) {
            Some(text) => ToolOutput::structured(json!({ "id": get("id"), "text": text })),
            None => ToolOutput::error("note not found"),
        },
        "notes.put" => {
            notes.insert(get("id").to_owned(), get("text").to_owned());
            ToolOutput::structured(json!({ "id": get("id"), "text": get("text") }))
        }
        "notes.append" => {
            let text = append_line(notes, get("id"), get("text"));
            ToolOutput::structured(json!({ "id": get("id"), "text": text }))
        }
        "notes.delete" => {
            notes.remove(get("id"));
            ToolOutput::structured(json!({ "id": get("id"), "deleted": true }))
        }
        "raw.text" => ToolOutput::text("plain words, not JSON"),
        "raw.json_text" => ToolOutput::text(r#"{"answer":42}"#),
        "region.echo" => {
            ToolOutput::structured(json!({ "region": get("region"), "query": get("query") }))
        }
        "plain.unannotated" => {
            append_line(notes, "unannotated", get("value"));
            ToolOutput::structured(json!({ "ok": true }))
        }
        other => ToolOutput::error(format!("tool `{other}` has no implementation")),
    }
}

fn append_line(notes: &mut BTreeMap<String, String>, id: &str, line: &str) -> String {
    let note = notes.entry(id.to_owned()).or_default();
    note.push_str(line);
    note.push('\n');
    note.clone()
}

fn input_required() -> Value {
    json!({
        "resultType": "input_required",
        "inputRequests": {
            "confirm": {
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "message": "Confirm?",
                    "requestedSchema": { "type": "object", "properties": {} },
                },
            },
        },
        "requestState": "abc",
    })
}

// ---------------------------------------------------------------------------------------
// Pagination and header values
// ---------------------------------------------------------------------------------------

fn encode_cursor(offset: usize) -> String {
    BASE64.encode(offset.to_string())
}

/// Accepts only cursors this server could have issued.
fn decode_cursor(cursor: &str) -> Option<usize> {
    let bytes = BASE64.decode(cursor).ok()?;
    let offset: usize = std::str::from_utf8(&bytes).ok()?.parse().ok()?;
    let issued = offset > 0 && offset < TOOLS.len() && encode_cursor(offset) == cursor;
    issued.then_some(offset)
}

fn page(offset: usize, size: Option<usize>) -> (Vec<Value>, Option<String>) {
    let end = size.map_or(TOOLS.len(), |size| {
        offset.saturating_add(size).min(TOOLS.len())
    });
    let tools = TOOLS[offset..end].iter().map(tool_json).collect();
    (tools, (end < TOOLS.len()).then(|| encode_cursor(end)))
}

/// Decodes an `Mcp-Name` / `Mcp-Param-*` value, including the `=?base64?...?=` form.
/// Returns `None` for malformed base64, non-UTF-8 content, or invalid header characters.
fn decode_header_value(raw: &str) -> Option<String> {
    if let Some(inner) = raw
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    {
        return BASE64
            .decode(inner)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }
    let valid = raw
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte));
    valid.then(|| raw.to_owned())
}

fn accepts_json_and_sse(accept: Option<&str>) -> bool {
    let types: Vec<String> = accept
        .unwrap_or_default()
        .split(',')
        .map(|part| {
            part.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .collect();
    ["application/json", "text/event-stream"]
        .iter()
        .all(|wanted| types.iter().any(|t| t == wanted))
}

// ---------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------

/// A transport-neutral reply. `status` is used only by HTTP.
#[derive(Debug)]
enum Reply {
    Json {
        status: u16,
        body: Value,
        session: Option<String>,
    },
    /// A notification was accepted (HTTP `202`, nothing on stdio).
    Accepted,
    /// HTTP status with an empty body (nothing on stdio).
    Empty(u16),
    /// HTTP status with a plain-text body (nothing on stdio).
    Text(u16, String),
    /// Send nothing at all.
    Drop,
}

fn ok(id: &Value, result: Value) -> Reply {
    Reply::Json {
        status: 200,
        body: json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        session: None,
    }
}

fn rpc_error(
    id: &Value,
    status: u16,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Reply {
    let mut error = json!({ "code": code, "message": message.into() });
    if let Some(data) = data {
        error["data"] = data;
    }
    Reply::Json {
        status,
        body: json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        session: None,
    }
}

fn parse_error(error: &serde_json::Error) -> Reply {
    rpc_error(
        &Value::Null,
        400,
        -32700,
        format!("Parse error: {error}"),
        None,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Modern,
    Legacy,
}

/// Adds the modern `resultType` and `_meta.serverInfo` fields; legacy results pass through.
fn finish_result(style: Style, mut result: Value) -> Value {
    if style == Style::Modern
        && let Some(map) = result.as_object_mut()
    {
        map.entry("resultType").or_insert_with(|| json!("complete"));
        map.insert(
            "_meta".into(),
            json!({ SERVER_INFO_KEY: { "name": SERVER_NAME, "version": "0" } }),
        );
    }
    result
}

fn call_result(style: Style, output: &ToolOutput) -> Value {
    let mut result = json!({ "content": [{ "type": "text", "text": output.text }] });
    if let Some(structured) = &output.structured {
        result["structuredContent"] = structured.clone();
    }
    if output.is_error {
        result["isError"] = json!(true);
    }
    finish_result(style, result)
}

// ---------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------

#[derive(Default, Deserialize, Serialize)]
struct Store {
    notes: BTreeMap<String, String>,
}

struct State {
    notes: BTreeMap<String, String>,
    faults: HashMap<String, Fault>,
    ledger: Option<File>,
    seq: u64,
    stdio_initialized: bool,
    sessions: HashSet<String>,
    /// Serialized ids of stdio requests named by `notifications/cancelled`.
    cancelled: HashSet<String>,
}

struct Server {
    config: Config,
    state: Mutex<State>,
}

/// Received over one transport; HTTP carries the request headers.
enum Transport<'a> {
    Stdio,
    Http(&'a Headers),
}

/// The outcome of handling a message: a reply, or a tool call still to run.
enum Outcome {
    Ready(Reply),
    Call(PlannedCall),
}

impl From<Reply> for Outcome {
    fn from(reply: Reply) -> Self {
        Self::Ready(reply)
    }
}

struct PlannedCall {
    id: Value,
    def: &'static ToolDef,
    args: Value,
    fault: Fault,
    style: Style,
    http: bool,
}

enum SessionCheck {
    Valid,
    NotInitialized,
    Reject(u16),
}

impl Server {
    fn open(mut config: Config) -> Result<Self, String> {
        let notes = match &config.state {
            Some(path) if path.exists() => {
                let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
                serde_json::from_slice::<Store>(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .notes
            }
            _ => BTreeMap::new(),
        };
        let (ledger, seq) = match &config.ledger {
            Some(path) => {
                let seq = match fs::read_to_string(path) {
                    Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count() as u64,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                };
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                (Some(file), seq)
            }
            None => (None, 0),
        };
        let faults = std::mem::take(&mut config.faults);
        Ok(Self::new(config, notes, faults, ledger, seq))
    }

    fn new(
        config: Config,
        notes: BTreeMap<String, String>,
        faults: HashMap<String, Fault>,
        ledger: Option<File>,
        seq: u64,
    ) -> Self {
        let state = State {
            notes,
            faults,
            ledger,
            seq,
            stdio_initialized: false,
            sessions: HashSet::new(),
            cancelled: HashSet::new(),
        };
        Self {
            config,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn supported_versions(&self) -> Vec<&'static str> {
        match self.config.era {
            Era::Dual => vec![MODERN, LEGACY],
            Era::Modern | Era::Legacy => vec![MODERN],
        }
    }

    fn is_modern(&self, params: &Map<String, Value>) -> bool {
        match self.config.era {
            Era::Modern => true,
            Era::Legacy => false,
            Era::Dual => meta(params).is_some_and(|meta| meta.contains_key(VERSION_KEY)),
        }
    }

    /// Handles one parsed JSON-RPC message.
    fn handle(&self, msg: &Value, transport: &Transport<'_>) -> Outcome {
        let Some(obj) = msg.as_object() else {
            return rpc_error(
                &Value::Null,
                400,
                -32600,
                "expected a JSON-RPC object",
                None,
            )
            .into();
        };
        let id = obj.get("id");
        let id_valid = matches!(id, None | Some(Value::String(_) | Value::Number(_)));
        let reply_id = id.filter(|_| id_valid).cloned().unwrap_or(Value::Null);
        let method = obj.get("method").and_then(Value::as_str);
        let version_ok = obj.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
        let (Some(method), true, true) = (method, id_valid, version_ok) else {
            return rpc_error(&reply_id, 400, -32600, "invalid JSON-RPC request", None).into();
        };
        let empty = Map::new();
        let params = match obj.get("params") {
            None => &empty,
            Some(Value::Object(params)) => params,
            Some(_) => {
                return rpc_error(&reply_id, 400, -32600, "params must be an object", None).into();
            }
        };
        let Some(id) = id else {
            return self.notification(method, params, transport).into();
        };
        if self.is_modern(params) {
            self.modern(id, method, params, transport)
        } else {
            self.legacy(id, method, params, transport)
        }
    }

    fn notification(
        &self,
        method: &str,
        params: &Map<String, Value>,
        transport: &Transport<'_>,
    ) -> Reply {
        match transport {
            Transport::Http(headers) if !self.is_modern(params) => {
                match self.check_session(headers) {
                    SessionCheck::Valid => Reply::Accepted,
                    SessionCheck::NotInitialized => Reply::Empty(400),
                    SessionCheck::Reject(status) => Reply::Empty(status),
                }
            }
            Transport::Http(_) => Reply::Accepted,
            Transport::Stdio => {
                if method == "notifications/cancelled"
                    && let Some(request_id) = params.get("requestId")
                {
                    self.lock().cancelled.insert(request_id.to_string());
                }
                Reply::Accepted
            }
        }
    }

    fn modern(
        &self,
        id: &Value,
        method: &str,
        params: &Map<String, Value>,
        transport: &Transport<'_>,
    ) -> Outcome {
        let supported = self.supported_versions();
        if method == "initialize" {
            let message = format!(
                "Method not found: initialize. This server has no initialize handshake; \
                 supported protocol versions: {}",
                supported.join(", ")
            );
            return rpc_error(id, 404, -32601, message, None).into();
        }
        let meta = meta(params);
        let version = meta
            .and_then(|meta| meta.get(VERSION_KEY))
            .and_then(Value::as_str);
        let has_capabilities = meta
            .and_then(|meta| meta.get(CAPABILITIES_KEY))
            .is_some_and(Value::is_object);
        let Some(version) = version.filter(|_| has_capabilities) else {
            let message = format!(
                "Invalid params: _meta must carry `{VERSION_KEY}` (string) and \
                 `{CAPABILITIES_KEY}` (object)"
            );
            return rpc_error(id, 400, -32602, message, None).into();
        };
        if !supported.contains(&version) {
            let data = json!({ "supported": supported, "requested": version });
            return rpc_error(id, 400, -32022, "Unsupported protocol version", Some(data)).into();
        }
        if let Transport::Http(headers) = transport
            && let Err(reply) = check_modern_headers(id, headers, version, method, params)
        {
            return reply.into();
        }
        match method {
            "server/discover" => ok(
                id,
                finish_result(
                    Style::Modern,
                    json!({
                        "supportedVersions": supported,
                        "capabilities": { "tools": { "listChanged": false } },
                        "ttlMs": 0,
                        "cacheScope": "public",
                    }),
                ),
            )
            .into(),
            "tools/list" => self.tools_list(id, params, Style::Modern).into(),
            "tools/call" => self.plan_call(id, params, Style::Modern, transport),
            _ => rpc_error(id, 404, -32601, format!("Method not found: {method}"), None).into(),
        }
    }

    fn legacy(
        &self,
        id: &Value,
        method: &str,
        params: &Map<String, Value>,
        transport: &Transport<'_>,
    ) -> Outcome {
        if let Transport::Http(headers) = transport
            && let Some(version) = headers.get("mcp-protocol-version")
            && version != LEGACY
        {
            let message = format!("MCP-Protocol-Version `{version}` is not {LEGACY}");
            return rpc_error(id, 400, -32600, message, None).into();
        }
        if method == "initialize" {
            return self.initialize(id, transport).into();
        }
        let check = match transport {
            Transport::Stdio if self.lock().stdio_initialized => SessionCheck::Valid,
            Transport::Stdio => SessionCheck::NotInitialized,
            Transport::Http(headers) => self.check_session(headers),
        };
        match check {
            SessionCheck::Valid => {}
            SessionCheck::NotInitialized => {
                return rpc_error(id, 400, -32600, "server not initialized", None).into();
            }
            SessionCheck::Reject(status) => return Reply::Empty(status).into(),
        }
        match method {
            "ping" => ok(id, json!({})).into(),
            "tools/list" => self.tools_list(id, params, Style::Legacy).into(),
            "tools/call" => self.plan_call(id, params, Style::Legacy, transport),
            _ => rpc_error(id, 200, -32601, format!("Method not found: {method}"), None).into(),
        }
    }

    fn initialize(&self, id: &Value, transport: &Transport<'_>) -> Reply {
        let result = json!({
            "protocolVersion": LEGACY,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": "0" },
        });
        let mut state = self.lock();
        let session = match transport {
            Transport::Stdio => {
                state.stdio_initialized = true;
                None
            }
            Transport::Http(_) => {
                let session = new_session_id();
                state.sessions.insert(session.clone());
                Some(session)
            }
        };
        Reply::Json {
            status: 200,
            body: json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            session,
        }
    }

    fn check_session(&self, headers: &Headers) -> SessionCheck {
        let state = self.lock();
        match headers.get("mcp-session-id") {
            Some(session) if state.sessions.contains(session) => SessionCheck::Valid,
            Some(_) => SessionCheck::Reject(404),
            None if state.sessions.is_empty() => SessionCheck::NotInitialized,
            None => SessionCheck::Reject(400),
        }
    }

    fn tools_list(&self, id: &Value, params: &Map<String, Value>, style: Style) -> Reply {
        let offset = match params.get("cursor") {
            None | Some(Value::Null) => Some(0),
            Some(Value::String(cursor)) => decode_cursor(cursor),
            Some(_) => None,
        };
        let Some(offset) = offset else {
            return rpc_error(id, 200, -32602, "Invalid params: unknown cursor", None);
        };
        let (tools, next) = page(offset, self.config.page_size);
        let mut result = json!({ "tools": tools });
        if let Some(next) = next {
            result["nextCursor"] = json!(next);
        }
        if style == Style::Modern {
            result["ttlMs"] = json!(0);
            result["cacheScope"] = json!("public");
        }
        ok(id, finish_result(style, result))
    }

    fn plan_call(
        &self,
        id: &Value,
        params: &Map<String, Value>,
        style: Style,
        transport: &Transport<'_>,
    ) -> Outcome {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return rpc_error(id, 200, -32602, "tools/call requires a string `name`", None).into();
        };
        let Some(def) = find_tool(name) else {
            return rpc_error(id, 200, -32602, format!("Unknown tool: {name}"), None).into();
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let (Style::Modern, Transport::Http(headers)) = (style, transport)
            && let Err(reply) = check_param_headers(id, headers, def, &args)
        {
            return reply.into();
        }
        Outcome::Call(PlannedCall {
            id: id.clone(),
            def,
            args,
            fault: self.take_fault(name),
            style,
            http: matches!(transport, Transport::Http(_)),
        })
    }

    fn take_fault(&self, tool: &str) -> Fault {
        let mut state = self.lock();
        match state.faults.get(tool) {
            Some(fault) if fault.sticky => *fault,
            Some(_) => state.faults.remove(tool).unwrap_or_default(),
            None => Fault::default(),
        }
    }

    /// Runs a planned call, honouring its fault (except the delay, which callers apply).
    fn finish(&self, call: PlannedCall) -> Reply {
        let PlannedCall {
            id,
            def,
            args,
            fault,
            style,
            http,
        } = call;
        match fault.action {
            Some(FaultAction::NoEffectDrop) => Reply::Drop,
            Some(FaultAction::IsError) => ok(
                &id,
                call_result(style, &ToolOutput::error("upstream said no")),
            ),
            Some(FaultAction::Http500) if http => {
                Reply::Text(500, "injected upstream failure\n".into())
            }
            Some(FaultAction::RpcError | FaultAction::Http500) => {
                rpc_error(&id, 200, -32603, "internal error", None)
            }
            Some(FaultAction::InputRequired) => ok(&id, finish_result(style, input_required())),
            Some(FaultAction::DropReply) => {
                if let Err(error) = self.execute(def, &args) {
                    eprintln!("{SERVER_NAME}: {error}");
                }
                Reply::Drop
            }
            None => match self.execute(def, &args) {
                Ok(output) => ok(&id, call_result(style, &output)),
                Err(error) => rpc_error(&id, 200, -32603, error, None),
            },
        }
    }

    /// Runs a tool and, for a successful effect, persists the notes and appends the ledger.
    fn execute(&self, def: &ToolDef, args: &Value) -> Result<ToolOutput, String> {
        let map = match validate_args(def, args) {
            Ok(map) => map,
            Err(message) => {
                return Ok(ToolOutput::error(format!(
                    "invalid arguments for {}: {message}",
                    def.name
                )));
            }
        };
        let mut state = self.lock();
        let output = run_tool(&mut state.notes, def.name, map);
        if def.effect && !output.is_error {
            self.record(&mut state, def.name, args)
                .map_err(|e| format!("failed to record effect: {e}"))?;
        }
        Ok(output)
    }

    fn record(&self, state: &mut State, tool: &str, args: &Value) -> io::Result<()> {
        if let Some(path) = &self.config.state {
            let store = json!({ "notes": state.notes });
            write_atomically(path, store.to_string().as_bytes())?;
        }
        state.seq += 1;
        if let Some(ledger) = &mut state.ledger {
            let line = json!({ "seq": state.seq, "tool": tool, "arguments": args });
            ledger.write_all(format!("{line}\n").as_bytes())?;
            ledger.flush()?;
            ledger.sync_all()?;
        }
        Ok(())
    }

    /// Resolves an outcome on the current thread, sleeping through any delay fault.
    fn resolve(&self, outcome: Outcome) -> Reply {
        match outcome {
            Outcome::Ready(reply) => reply,
            Outcome::Call(call) => {
                if let Some(ms) = call.fault.delay_ms {
                    thread::sleep(Duration::from_millis(ms));
                }
                self.finish(call)
            }
        }
    }
}

fn meta(params: &Map<String, Value>) -> Option<&Map<String, Value>> {
    params.get("_meta").and_then(Value::as_object)
}

fn header_mismatch(id: &Value, message: String) -> Reply {
    rpc_error(id, 400, -32020, format!("Header mismatch: {message}"), None)
}

fn check_modern_headers(
    id: &Value,
    headers: &Headers,
    version: &str,
    method: &str,
    params: &Map<String, Value>,
) -> Result<(), Reply> {
    if !accepts_json_and_sse(headers.get("accept")) {
        return Err(Reply::Empty(406));
    }
    expect_header(id, headers, "MCP-Protocol-Version", version, false)?;
    expect_header(id, headers, "Mcp-Method", method, false)?;
    if method == "tools/call" {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        expect_header(id, headers, "Mcp-Name", name, true)?;
    }
    Ok(())
}

fn check_param_headers(
    id: &Value,
    headers: &Headers,
    def: &ToolDef,
    args: &Value,
) -> Result<(), Reply> {
    for prop in def.props {
        if let Some(header) = prop.header
            && let Some(value) = args.get(prop.name).and_then(Value::as_str)
        {
            expect_header(id, headers, &format!("Mcp-Param-{header}"), value, true)?;
        }
    }
    Ok(())
}

fn expect_header(
    id: &Value,
    headers: &Headers,
    name: &str,
    expected: &str,
    encoded: bool,
) -> Result<(), Reply> {
    let Some(raw) = headers.get(name) else {
        return Err(header_mismatch(id, format!("missing {name} header")));
    };
    let value = if encoded {
        decode_header_value(raw)
    } else {
        Some(raw.to_owned())
    };
    match value {
        None => Err(header_mismatch(id, format!("malformed {name} header"))),
        Some(value) if value != expected => Err(header_mismatch(
            id,
            format!("{name} header value '{value}' does not match body value '{expected}'"),
        )),
        Some(_) => Ok(()),
    }
}

fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let digest = Sha256::new()
        .chain_update(nanos.to_le_bytes())
        .chain_update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes())
        .chain_update(std::process::id().to_le_bytes())
        .finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let temp = path.with_file_name(name);
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
}

// ---------------------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------------------

fn serve_stdio(server: &Arc<Server>) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut delayed = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = input
            .by_ref()
            .take(MAX_BODY as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_BODY {
            skip_line(&mut input)?;
            emit_stdio(&rpc_error(
                &Value::Null,
                400,
                -32600,
                "message exceeds 1 MiB",
                None,
            ));
            continue;
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let msg = match serde_json::from_slice::<Value>(line.trim_ascii()) {
            Ok(msg) => msg,
            Err(error) => {
                emit_stdio(&parse_error(&error));
                continue;
            }
        };
        match server.handle(&msg, &Transport::Stdio) {
            Outcome::Call(call) if call.fault.delay_ms.is_some() => {
                let server = Arc::clone(server);
                delayed.push(thread::spawn(move || finish_delayed_stdio(&server, call)));
            }
            outcome => emit_stdio(&server.resolve(outcome)),
        }
    }
    for handle in delayed {
        let _ = handle.join();
    }
    Ok(())
}

/// Sleeps, applies the call, and replies unless the client cancelled it meanwhile.
fn finish_delayed_stdio(server: &Server, call: PlannedCall) {
    thread::sleep(Duration::from_millis(
        call.fault.delay_ms.unwrap_or_default(),
    ));
    let key = call.id.to_string();
    let reply = server.finish(call);
    if !server.lock().cancelled.remove(&key) {
        emit_stdio(&reply);
    }
}

fn skip_line(input: &mut impl BufRead) -> io::Result<()> {
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        if let Some(end) = buffer.iter().position(|&byte| byte == b'\n') {
            input.consume(end + 1);
            return Ok(());
        }
        let len = buffer.len();
        input.consume(len);
    }
}

fn emit_stdio(reply: &Reply) {
    if let Reply::Json { body, .. } = reply {
        let mut out = io::stdout().lock();
        let written = out
            .write_all(format!("{body}\n").as_bytes())
            .and_then(|()| out.flush());
        if let Err(error) = written {
            eprintln!("{SERVER_NAME}: stdout: {error}");
        }
    }
}

// ---------------------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------------------

/// Request headers with lowercase names, in arrival order.
#[derive(Debug, Default)]
struct Headers(Vec<(String, String)>);

impl Headers {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Head {
    method: String,
    path: String,
    headers: Headers,
}

struct HttpResponse {
    status: u16,
    content_type: Option<&'static str>,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn empty(status: u16) -> Self {
        let mut response = Self {
            status,
            content_type: None,
            headers: Vec::new(),
            body: Vec::new(),
        };
        if status == 405 {
            response.headers.push(("Allow", "POST".into()));
        }
        response
    }
}

fn serve_http(server: &Arc<Server>, addr_file: &Path) -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
    write_atomically(addr_file, url.as_bytes())?;
    eprintln!("{SERVER_NAME}: listening on {url}");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = Arc::clone(server);
                thread::spawn(move || serve_connection(&server, stream));
            }
            Err(error) => eprintln!("{SERVER_NAME}: accept: {error}"),
        }
    }
    Ok(())
}

fn serve_connection(server: &Server, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(&stream);
    let response = match read_head(&mut reader) {
        Ok(head) => server.exchange(&head, &mut reader),
        Err(Some(status)) => Some(HttpResponse::empty(status)),
        Err(None) => return,
    };
    let Some(response) = response else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    if let Err(error) = write_response(&stream, &response) {
        // The client went away (for example, it cancelled a delayed call).
        eprintln!("{SERVER_NAME}: write: {error}");
        return;
    }
    // Drain any unread body so closing does not reset the connection before the client
    // reads the response.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = io::copy(&mut reader.take(4 * MAX_BODY as u64), &mut io::sink());
}

impl Server {
    /// Handles one HTTP request; `None` closes the connection without a response.
    fn exchange(&self, head: &Head, reader: &mut impl Read) -> Option<HttpResponse> {
        if let Some((name, expected)) = &self.config.require_header
            && head.headers.get(name) != Some(expected.as_str())
        {
            return Some(HttpResponse::empty(401));
        }
        if head.path != "/mcp" {
            return Some(HttpResponse::empty(404));
        }
        match head.method.as_str() {
            "POST" => {}
            "DELETE" if self.config.era != Era::Modern => {
                return Some(self.delete_session(&head.headers));
            }
            _ => return Some(HttpResponse::empty(405)),
        }
        if !origin_allowed(head.headers.get("origin")) {
            return Some(HttpResponse::empty(403));
        }
        let length = match head.headers.get("content-length").map(str::parse::<usize>) {
            None => return Some(HttpResponse::empty(411)),
            Some(Err(_)) => return Some(HttpResponse::empty(400)),
            Some(Ok(length)) if length > MAX_BODY => return Some(HttpResponse::empty(413)),
            Some(Ok(length)) => length,
        };
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        let msg = match serde_json::from_slice::<Value>(&body) {
            Ok(msg) => msg,
            Err(error) => return render(parse_error(&error), false, &Value::Null),
        };
        let reply = self.resolve(self.handle(&msg, &Transport::Http(&head.headers)));
        render(reply, self.config.sse, &msg)
    }

    fn delete_session(&self, headers: &Headers) -> HttpResponse {
        match headers.get("mcp-session-id") {
            Some(session) if self.lock().sessions.remove(session) => HttpResponse::empty(200),
            Some(_) => HttpResponse::empty(404),
            None => HttpResponse::empty(405),
        }
    }
}

fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = if rest.starts_with('[') {
        rest.split_inclusive(']').next().unwrap_or_default()
    } else {
        rest.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn render(reply: Reply, sse: bool, msg: &Value) -> Option<HttpResponse> {
    match reply {
        Reply::Drop => None,
        Reply::Accepted => Some(HttpResponse::empty(202)),
        Reply::Empty(status) => Some(HttpResponse::empty(status)),
        Reply::Text(status, text) => Some(HttpResponse {
            status,
            content_type: Some("text/plain; charset=utf-8"),
            headers: Vec::new(),
            body: text.into_bytes(),
        }),
        Reply::Json {
            status,
            body,
            session,
        } => {
            let mut headers = Vec::new();
            if let Some(session) = session {
                headers.push(("Mcp-Session-Id", session));
            }
            let stream = sse && status == 200 && msg.get("id").is_some();
            let (content_type, body) = if stream {
                headers.push(("Cache-Control", "no-cache".into()));
                headers.push(("X-Accel-Buffering", "no".into()));
                ("text/event-stream", sse_body(&body, progress_token(msg)))
            } else {
                ("application/json", body.to_string().into_bytes())
            };
            Some(HttpResponse {
                status,
                content_type: Some(content_type),
                headers,
                body,
            })
        }
    }
}

fn progress_token(msg: &Value) -> Option<&Value> {
    msg.pointer("/params/_meta/progressToken")
        .filter(|token| !token.is_null())
}

fn sse_body(response: &Value, progress_token: Option<&Value>) -> Vec<u8> {
    let mut out = String::from(": keep-alive\n\n");
    if let Some(token) = progress_token {
        let progress = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": token, "progress": 1, "total": 1, "message": "working" },
        });
        out.push_str(&format!("event: message\ndata: {progress}\n\n"));
    }
    out.push_str(&format!("event: message\ndata: {response}\n\n"));
    out.into_bytes()
}

fn read_head(reader: &mut impl BufRead) -> Result<Head, Option<u16>> {
    let mut budget = MAX_HEAD;
    let request_line = read_line(reader, &mut budget)?;
    let mut parts = request_line.split_ascii_whitespace();
    let (Some(method), Some(target), Some(_version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Some(400));
    };
    let path = target.split('?').next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(Some(400))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Head {
        method: method.to_owned(),
        path,
        headers: Headers(headers),
    })
}

/// Reads one CRLF- or LF-terminated line. `Err(None)` means the peer closed or failed.
fn read_line(reader: &mut impl BufRead, budget: &mut usize) -> Result<String, Option<u16>> {
    let mut line = Vec::new();
    let read = reader
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| None)?;
    if read > *budget {
        return Err(Some(431));
    }
    if read == 0 || !line.ends_with(b"\n") {
        return Err(None);
    }
    *budget -= read;
    let line = String::from_utf8(line).map_err(|_| Some(400))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn write_response(mut out: impl Write, response: &HttpResponse) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason(response.status),
        response.body.len()
    );
    if let Some(content_type) = response.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(&response.body)?;
    out.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------

fn main() -> ExitCode {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{SERVER_NAME}: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let server = match Server::open(config) {
        Ok(server) => Arc::new(server),
        Err(error) => {
            eprintln!("{SERVER_NAME}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match &server.config.mode {
        Mode::Stdio => serve_stdio(&server),
        Mode::Http(addr_file) => serve_http(&server, addr_file),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{SERVER_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(era: Era, page_size: Option<usize>, faults: &[&str]) -> Server {
        let faults = faults
            .iter()
            .map(|spec| parse_fault(spec).expect("valid fault"))
            .collect();
        let config = Config {
            mode: Mode::Stdio,
            era,
            state: None,
            ledger: None,
            sse: false,
            require_header: None,
            page_size,
            faults: HashMap::new(),
        };
        Server::new(config, BTreeMap::new(), faults, None, 0)
    }

    fn modern_request(id: u64, method: &str, mut params: Value) -> Value {
        params["_meta"] = json!({ VERSION_KEY: MODERN, CAPABILITIES_KEY: {} });
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn body(server: &Server, msg: &Value, transport: &Transport<'_>) -> (u16, Value) {
        match server.resolve(server.handle(msg, transport)) {
            Reply::Json { status, body, .. } => (status, body),
            other => panic!("expected a JSON reply, got {other:?}"),
        }
    }

    fn call(server: &Server, tool: &str, args: Value) -> Value {
        let msg = modern_request(1, "tools/call", json!({ "name": tool, "arguments": args }));
        body(server, &msg, &Transport::Stdio).1["result"].clone()
    }

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers(
            pairs
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn decodes_header_sentinels() {
        assert_eq!(decode_header_value("us-west1").as_deref(), Some("us-west1"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(
            decode_header_value("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=").as_deref(),
            Some("=?base64?literal?=")
        );
        assert_eq!(decode_header_value("=?base64?not base64!?="), None);
        assert_eq!(decode_header_value("=?base64?/w==?="), None, "not UTF-8");
        assert_eq!(decode_header_value("bad\u{1}value"), None);
        assert_eq!(
            decode_header_value("café"),
            None,
            "non-ASCII must be encoded"
        );
    }

    #[test]
    fn paginates_with_opaque_cursors() {
        let (first, next) = page(0, Some(4));
        assert_eq!(first.len(), 4);
        assert_eq!(first[0]["name"], "notes.list");
        let next = next.expect("more pages");
        let offset = decode_cursor(&next).expect("issued cursor");
        assert_eq!(offset, 4);
        let (second, next) = page(offset, Some(4));
        assert_eq!(second[0]["name"], "notes.delete");
        let (last, none) = page(decode_cursor(&next.expect("third page")).unwrap(), Some(4));
        assert_eq!(last.len(), 1);
        assert_eq!(none, None);
        assert_eq!(page(0, None).0.len(), TOOLS.len());

        for bad in [
            "bogus",
            "",
            &encode_cursor(0),
            &encode_cursor(TOOLS.len()),
            "Kzg=",
        ] {
            assert_eq!(decode_cursor(bad), None, "{bad}");
        }
        let server = server(Era::Modern, Some(4), &[]);
        let msg = modern_request(7, "tools/list", json!({ "cursor": "bogus" }));
        let (_, reply) = body(&server, &msg, &Transport::Stdio);
        assert_eq!(reply["error"]["code"], -32602);
        let msg = modern_request(8, "tools/list", json!({}));
        let (_, reply) = body(&server, &msg, &Transport::Stdio);
        assert_eq!(reply["result"]["tools"].as_array().map(Vec::len), Some(4));
        assert_eq!(reply["result"]["nextCursor"], json!(encode_cursor(4)));
        assert_eq!(reply["result"]["resultType"], "complete");
    }

    #[test]
    fn validates_arguments() {
        let put = find_tool("notes.put").unwrap();
        assert!(validate_args(put, &json!({ "id": "a", "text": "b" })).is_ok());
        let cases = [
            (json!({ "id": "a" }), "missing required property `text`"),
            (
                json!({ "id": "a", "text": "b", "x": 1 }),
                "unexpected property `x`",
            ),
            (
                json!({ "id": 1, "text": "b" }),
                "property `id` must be a string",
            ),
            (json!(["a"]), "arguments must be an object"),
        ];
        for (args, message) in cases {
            assert_eq!(validate_args(put, &args).unwrap_err(), message);
        }
        assert!(validate_args(find_tool("notes.list").unwrap(), &json!({})).is_ok());

        let server = server(Era::Modern, None, &[]);
        let result = call(&server, "notes.put", json!({ "id": "a" }));
        assert_eq!(result["isError"], true);
        assert!(
            server.lock().notes.is_empty(),
            "no effect on invalid arguments"
        );
        assert_eq!(server.lock().seq, 0, "no ledger entry on invalid arguments");
    }

    #[test]
    fn tools_change_state() {
        let mut notes = BTreeMap::new();
        let run = |notes: &mut BTreeMap<String, String>, name: &str, args: Value| {
            run_tool(notes, name, args.as_object().unwrap())
        };
        let put = run(&mut notes, "notes.put", json!({ "id": "b", "text": "two" }));
        assert_eq!(put.structured, Some(json!({ "id": "b", "text": "two" })));
        let append = run(
            &mut notes,
            "notes.append",
            json!({ "id": "a", "text": "x" }),
        );
        assert_eq!(append.structured, Some(json!({ "id": "a", "text": "x\n" })));
        run(
            &mut notes,
            "notes.append",
            json!({ "id": "a", "text": "y" }),
        );
        assert_eq!(notes["a"], "x\ny\n");
        let list = run(&mut notes, "notes.list", json!({}));
        assert_eq!(
            list.structured,
            Some(
                json!({ "notes": [{ "id": "a", "text": "x\ny\n" }, { "id": "b", "text": "two" }] })
            )
        );
        assert_eq!(list.text, list.structured.as_ref().unwrap().to_string());
        let get = run(&mut notes, "notes.get", json!({ "id": "b" }));
        assert_eq!(get.structured, Some(json!({ "id": "b", "text": "two" })));
        assert_eq!(
            run(&mut notes, "notes.get", json!({ "id": "zz" })),
            ToolOutput::error("note not found")
        );
        let delete = run(&mut notes, "notes.delete", json!({ "id": "b" }));
        assert_eq!(
            delete.structured,
            Some(json!({ "id": "b", "deleted": true }))
        );
        assert!(!notes.contains_key("b"));
        assert!(!run(&mut notes, "notes.delete", json!({ "id": "b" })).is_error);
        assert_eq!(
            run(&mut notes, "raw.text", json!({})),
            ToolOutput::text("plain words, not JSON")
        );
        assert_eq!(
            run(&mut notes, "raw.json_text", json!({})),
            ToolOutput::text(r#"{"answer":42}"#)
        );
        let echo = run(
            &mut notes,
            "region.echo",
            json!({ "region": "eu", "query": "q" }),
        );
        assert_eq!(
            echo.structured,
            Some(json!({ "region": "eu", "query": "q" }))
        );
        let plain = run(&mut notes, "plain.unannotated", json!({ "value": "v" }));
        assert_eq!(plain.structured, Some(json!({ "ok": true })));
        assert_eq!(notes["unannotated"], "v\n");
    }

    #[test]
    fn tool_definitions_match_the_contract() {
        let names: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            [
                "notes.list",
                "notes.get",
                "notes.put",
                "notes.append",
                "notes.delete",
                "raw.text",
                "raw.json_text",
                "region.echo",
                "plain.unannotated"
            ]
        );
        let unannotated = tool_json(find_tool("plain.unannotated").unwrap());
        assert!(unannotated.get("annotations").is_none());
        let echo = tool_json(find_tool("region.echo").unwrap());
        assert_eq!(
            echo["inputSchema"]["properties"]["region"]["x-mcp-header"],
            "Region"
        );
        assert_eq!(echo["inputSchema"]["additionalProperties"], false);
        assert!(tool_json(&TOOLS[0]).get("outputSchema").is_some());
    }

    #[test]
    fn faults_clear_unless_sticky() {
        let server = server(
            Era::Modern,
            None,
            &["notes.put=is-error", "notes.append=rpc-error,sticky"],
        );
        let args = json!({ "id": "a", "text": "t" });
        assert_eq!(call(&server, "notes.put", args.clone())["isError"], true);
        assert!(server.lock().notes.is_empty());
        assert!(
            call(&server, "notes.put", args.clone())
                .get("isError")
                .is_none()
        );
        for _ in 0..2 {
            let msg = modern_request(
                2,
                "tools/call",
                json!({ "name": "notes.append", "arguments": args }),
            );
            assert_eq!(
                body(&server, &msg, &Transport::Stdio).1["error"]["code"],
                -32603
            );
        }
        assert_eq!(server.lock().notes["a"], "t");
    }

    #[test]
    fn modern_rejects_bad_metadata_and_versions() {
        let server = server(Era::Modern, None, &[]);
        let bare = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
        let (status, reply) = body(&server, &bare, &Transport::Stdio);
        assert_eq!(
            (status, reply["error"]["code"].clone()),
            (400, json!(-32602))
        );

        let mut old = modern_request(2, "tools/list", json!({}));
        old["params"]["_meta"][VERSION_KEY] = json!("1900-01-01");
        let (status, reply) = body(&server, &old, &Transport::Stdio);
        assert_eq!(status, 400);
        assert_eq!(
            reply["error"]["data"],
            json!({ "supported": [MODERN], "requested": "1900-01-01" })
        );

        let init = json!({ "jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {} });
        let (_, reply) = body(&server, &init, &Transport::Stdio);
        assert_eq!(reply["error"]["code"], -32601);
        assert!(reply["error"]["message"].as_str().unwrap().contains(MODERN));
    }

    #[test]
    fn modern_http_checks_mirrored_headers() {
        let server = server(Era::Modern, None, &[]);
        let msg = modern_request(
            1,
            "tools/call",
            json!({ "name": "region.echo", "arguments": { "region": "eu 1", "query": "q" } }),
        );
        let base = [
            ("Accept", "application/json, text/event-stream"),
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "=?base64?cmVnaW9uLmVjaG8=?="),
        ];
        let good = headers(&[&base[..], &[("Mcp-Param-Region", "eu 1")]].concat());
        let (status, reply) = body(&server, &msg, &Transport::Http(&good));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["structuredContent"]["region"], "eu 1");

        let missing = headers(&base);
        let (status, reply) = body(&server, &msg, &Transport::Http(&missing));
        assert_eq!(
            (status, reply["error"]["code"].clone()),
            (400, json!(-32020))
        );

        let wrong_name = headers(&[&base[..3], &[("Mcp-Name", "notes.get")]].concat());
        let (status, reply) = body(&server, &msg, &Transport::Http(&wrong_name));
        assert_eq!(
            (status, reply["error"]["code"].clone()),
            (400, json!(-32020))
        );

        let no_accept = headers(&base[1..]);
        let reply = server.resolve(server.handle(&msg, &Transport::Http(&no_accept)));
        assert!(matches!(reply, Reply::Empty(406)));
    }

    #[test]
    fn legacy_requires_initialize() {
        let server = server(Era::Legacy, None, &[]);
        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let (status, reply) = body(&server, &list, &Transport::Stdio);
        assert_eq!(
            (status, reply["error"]["code"].clone()),
            (400, json!(-32600))
        );
        let init = json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {
            "protocolVersion": "2099-01-01", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" }
        }});
        let (_, reply) = body(&server, &init, &Transport::Stdio);
        assert_eq!(reply["result"]["protocolVersion"], LEGACY);
        let (_, reply) = body(&server, &list, &Transport::Stdio);
        assert!(reply["result"].get("resultType").is_none());
        assert_eq!(
            reply["result"]["tools"].as_array().map(Vec::len),
            Some(TOOLS.len())
        );
    }

    #[test]
    fn parses_command_lines() {
        let args = |line: &str| parse_args(line.split_whitespace().map(str::to_owned));
        let config = args("--http a --era dual --sse --page-size 3 --fault notes.put=delay-ms:50,drop-reply,sticky")
            .unwrap();
        assert_eq!(config.era, Era::Dual);
        assert_eq!(
            config.faults["notes.put"],
            Fault {
                delay_ms: Some(50),
                action: Some(FaultAction::DropReply),
                sticky: true
            }
        );
        assert!(args("--stdio --sse").is_err());
        assert!(args("--stdio --fault nope=is-error").is_err());
        assert!(args("--stdio --fault notes.put=is-error,rpc-error").is_err());
        assert!(args("--era modern").is_err());
    }
}
