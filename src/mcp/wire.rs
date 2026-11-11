//! MCP message shapes this client reads and writes.
//!
//! Follows the 2026-07-28 revision, and reads the older handshake-based
//! revisions where they differ (no `resultType`, no per-request `_meta`).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::plan::{Digest, canonical_json};

/// The protocol revision this client prefers.
pub const MODERN_VERSION: &str = "2026-07-28";
/// Handshake-based revisions this client can fall back to, newest first.
pub const LEGACY_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// JSON-RPC error codes that say the server did not act on the request: parse
/// error, method not found, header mismatch, missing capability, and
/// unsupported version. "Invalid request" (-32600) and "invalid params"
/// (-32602) are left out: a server can raise them from a tool handler after
/// the tool did its work, for example when its own output fails validation.
const NOT_PROCESSED_CODES: [i64; 5] = [-32700, -32601, -32020, -32021, -32022];
/// The `UnsupportedProtocolVersion` error code.
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// Error codes only a 2026-07-28 or later server sends.
pub const MODERN_ERROR_CODES: [i64; 3] = [-32020, -32021, -32022];

/// Longest tool-error or diagnostic text kept from a server.
pub const MAX_MESSAGE_BYTES: usize = 1024;

/// This client's name and version, sent with every request.
#[must_use]
pub fn client_info() -> Value {
    json!({"name": "codemode", "version": env!("CARGO_PKG_VERSION")})
}

/// The `_meta` every 2026-07-28 request carries. This client declares no
/// capabilities: it cannot answer elicitation, sampling, or roots requests.
#[must_use]
pub fn request_meta() -> Value {
    json!({
        META_VERSION: MODERN_VERSION,
        META_CLIENT: client_info(),
        META_CAPABILITIES: {},
    })
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcError {
    /// The error code.
    pub code: i64,
    /// The server's explanation.
    #[serde(default)]
    pub message: String,
    /// Additional data, such as the supported versions.
    #[serde(default)]
    pub data: Option<Value>,
}

impl RpcError {
    /// Whether the code says the server did not act on the request.
    #[must_use]
    pub fn not_processed(&self) -> bool {
        NOT_PROCESSED_CODES.contains(&self.code)
    }
}

/// What a server says about a tool's behavior. Every field is a hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotations {
    /// The tool does not change its environment. Absent means `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may destroy data. Absent means `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Calling it again with the same arguments has no further effect. Absent means `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool reaches systems outside the server. Absent means `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// One tool as the server lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// The name used to call it.
    pub name: String,
    /// The server's description, for people and models.
    pub description: Option<String>,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
    /// JSON Schema of `structuredContent`, when declared.
    pub output_schema: Option<Value>,
    /// The server's hints.
    pub annotations: Annotations,
    /// Arguments mirrored into `Mcp-Param-*` headers on Streamable HTTP, as
    /// (header name suffix, property path).
    pub header_parameters: Vec<(String, Vec<String>)>,
}

impl Tool {
    /// Read one entry of a `tools/list` result.
    ///
    /// # Errors
    /// Returns why the entry is not a usable tool definition, including an
    /// `x-mcp-header` annotation that breaks the specification's rules; such a
    /// tool must be left out of the list.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or("a tool has no name")?
            .to_string();
        let input_schema = value
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .ok_or_else(|| format!("tool {name:?} has no inputSchema object"))?
            .clone();
        let annotations = match value.get("annotations") {
            None | Some(Value::Null) => Annotations::default(),
            Some(annotations) => Annotations::deserialize(annotations)
                .map_err(|error| format!("tool {name:?} has invalid annotations: {error}"))?,
        };
        let mut header_parameters = Vec::new();
        collect_headers(&input_schema, &mut Vec::new(), true, &mut header_parameters)
            .map_err(|reason| format!("tool {name:?}: {reason}"))?;
        header_parameters.sort();
        let mut seen = std::collections::HashSet::new();
        for (header, _) in &header_parameters {
            if !seen.insert(header.to_ascii_lowercase()) {
                return Err(format!(
                    "tool {name:?}: x-mcp-header {header:?} is used twice"
                ));
            }
        }
        Ok(Self {
            description: value
                .get("description")
                .and_then(Value::as_str)
                .map(String::from),
            output_schema: value.get("outputSchema").filter(|v| v.is_object()).cloned(),
            name,
            input_schema,
            annotations,
            header_parameters,
        })
    }

    /// A digest of what an approval relies on: the name, the argument schema,
    /// and the annotations. The description is left out, so a reworded
    /// description does not invalidate an approval.
    #[must_use]
    pub fn definition_digest(&self) -> Digest {
        let value = json!({
            "name": self.name,
            "inputSchema": self.input_schema,
            "annotations": self.annotations,
        });
        Digest::of(canonical_json(value).as_bytes())
    }

    /// The `Mcp-Param-*` headers for a call with `arguments`.
    #[must_use]
    pub fn parameter_headers(&self, arguments: &Value) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        for (header, path) in &self.header_parameters {
            let mut value = arguments;
            for key in path {
                value = match value.get(key) {
                    Some(inner) => inner,
                    None => &Value::Null,
                };
            }
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
                // Absent, null, or a type the schema rule does not allow: no header.
                _ => continue,
            };
            headers.push((format!("Mcp-Param-{header}"), header_value(&text)));
        }
        headers
    }

    /// Check `arguments` against the top level of the argument schema: it must
    /// be an object, hold every `required` property, hold no unknown property
    /// when `additionalProperties` is `false`, and match each property's simple
    /// `type`. The server remains the authority; this catches a wrong plan
    /// before a person approves it.
    ///
    /// # Errors
    /// Returns the first mismatch.
    pub fn check_arguments(&self, arguments: &Value) -> Result<(), String> {
        let Some(arguments) = arguments.as_object() else {
            return Err("arguments must be a JSON object".into());
        };
        let schema = &self.input_schema;
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !arguments.contains_key(name) {
                return Err(format!("required argument {name:?} is missing"));
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (name, value) in arguments {
            match properties.and_then(|properties| properties.get(name)) {
                Some(property) => {
                    if let Some(expected) = property.get("type").and_then(Value::as_str)
                        && !has_type(value, expected)
                    {
                        return Err(format!("argument {name:?} must be of type {expected}"));
                    }
                }
                None if closed => {
                    return Err(format!("argument {name:?} is not in the tool's schema"));
                }
                None => {}
            }
        }
        Ok(())
    }
}

