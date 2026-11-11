//! The plan file: one self-contained change list for one MCP server.
//!
//! A plan file holds the writes a program or a person proposes, the identity
//! of the server they go to, and, for each tool they use, what was known
//! about the tool when the plan was made: its class, the digest of its
//! definition, and whether the server marks it destructive. A second person
//! can review the file, for example in a pull request, and one digest names
//! exactly what an approval binds.
//!
//! The file is pretty-printed with a fixed layout: keys in a fixed order, one
//! tool per line, one write per line, argument keys sorted. The same content
//! always gives the same bytes. It never holds a header or `env` value.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mcp::{Annotations, MCP_PROVIDER, McpProvider, ServerConfig, Tool, ToolClass};
use crate::plan::{
    Digest, FrozenPlan, MAX_PLAN_BYTES, Operation, PLAN_FORMAT_VERSION, Plan, PlanError, Scope,
    canonical_json,
};
use crate::recovery::{
    ProviderDeclaration, ReconcileMethod, RecoveryContract, RepeatSafety, Retention,
};
use crate::render::unsafe_to_print;

/// The plan file format this build reads and writes.
pub const PLAN_FILE_VERSION: u32 = 1;

/// One plan file.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFile {
    /// A short description, shown when the plan is reviewed.
    pub summary: Option<String>,
    /// The server the writes go to.
    pub server: PlanServer,
    /// What was known about each tool the writes use, by tool name.
    pub tools: BTreeMap<String, PlannedTool>,
    /// The writes, in the order they were proposed.
    pub writes: Vec<PlannedWrite>,
}

/// The server a plan was made for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanServer {
    /// The server identity; see [`ServerConfig::identity`].
    pub identity: String,
    /// The server file, relative to the plan file, so that later commands
    /// find it. The digest does not cover it: the file may move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Digest of the server file's bytes as `plan` read them. The plan's
    /// digest covers it, so an approval binds which server file is used:
    /// its command or URL, its `env` and header entries, and its
    /// `write_tools`.
    pub config: Digest,
    /// The MCP revision the server spoke when the plan was made.
    pub protocol: String,
    /// Whether tool classes came from the server's own annotations.
    pub trust_annotations: bool,
}

/// What was known about one tool when the plan was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedTool {
    /// `write` or `repeatable_write`.
    pub class: ToolClass,
    /// Digest of the tool's name, argument schema, and annotations.
    pub definition: Digest,
    /// Whether the server marks the tool destructive (`destructiveHint: true`).
    pub destructive: bool,
    /// Whether the server sent any annotation for the tool.
    pub annotated: bool,
}

impl PlannedTool {
    /// What a plan records about `tool` of class `class`.
    #[must_use]
    pub fn of(tool: &Tool, class: ToolClass) -> Self {
        Self {
            class,
            definition: tool.definition_digest(),
            destructive: tool.annotations.destructive_hint == Some(true),
            annotated: tool.annotations != Annotations::default(),
        }
    }

    /// Whether a write of this tool is sent again after a lost reply.
    #[must_use]
    pub fn repeatable(&self) -> bool {
        self.class == ToolClass::RepeatableWrite
    }
}

/// One proposed write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedWrite {
    /// Stable identity, unique in the plan: 1 to 64 of `A-Z a-z 0-9 . _ -`.
    pub id: String,
    /// The tool to call.
    pub tool: String,
    /// The arguments, a JSON object.
    #[serde(deserialize_with = "crate::json::value")]
    pub arguments: Value,
}

/// The file as serde reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlanFile {
    codemode_plan: u32,
    #[serde(default)]
    summary: Option<String>,
    server: PlanServer,
    tools: BTreeMap<String, PlannedTool>,
    operations: Vec<PlannedWrite>,
}

/// What a connected server says now: the facts a plan is made from and
/// checked against.
#[derive(Debug, Clone)]
pub struct ServerFacts {
    /// The server identity.
    pub identity: String,
    /// The MCP revision in use.
    pub protocol: String,
    /// Whether the server file trusts the server's annotations.
    pub trust_annotations: bool,
    /// The server file's `write_tools`, when it has one.
    pub write_tools: Option<Vec<String>>,
    /// Every usable tool with its class, by name.
    pub tools: BTreeMap<String, (Tool, ToolClass)>,
}

impl ServerFacts {
    /// The facts of a connected server and its server file.
    #[must_use]
    pub fn of(provider: &McpProvider, config: &ServerConfig) -> Self {
        Self {
            identity: provider.identity().to_string(),
            protocol: provider.protocol_version().to_string(),
            trust_annotations: config.trust_annotations,
            write_tools: config.write_tools.clone(),
            tools: provider
                .tools()
                .map(|(tool, class)| (tool.name.clone(), (tool.clone(), class)))
                .collect(),
        }
    }