fn has_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        // An unknown type keyword is the server's to judge.
        _ => true,
    }
}

/// Find `x-mcp-header` annotations. They are valid only on primitive properties
/// reached from the root through `properties` keys alone.
fn collect_headers(
    schema: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    out: &mut Vec<(String, Vec<String>)>,
) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(header) = object.get("x-mcp-header") {
        let name = header.as_str().unwrap_or_default();
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
        let primitive = matches!(
            object.get("type").and_then(Value::as_str),
            Some("string" | "integer" | "boolean")
        );
        if !reachable || path.is_empty() || !token || !primitive {
            return Err(format!("invalid x-mcp-header at {:?}", path.join(".")));
        }
        out.push((name.to_string(), path.clone()));
    }
    for (keyword, child) in object {
        match (keyword.as_str(), child) {
            ("properties", Value::Object(properties)) => {
                for (name, property) in properties {
                    path.push(name.clone());
                    collect_headers(property, path, reachable, out)?;
                    path.pop();
                }
            }
            ("x-mcp-header", _) => {}
            // Anywhere else an annotation is not statically reachable.
            (_, Value::Object(_)) => collect_headers(child, path, false, out)?,
            (_, Value::Array(items)) => {
                for item in items {
                    collect_headers(item, path, false, out)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Encode `text` as an HTTP header value: as is when it is plain visible ASCII
/// without surrounding spaces, otherwise in the `=?base64?...?=` form.
#[must_use]
pub fn header_value(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte == b' ' || byte == b'\t' || (0x21..0x7f).contains(&byte))
        && !text.starts_with([' ', '\t'])
        && !text.ends_with([' ', '\t'])
        && !(text.starts_with("=?base64?") && text.ends_with("?="));
    if plain {
        text.to_string()
    } else {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        format!("=?base64?{encoded}?=")
    }
}

/// What a completed `tools/call` returned.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    /// The tool ran; this is its value.
    Value(Value),
    /// The tool reported an error (`isError`), with its whole text. The
    /// caller removes configured values from it before it cuts it short.
    ToolError(String),
    /// The server wants more input (elicitation, sampling, or roots), which
    /// this client cannot give.
    InputRequired,
}

impl ToolOutcome {
    /// Read a `tools/call` result.
    ///
    /// The value is `structuredContent` when present. Otherwise, when every
    /// content block is text, it is the joined text, parsed as JSON when it is
    /// JSON. Otherwise it is the content array itself.
    ///
    /// # Errors
    /// Returns why the result is not a valid tool result.
    pub fn parse(result: &Value) -> Result<Self, String> {
        let object = result.as_object().ok_or("the result is not an object")?;
        match object.get("resultType").and_then(Value::as_str) {
            None | Some("complete") => {}
            Some("input_required") => return Ok(Self::InputRequired),
            Some(other) => return Err(format!("unknown resultType {other:?}")),
        }
        let empty = Vec::new();
        let content = match object.get("content") {
            None => &empty,
            Some(Value::Array(content)) => content,
            Some(_) => return Err("content is not an array".into()),
        };
        let text = joined_text(content);
        if object.get("isError").and_then(Value::as_bool) == Some(true) {
            let message = text.unwrap_or_else(|| "the tool reported an error".into());
            return Ok(Self::ToolError(message));
        }
        if let Some(structured) = object.get("structuredContent") {
            return Ok(Self::Value(structured.clone()));
        }
        Ok(Self::Value(match text {
            Some(text) => crate::json::parse_json(&text).unwrap_or(Value::String(text)),
            None => Value::Array(content.clone()),
        }))
    }
}

/// The text of `content` when it holds only text blocks, and at least one.
fn joined_text(content: &[Value]) -> Option<String> {
    let mut parts = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        parts.push(block.get("text").and_then(Value::as_str)?);
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// `text` cut to at most `limit` bytes on a character boundary.
#[must_use]
pub fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// A decoded JSON-RPC response: its result, or its error.
pub type Reply = Result<Value, RpcError>;

/// Read one JSON-RPC message. Returns the id and reply when it is a response
/// to a request, and `None` for a notification or anything else.
#[must_use]
pub fn response(message: &Value) -> Option<(Option<u64>, Reply)> {
    let object: &Map<String, Value> = message.as_object()?;
    let id = object.get("id").and_then(Value::as_u64);
    if let Some(error) = object.get("error") {
        let error = RpcError::deserialize(error).ok()?;
        return Some((id, Err(error)));
    }
    let result = object.get("result")?;
    id?;
    Some((id, Ok(result.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_use_the_base64_form_only_when_needed() {
        assert_eq!(header_value("us-west1"), "us-west1");
        assert_eq!(
            header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(header_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(
            header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn header_parameters_must_be_statically_reachable_primitives() {
        let tool = Tool::parse(&json!({
            "name": "sql",
            "inputSchema": {"type": "object", "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "nested": {"type": "object", "properties": {
                    "shard": {"type": "integer", "x-mcp-header": "Shard"}}},
                "query": {"type": "string"}}}
        }))
        .unwrap();
        let headers = tool
            .parameter_headers(&json!({"region": "eu", "nested": {"shard": 7}, "query": "select"}));
        assert_eq!(
            headers,
            [
                ("Mcp-Param-Region".to_string(), "eu".to_string()),
                ("Mcp-Param-Shard".to_string(), "7".to_string())
            ]
        );
        assert!(tool.parameter_headers(&json!({"region": null})).is_empty());

        let invalid = |schema: Value| Tool::parse(&json!({"name": "t", "inputSchema": schema}));
        assert!(
            invalid(json!({"type": "object", "properties": {
            "list": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}))
            .is_err()
        );
        assert!(
            invalid(json!({"type": "object", "properties": {
            "n": {"type": "number", "x-mcp-header": "A"}}}))
            .is_err()
        );
        assert!(
            invalid(json!({"type": "object", "properties": {
            "a": {"type": "string", "x-mcp-header": "Same"},
            "b": {"type": "string", "x-mcp-header": "same"}}}))
            .is_err()
        );
        assert!(
            invalid(json!({"type": "object", "properties": {
            "a": {"type": "string", "x-mcp-header": "bad name"}}}))
            .is_err()
        );
    }

    #[test]
    fn the_definition_digest_ignores_the_description_only() {
        let tool = |description: &str, hint: bool| {
            Tool::parse(&json!({
                "name": "put", "description": description,
                "inputSchema": {"type": "object"},
                "annotations": {"idempotentHint": hint}
            }))
            .unwrap()
            .definition_digest()
        };
        assert_eq!(tool("one", true), tool("two", true));
        assert_ne!(tool("one", true), tool("one", false));
    }

    #[test]
    fn outcomes_prefer_structured_content_then_json_text_then_text() {
        let parse = |value: Value| ToolOutcome::parse(&value).unwrap();
        assert_eq!(
            parse(json!({"content": [{"type": "text", "text": "x"}],
                "structuredContent": {"a": 1}})),
            ToolOutcome::Value(json!({"a": 1}))
        );
        assert_eq!(
            parse(json!({"content": [{"type": "text", "text": "{\"a\":2}"}]})),
            ToolOutcome::Value(json!({"a": 2}))
        );
        assert_eq!(
            parse(json!({"resultType": "complete",
                "content": [{"type": "text", "text": "plain words"}]})),
            ToolOutcome::Value(json!("plain words"))
        );
        assert_eq!(
            parse(json!({"content": [{"type": "image", "data": "", "mimeType": "image/png"}]})),
            ToolOutcome::Value(json!([{"type": "image", "data": "", "mimeType": "image/png"}]))
        );
        assert_eq!(
            parse(json!({"content": [{"type": "text", "text": "no"}], "isError": true})),
            ToolOutcome::ToolError("no".into())
        );
        assert_eq!(
            parse(json!({"resultType": "input_required", "requestState": "s"})),
            ToolOutcome::InputRequired
        );
        assert!(ToolOutcome::parse(&json!({"resultType": "other"})).is_err());
    }

    #[test]
    fn arguments_are_checked_against_the_top_level_schema() {
        let tool = Tool::parse(&json!({"name": "put", "inputSchema": {
            "type": "object", "additionalProperties": false,
            "properties": {"id": {"type": "string"}, "n": {"type": "integer"}},
            "required": ["id"]}}))
        .unwrap();
        tool.check_arguments(&json!({"id": "a", "n": 3})).unwrap();
        assert!(
            tool.check_arguments(&json!({"n": 3}))
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            tool.check_arguments(&json!({"id": 1}))
                .unwrap_err()
                .contains("type string")
        );
        assert!(
            tool.check_arguments(&json!({"id": "a", "x": 1}))
                .unwrap_err()
                .contains("not in the tool's schema")
        );
        assert!(tool.check_arguments(&json!([])).is_err());
    }
}