    /// Whether a plan may hold the write tool `name`.
    #[must_use]
    pub fn allows_write(&self, name: &str) -> bool {
        self.write_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
    }
}

/// Why a plan file, or a proposed change list, cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFileError {
    /// The text is not a plan file, or a change list has the wrong shape.
    Format(String),
    /// The plan file declares a format version this build does not accept.
    UnsupportedVersion(u32),
    /// The writes are not a valid plan; see [`PlanError`].
    Plan(PlanError),
    /// A write cannot be planned for this server.
    Write {
        /// The write's identity.
        id: String,
        /// What is wrong.
        reason: String,
    },
    /// The server differs from what the plan records.
    ServerChanged(String),
}

impl fmt::Display for PlanFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(message) => formatter.write_str(message),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "plan file version {version} is not supported; expected {PLAN_FILE_VERSION}"
            ),
            Self::Plan(error) => error.fmt(formatter),
            Self::Write { id, reason } => write!(formatter, "write {id:?}: {reason}"),
            Self::ServerChanged(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PlanFileError {}

impl From<PlanError> for PlanFileError {
    fn from(error: PlanError) -> Self {
        Self::Plan(error)
    }
}

/// The shape of a change list, for error messages.
const SHAPE: &str = "{summary?, operations: [{id, tool, arguments}]}";

impl PlanFile {
    /// Turn a change list into a plan for the server `facts` describes.
    ///
    /// A change list is the JSON object `{summary?, operations: [{id, tool,
    /// arguments}]}` that a program returns or a person writes. Every
    /// operation must name a write tool the server lists and `write_tools`
    /// allows, with arguments the top level of the tool's schema accepts.
    /// `server_file` is recorded as given, and `server_config` is the digest
    /// of that file's bytes. Returns `None` for a change list without
    /// operations.
    ///
    /// # Errors
    /// Returns a [`PlanFileError`] that names the first operation that cannot
    /// be planned, or says how the change list is malformed.
    pub fn propose(
        change_list: &Value,
        facts: &ServerFacts,
        server_file: Option<String>,
        server_config: Digest,
    ) -> Result<Option<Self>, PlanFileError> {
        let wrong = || PlanFileError::Format(format!("a change list is {SHAPE}"));
        let object = change_list.as_object().ok_or_else(wrong)?;
        if let Some(key) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "summary" | "operations"))
        {
            return Err(PlanFileError::Format(format!(
                "unknown key {key:?}: a change list is {SHAPE}"
            )));
        }
        let summary = match object.get("summary") {
            None | Some(Value::Null) => None,
            Some(Value::String(summary)) => Some(summary.clone()),
            Some(_) => {
                return Err(PlanFileError::Format(format!(
                    "summary must be a string: a change list is {SHAPE}"
                )));
            }
        };
        let proposed = object
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(wrong)?;
        if proposed.is_empty() {
            return Ok(None);
        }
        let mut tools = BTreeMap::new();
        let mut writes = Vec::with_capacity(proposed.len());
        for (index, entry) in proposed.iter().enumerate() {
            let field = |name: &str| entry.get(name).and_then(Value::as_str);
            let (Some(id), Some(tool)) = (field("id"), field("tool")) else {
                return Err(PlanFileError::Format(format!(
                    "operation {index} needs a string id and tool"
                )));
            };
            let refused = |reason: String| PlanFileError::Write {
                id: id.to_string(),
                reason,
            };
            if let Some(key) = entry.as_object().and_then(|entry| {
                entry
                    .keys()
                    .find(|key| !matches!(key.as_str(), "id" | "tool" | "arguments"))
            }) {
                return Err(refused(format!(
                    "unknown key {key:?}; an MCP server cannot check preconditions or \
                     restrict a destination"
                )));
            }
            let arguments = entry.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match facts.tools.get(tool) {
                None => {
                    return Err(refused(format!(
                        "it names {tool:?}, which the server does not list"
                    )));
                }
                Some((_, ToolClass::Read)) => {
                    return Err(refused(format!(
                        "it names {tool:?}, a read tool; a plan holds writes only"
                    )));
                }
                Some(_) if !facts.allows_write(tool) => {
                    return Err(refused(format!(
                        "it names {tool:?}, which \"write_tools\" in the server file does not allow"
                    )));
                }
                Some((definition, class)) => {
                    definition.check_arguments(&arguments).map_err(refused)?;
                    tools
                        .entry(tool.to_string())
                        .or_insert_with(|| PlannedTool::of(definition, *class));
                }
            }
            writes.push(PlannedWrite {
                id: id.to_string(),
                tool: tool.to_string(),
                arguments,
            });
        }
        let plan = Self {
            summary,
            server: PlanServer {
                identity: facts.identity.clone(),
                file: server_file,
                config: server_config,
                protocol: facts.protocol.clone(),
                trust_annotations: facts.trust_annotations,
            },
            tools,
            writes,
        };
        plan.frozen()?;
        Ok(Some(plan))
    }

    /// Parse and validate a plan file.
    ///
    /// ```
    /// use codemode::plan_file::PlanFile;
    ///
    /// let text = r#"{
    ///   "codemode_plan": 1,
    ///   "server": {"identity": "https://mcp.example.com/mcp", "protocol": "2025-11-25", "trust_annotations": true,
    ///              "config": "sha256:1111111111111111111111111111111111111111111111111111111111111111"},
    ///   "tools": {
    ///     "notes.put": {"class": "repeatable_write", "definition": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "destructive": false, "annotated": true}
    ///   },
    ///   "operations": [
    ///     {"id": "put-a", "tool": "notes.put", "arguments": {"id": "a", "text": "one"}}
    ///   ]
    /// }"#;
    /// let plan = PlanFile::parse(text)?;
    /// assert_eq!(plan.writes.len(), 1);
    /// // The layout and the order of keys do not change the digest; a value does.
    /// assert_eq!(PlanFile::parse(&plan.to_text())?.digest(), plan.digest());
    /// let edited = PlanFile::parse(&text.replace("one", "two"))?;
    /// assert_ne!(edited.digest(), plan.digest());
    /// # Ok::<(), codemode::plan_file::PlanFileError>(())
    /// ```
    ///
    /// # Errors
    /// Returns a [`PlanFileError`] for text over 32 MiB, invalid JSON, an
    /// unknown key, an unsupported version, a write whose tool the file does
    /// not describe, a described tool no write uses, a tool of class `read`,
    /// or writes that are not a valid plan (see [`Plan::parse`]).
    pub fn parse(text: &str) -> Result<Self, PlanFileError> {
        if text.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge.into());
        }
        let raw: RawPlanFile = serde_json::from_str(text)
            .map_err(|error| PlanFileError::Format(format!("not a plan file: {error}")))?;
        if raw.codemode_plan != PLAN_FILE_VERSION {
            return Err(PlanFileError::UnsupportedVersion(raw.codemode_plan));
        }
        let plan = Self {
            summary: raw.summary,
            server: raw.server,
            tools: raw.tools,
            writes: raw.operations,
        };
        let used: BTreeSet<&str> = plan
            .writes
            .iter()
            .map(|write| write.tool.as_str())
            .collect();
        for write in &plan.writes {
            if !plan.tools.contains_key(&write.tool) {
                return Err(PlanFileError::Write {
                    id: write.id.clone(),
                    reason: format!("the plan file does not describe its tool {:?}", write.tool),
                });
            }
        }
        for (name, tool) in &plan.tools {
            if !used.contains(name.as_str()) {
                return Err(PlanFileError::Format(format!(
                    "the plan file describes tool {name:?}, which no write uses"
                )));
            }
            if tool.class == ToolClass::Read {
                return Err(PlanFileError::Format(format!(
                    "tool {name:?} is a read tool; a plan holds writes only"
                )));
            }
        }
        plan.frozen()?;
        Ok(plan)
    }

    /// The writes as the plan the journal and the executor use: every write
    /// acts through this plan's server, with no precondition.
    ///
    /// # Errors
    /// Returns the [`PlanError`] of the first invalid write: an invalid or
    /// repeated identity, arguments that are not an object or exceed 64 KiB, a
    /// key that names a credential, a number a JSON host would round, no
    /// writes, or more than 10,000.
    pub fn frozen(&self) -> Result<FrozenPlan, PlanError> {
        Plan {
            version: PLAN_FORMAT_VERSION,
            summary: self.summary.clone(),
            operations: self
                .writes
                .iter()
                .map(|write| Operation {
                    id: write.id.clone(),
                    tool: write.tool.clone(),
                    scope: Scope {
                        provider: MCP_PROVIDER.into(),
                        account: self.server.identity.clone(),
                        destination: None,
                    },
                    arguments: write.arguments.clone(),
                    preconditions: Vec::new(),
                })
                .collect(),
        }
        .freeze()
    }

    /// What an approval of this plan binds about the server, in the form the
    /// executor compares with the connected server: its identity, and for
    /// each tool whether a write may be repeated and the tool's definition.
    #[must_use]
    pub fn declaration(&self) -> ProviderDeclaration {
        let mut contracts = BTreeMap::new();
        let mut definitions = BTreeMap::new();
        for (name, tool) in &self.tools {
            let repeat = if tool.repeatable() {
                RepeatSafety::Safe
            } else {
                RepeatSafety::Unsafe
            };
            contracts.insert(
                name.clone(),
                RecoveryContract {
                    repeat,
                    reconcile: ReconcileMethod::None,
                },
            );
            definitions.insert(name.clone(), tool.definition.clone());
        }
        ProviderDeclaration {
            provider: MCP_PROVIDER.into(),
            account: self.server.identity.clone(),
            contracts,
            retention: Retention::Summary,
            definitions,
        }
    }

    /// The one digest of this plan: SHA-256 of its canonical JSON (keys
    /// sorted, no white space) without `server.file`. It covers the writes,
    /// the summary, the server identity, the digest of the server file, and
    /// every tool's class, definition digest, and flags. An approval names
    /// this digest.
    #[must_use]
    pub fn digest(&self) -> Digest {
        let mut value = self.value();
        if let Some(server) = value.get_mut("server").and_then(Value::as_object_mut) {
            server.remove("file");
        }
        Digest::of(canonical_json(value).as_bytes())
    }

    fn value(&self) -> Value {
        let mut value = json!({
            "codemode_plan": PLAN_FILE_VERSION,
            "server": self.server,
            "tools": self.tools,
            "operations": self.writes,
        });
        if let Some(summary) = &self.summary {
            value["summary"] = json!(summary);
        }
        value
    }

    /// [`Self::to_text`], refused when it exceeds the 32 MiB a plan file may
    /// have. Escaping can make the text several times larger than the change
    /// list it was made from.
    ///
    /// # Errors
    /// Returns [`PlanError::TooLarge`], wrapped, for a text no command would
    /// read back.
    pub fn checked_text(&self) -> Result<String, PlanFileError> {
        let text = self.to_text();
        if text.len() > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge.into());
        }
        Ok(text)
    }

    /// The file's text. The same plan always gives the same bytes: keys in a
    /// fixed order, one tool and one write per line, argument keys sorted,
    /// and a trailing newline. Characters a terminal or a diff viewer could
    /// act on are written as JSON `\u` escapes.
    #[must_use]
    pub fn to_text(&self) -> String {
        let line = |value: &Value| spaced_json(value);
        let mut out = String::from("{\n");
        out.push_str(&format!("  \"codemode_plan\": {PLAN_FILE_VERSION},\n"));
        if let Some(summary) = &self.summary {
            out.push_str(&format!("  \"summary\": {},\n", line(&json!(summary))));
        }
        out.push_str("  \"server\": {\n");
        let server = &self.server;
        let mut fields = vec![("identity", json!(server.identity))];
        if let Some(file) = &server.file {
            fields.push(("file", json!(file)));
        }
        fields.push(("config", json!(server.config)));
        fields.push(("protocol", json!(server.protocol)));
        fields.push(("trust_annotations", json!(server.trust_annotations)));
        for (index, (key, value)) in fields.iter().enumerate() {
            let comma = if index + 1 < fields.len() { "," } else { "" };
            out.push_str(&format!("    \"{key}\": {}{comma}\n", line(value)));
        }
        out.push_str("  },\n  \"tools\": {\n");
        for (index, (name, tool)) in self.tools.iter().enumerate() {
            let comma = if index + 1 < self.tools.len() {
                ","
            } else {
                ""
            };
            out.push_str(&format!(
                "    {}: {{\"class\": {}, \"definition\": {}, \"destructive\": {}, \"annotated\": {}}}{comma}\n",
                line(&json!(name)),
                line(&json!(tool.class)),
                line(&json!(tool.definition)),
                tool.destructive,
                tool.annotated,
            ));
        }
        out.push_str("  },\n  \"operations\": [\n");
        for (index, write) in self.writes.iter().enumerate() {
            let comma = if index + 1 < self.writes.len() {
                ","
            } else {
                ""
            };
            out.push_str(&format!(
                "    {{\"id\": {}, \"tool\": {}, \"arguments\": {}}}{comma}\n",
                line(&json!(write.id)),
                line(&json!(write.tool)),
                line(&write.arguments),
            ));
        }
        out.push_str("  ]\n}\n");
        out
    }

    /// Check that the connected server is still the one this plan was made
    /// for: the same identity, and for every tool the plan uses the same
    /// class, definition, and flags, and that `write_tools` allows the tool.
    ///
    /// # Errors
    /// Returns [`PlanFileError::ServerChanged`] with the first difference, or
    /// [`PlanFileError::Write`] for a write that `write_tools` does not allow.
    pub fn check_server(&self, facts: &ServerFacts) -> Result<(), PlanFileError> {
        let changed = |message: String| {
            Err(PlanFileError::ServerChanged(format!(
                "{message} Run `codemode plan` again."
            )))
        };
        if facts.identity != self.server.identity {
            return changed(format!(
                "This plan was made for {}, but the server file names {}.",
                crate::render::escape(&self.server.identity),
                crate::render::escape(&facts.identity)
            ));
        }
        self.check_write_tools(facts)?;
        for (name, planned) in &self.tools {
            let shown = crate::render::escape(name);
            let Some((tool, class)) = facts.tools.get(name) else {
                return changed(format!("The server no longer lists the tool {shown}."));
            };
            let current = PlannedTool::of(tool, *class);
            if current.class != planned.class {
                return changed(format!(
                    "The tool {shown} was {} when the plan was made and is {} now: the \
                     server's hints, or \"trust_annotations\" or \"read_tools\" in the server \
                     file, changed.",
                    class_words(planned.class),
                    class_words(current.class)
                ));
            }
            if current != *planned {
                return changed(format!(
                    "The server changed the definition of the tool {shown} (its arguments or \
                     its hints) since the plan was made."
                ));
            }
        }
        Ok(())
    }

    /// Check that `write_tools` allows the tool of every write.
    ///
    /// # Errors
    /// Returns [`PlanFileError::Write`] for the first write it does not allow.
    pub fn check_write_tools(&self, facts: &ServerFacts) -> Result<(), PlanFileError> {
        match self
            .writes
            .iter()
            .find(|write| !facts.allows_write(&write.tool))
        {
            Some(write) => Err(PlanFileError::Write {
                id: write.id.clone(),
                reason: format!(
                    "it names {:?}, which \"write_tools\" in the server file does not allow",
                    write.tool
                ),
            }),
            None => Ok(()),
        }
    }

    /// Groups of writes with the same tool and the same arguments, each with
    /// two or more identities, in plan order.
    #[must_use]
    pub fn identical_writes(&self) -> Vec<Vec<&str>> {
        let mut groups: Vec<Vec<&str>> = Vec::new();
        let mut seen: HashMap<(&str, String), usize> = HashMap::new();
        for write in &self.writes {
            let key = (write.tool.as_str(), canonical_json(write.arguments.clone()));
            match seen.get(&key) {
                Some(&index) => groups[index].push(&write.id),
                None => {
                    seen.insert(key, groups.len());
                    groups.push(vec![&write.id]);
                }
            }
        }
        groups.retain(|group| group.len() > 1);
        groups
    }
}

/// A tool class in the words a person reads.
#[must_use]
pub fn class_words(class: ToolClass) -> &'static str {
    match class {
        ToolClass::Read => "read",
        ToolClass::RepeatableWrite => "repeatable write",
        ToolClass::Write => "write",
    }
}

/// `value` as one line of JSON with sorted keys, a space after each `:` and
/// `,`, and every character that is unsafe to print written as a `\u` escape.
/// It parses back to exactly `value`.
#[must_use]
pub fn spaced_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, ": ", ", ");
    out
}

/// `value` as canonical JSON (sorted keys, no white space) with every
/// character that is unsafe to print written as a `\u` escape. It parses back
/// to exactly `value`.
#[must_use]
pub fn printable_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, ":", ",");
    out
}

fn write_json(out: &mut String, value: &Value, colon: &str, comma: &str) {
    match value {
        Value::String(text) => write_json_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(comma);
                }
                write_json(out, item, colon, comma);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(comma);
                }
                write_json_string(out, key);
                out.push_str(colon);
                write_json(out, item, colon, comma);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Write `text` as a JSON string. Unsafe characters become `\uXXXX` escapes
/// (a surrogate pair above U+FFFF), so the output is still JSON.
pub(crate) fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        push_json_char(out, character);
    }
    out.push('"');
}

/// Write one character of a JSON string.
pub(crate) fn push_json_char(out: &mut String, character: char) {
    match character {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        character if unsafe_to_print(character) => {
            for unit in character.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
        character => out.push(character),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, annotations: Value) -> Tool {
        Tool::parse(&json!({
            "name": name,
            "inputSchema": {"type": "object", "required": ["id"], "additionalProperties": false,
                            "properties": {"id": {"type": "string"}, "text": {"type": "string"}}},
            "annotations": annotations,
        }))
        .expect("tool")
    }

    fn facts() -> ServerFacts {
        let listed = [
            (
                tool("notes.list", json!({"readOnlyHint": true})),
                ToolClass::Read,
            ),
            (
                tool(
                    "notes.put",
                    json!({"idempotentHint": true, "destructiveHint": false}),
                ),
                ToolClass::RepeatableWrite,
            ),
            (
                tool(
                    "notes.delete",
                    json!({"idempotentHint": true, "destructiveHint": true}),
                ),
                ToolClass::RepeatableWrite,
            ),
            (tool("plain", Value::Null), ToolClass::Write),
        ];
        ServerFacts {
            identity: "https://mcp.example.com/mcp".into(),
            protocol: "2025-11-25".into(),
            trust_annotations: true,
            write_tools: None,
            tools: listed
                .into_iter()
                .map(|(tool, class)| (tool.name.clone(), (tool, class)))
                .collect(),
        }
    }

    fn config() -> Digest {
        Digest::of(b"server file")
    }

    fn change_list() -> Value {
        json!({"summary": "Tidy notes", "operations": [
            {"id": "put-b", "tool": "notes.put", "arguments": {"text": "two", "id": "b"}},
            {"id": "del-a", "tool": "notes.delete", "arguments": {"id": "a"}},
            {"id": "plain-1", "tool": "plain", "arguments": {"id": "x"}},
        ]})
    }

    fn plan() -> PlanFile {
        PlanFile::propose(
            &change_list(),
            &facts(),
            Some("server.json".into()),
            config(),
        )
        .expect("valid")
        .expect("has writes")
    }

    #[test]
    fn the_text_is_stable_and_reads_back_to_the_same_plan_and_digest() {
        let plan = plan();
        let text = plan.to_text();
        let definition = |name: &str| plan.tools[name].definition.to_string();
        let expected = format!(
            r#"{{
  "codemode_plan": 1,
  "summary": "Tidy notes",
  "server": {{
    "identity": "https://mcp.example.com/mcp",
    "file": "server.json",
    "config": "{}",
    "protocol": "2025-11-25",
    "trust_annotations": true
  }},
  "tools": {{
    "notes.delete": {{"class": "repeatable_write", "definition": "{}", "destructive": true, "annotated": true}},
    "notes.put": {{"class": "repeatable_write", "definition": "{}", "destructive": false, "annotated": true}},
    "plain": {{"class": "write", "definition": "{}", "destructive": false, "annotated": false}}
  }},
  "operations": [
    {{"id": "put-b", "tool": "notes.put", "arguments": {{"id": "b", "text": "two"}}}},
    {{"id": "del-a", "tool": "notes.delete", "arguments": {{"id": "a"}}}},
    {{"id": "plain-1", "tool": "plain", "arguments": {{"id": "x"}}}}
  ]
}}
"#,
            config(),
            definition("notes.delete"),
            definition("notes.put"),
            definition("plain"),
        );
        assert_eq!(text, expected);
        let reread = PlanFile::parse(&text).expect("reads back");
        assert_eq!(reread, plan);
        assert_eq!(
            reread.to_text(),
            text,
            "the same input gives the same bytes"
        );
        assert_eq!(reread.digest(), plan.digest());
    }

    #[test]
    fn the_digest_covers_everything_but_the_server_file_path() {
        let plan = plan();
        let digest = plan.digest();
        let mut moved = plan.clone();
        moved.server.file = Some("../elsewhere/server.json".into());
        assert_eq!(moved.digest(), digest, "the server file may move");

        let mut changes: Vec<(&str, PlanFile)> = Vec::new();
        let mut edited = plan.clone();
        edited.writes[0].arguments["text"] = json!("two!");
        changes.push(("an argument", edited));
        let mut edited = plan.clone();
        edited.writes.swap(0, 1);
        changes.push(("the order", edited));
        let mut edited = plan.clone();
        edited.summary = Some("Tidy notes.".into());
        changes.push(("the summary", edited));
        let mut edited = plan.clone();
        edited.server.identity = "https://mcp.example.com/other".into();
        changes.push(("the server", edited));
        let mut edited = plan.clone();
        edited.server.config = Digest::of(b"another server file");
        changes.push(("the server file", edited));
        let mut edited = plan.clone();
        edited.tools.get_mut("plain").expect("tool").class = ToolClass::RepeatableWrite;
        changes.push(("a tool class", edited));
        let mut edited = plan.clone();
        edited.tools.get_mut("plain").expect("tool").definition = Digest::of(b"other");
        changes.push(("a tool definition", edited));
        let mut edited = plan.clone();
        edited
            .tools
            .get_mut("notes.delete")
            .expect("tool")
            .destructive = false;
        changes.push(("the destructive flag", edited));
        for (what, edited) in changes {
            assert_ne!(edited.digest(), digest, "{what} changes the digest");
        }
    }

    #[test]
    fn a_change_list_outside_what_the_server_offers_is_refused() {
        let propose = |list: Value, facts: &ServerFacts| {
            PlanFile::propose(&list, facts, None, config())
                .expect_err("refused")
                .to_string()
        };
        let one = |tool: &str, arguments: Value| json!({"operations": [{"id": "a", "tool": tool, "arguments": arguments}]});
        let facts = facts();
        let cases = [
            (one("notes.list", json!({})), "a read tool"),
            (one("nope", json!({})), "does not list"),
            (one("notes.put", json!({"text": "t"})), "\"id\" is missing"),
            (
                one("notes.put", json!({"id": "a", "extra": 1})),
                "not in the tool's schema",
            ),
            (
                json!({"operations": [{"id": "a", "tool": "notes.put", "arguments": {"id": "a"}, "preconditions": [{}]}]}),
                "unknown key \"preconditions\"",
            ),
            (json!([]), "a change list is"),
            (
                json!({"operations": [], "extra": 1}),
                "unknown key \"extra\"",
            ),
            (
                json!({"operations": [{"id": 1, "tool": "notes.put"}]}),
                "needs a string id and tool",
            ),
            (
                json!({"operations": [
                    {"id": "a", "tool": "notes.put", "arguments": {"id": "a"}},
                    {"id": "a", "tool": "notes.put", "arguments": {"id": "b"}}]}),
                "appears twice",
            ),
            (
                one("notes.put", json!({"id": "a", "text": 1})),
                "must be of type string",
            ),
        ];
        for (list, expected) in cases {
            let error = propose(list, &facts);
            assert!(
                error.contains(expected),
                "{error} should mention {expected}"
            );
        }
        assert_eq!(
            PlanFile::propose(
                &json!({"summary": "nothing", "operations": []}),
                &facts,
                None,
                config()
            ),
            Ok(None),
            "no operations is not an error"
        );

        // The operator's allowlist decides which writes are possible.
        let narrow = ServerFacts {
            write_tools: Some(vec!["notes.put".into()]),
            ..facts.clone()
        };
        let error = propose(one("notes.delete", json!({"id": "a"})), &narrow);
        assert!(
            error.contains("\"write_tools\" in the server file does not allow"),
            "{error}"
        );
        PlanFile::propose(
            &one("notes.put", json!({"id": "a"})),
            &narrow,
            None,
            config(),
        )
        .expect("allowed");
        // A plan made before the allowlist is checked against it again.
        let error = plan()
            .check_server(&narrow)
            .expect_err("refused")
            .to_string();
        assert!(
            error.contains("write \"del-a\"") && error.contains("write_tools"),
            "{error}"
        );
    }

    #[test]
    fn a_server_that_changed_since_the_plan_was_made_is_refused() {
        let plan = plan();
        plan.check_server(&facts()).expect("unchanged");
        let refused =
            |facts: &ServerFacts| plan.check_server(facts).expect_err("refused").to_string();

        let mut other = facts();
        other.identity = "https://mcp.example.com/other".into();
        assert!(refused(&other).contains("was made for https://mcp.example.com/mcp"));

        let mut reclassified = facts();
        reclassified.tools.get_mut("plain").expect("tool").1 = ToolClass::RepeatableWrite;
        let error = refused(&reclassified);
        assert!(
            error.contains("was write when the plan was made and is repeatable write now"),
            "{error}"
        );

        let mut redefined = facts();
        redefined.tools.insert(
            "notes.put".into(),
            (
                tool(
                    "notes.put",
                    json!({"idempotentHint": true, "destructiveHint": true}),
                ),
                ToolClass::RepeatableWrite,
            ),
        );
        let error = refused(&redefined);
        assert!(
            error.contains("changed the definition of the tool notes.put"),
            "{error}"
        );
        assert!(error.ends_with("Run `codemode plan` again."), "{error}");

        let mut removed = facts();
        removed.tools.remove("notes.delete");
        assert!(refused(&removed).contains("no longer lists the tool notes.delete"));
    }

    #[test]
    fn an_inconsistent_plan_file_is_refused() {
        let text = plan().to_text();
        let refused = |text: &str| PlanFile::parse(text).expect_err("refused").to_string();
        assert!(
            refused(&text.replace("\"codemode_plan\": 1", "\"codemode_plan\": 2"))
                .contains("version 2")
        );
        assert!(
            refused(&text.replace("\"tool\": \"plain\"", "\"tool\": \"other\""))
                .contains("does not describe its tool \"other\"")
        );
        let unused = text.replace(
            "    {\"id\": \"del-a\", \"tool\": \"notes.delete\", \"arguments\": {\"id\": \"a\"}},\n",
            "",
        );
        assert!(refused(&unused).contains("which no write uses"));
        assert!(
            refused(&text.replace("\"class\": \"write\"", "\"class\": \"read\""))
                .contains("a read tool")
        );
        assert!(refused(&text.replace("\"summary\"", "\"surprise\"")).contains("not a plan file"));
        assert!(
            refused(&text.replace("{\"id\": \"x\"}", "{\"api_key\": \"x\"}"))
                .contains("looks like a credential")
        );
        assert!(
            refused(&text.replace("{\"id\": \"x\"}", "{\"n\": 9007199254740993}"))
                .contains("outside -(2^53-1)")
        );
        assert!(refused("{}").contains("not a plan file"));
        let unbound = text.replace(&format!("    \"config\": \"{}\",\n", config()), "");
        assert!(
            refused(&unbound).contains("missing field `config`"),
            "a plan names its server file"
        );
    }

    #[test]
    fn text_a_viewer_could_act_on_is_escaped_in_the_file_and_reads_back_exactly() {
        let hostile = "a\u{1b}[2Jb\u{202e}c\u{200b}d\u{7f}e\u{85}f\u{1f600}g\"h\\i\nj";
        let list = json!({"summary": hostile, "operations": [
            {"id": "a", "tool": "notes.put", "arguments": {"id": hostile, "text": "é ü 北"}}]});
        let plan = PlanFile::propose(&list, &facts(), None, config())
            .expect("valid")
            .expect("writes");
        let text = plan.to_text();
        assert!(
            text.chars().all(|c| c == '\n' || !unsafe_to_print(c)),
            "{text:?}"
        );
        assert!(
            text.contains(r"a\u001b[2Jb\u202ec\u200bd\u007fe\u0085f"),
            "{text}"
        );
        assert!(text.contains("é ü 北"), "ordinary text stays readable");
        let reread = PlanFile::parse(&text).expect("reads back");
        assert_eq!(reread.summary.as_deref(), Some(hostile));
        assert_eq!(reread.writes[0].arguments["id"], hostile);
        assert_eq!(reread.digest(), plan.digest());
    }

    #[test]
    fn a_text_that_escaping_makes_too_large_is_refused() {
        // 600 writes of 60,000 DEL characters: 36 MB as given, and six bytes
        // for each character once escaped.
        let delete = char::from_u32(0x7f).expect("a character").to_string();
        let operations: Vec<Value> = (0..110)
            .map(|index| {
                json!({"id": format!("w{index}"), "tool": "notes.put",
                       "arguments": {"id": "a", "text": delete.repeat(60_000)}})
            })
            .collect();
        let plan = PlanFile::propose(
            &json!({ "operations": operations }),
            &facts(),
            None,
            config(),
        )
        .expect("the writes are a valid plan")
        .expect("writes");
        assert!(plan.to_text().len() > MAX_PLAN_BYTES);
        assert_eq!(plan.checked_text(), Err(PlanError::TooLarge.into()));
        let small = self::plan();
        assert_eq!(small.checked_text(), Ok(small.to_text()));
    }

    #[test]
    fn identical_writes_are_found() {
        let list = json!({"operations": [
            {"id": "a", "tool": "notes.put", "arguments": {"id": "x", "text": "t"}},
            {"id": "b", "tool": "notes.delete", "arguments": {"id": "x"}},
            {"id": "c", "tool": "notes.put", "arguments": {"text": "t", "id": "x"}},
            {"id": "d", "tool": "notes.put", "arguments": {"id": "y", "text": "t"}},
        ]});
        let plan = PlanFile::propose(&list, &facts(), None, config())
            .expect("valid")
            .expect("writes");
        assert_eq!(plan.identical_writes(), vec![vec!["a", "c"]]);
    }
}
