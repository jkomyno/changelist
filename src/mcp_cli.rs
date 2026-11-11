//! `codemode mcp`: use any MCP server as the provider. Process I/O only;
//! behavior lives in the library.

use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use codemode::mcp::{MAX_SERVER_CONFIG_BYTES, McpProvider, ServerConfig, ToolClass};
use codemode::{
    Durability, Journal, MAX_PLAN_BYTES, Operation, PLAN_FORMAT_VERSION, Plan, RecoverableProvider,
    RunCancellation, RunConfig, SessionError, SessionKind, SessionOptions, run, run_session,
};
use serde_json::{Value, json};

use crate::plan_cli::{Flags, print_json};
use crate::{CliError, cancel_on_interrupt, read_input};

const MAX_PROGRAM_BYTES: u64 = 256 * 1024;

pub(crate) const HELP: &str = "codemode mcp — use any Model Context Protocol server as the provider

Usage:
  codemode mcp tools       --server <server.json> [--json]
  codemode mcp investigate --server <server.json> --program <file> --out <plan.json>
                           [--input <file.json>] [--trace <file>] [limits]
  codemode mcp declare     --server <server.json>
  codemode mcp run         --server <server.json> --journal <file> [--deadline-ms <n>]
  codemode mcp resume      --server <server.json> --journal <file> [--deadline-ms <n>]

The server file names one server and what you trust about it:
  {\"url\": \"https://host/mcp\", \"headers\": {\"x-api-key\": \"${API_KEY}\"},
   \"trust_annotations\": true, \"read_tools\": [\"SOME_TOOL\"]}
  {\"command\": \"my-server\", \"args\": [\"--flag\"], \"env\": {\"TOKEN\": \"${TOKEN}\"}}
${NAME} in a header or env value is read from this process's environment, so
the file holds no credential. A url must be https, or http on a loopback host.
The client speaks MCP 2026-07-28 and falls back to the initialize handshake of
2025-11-25, 2025-06-18, and 2025-03-26. It answers no elicitation, sampling, or
roots request.

Every tool has one class:
  read              a program may call it during investigate; never in a plan
  repeatable_write  may be in a plan; sent again after a lost reply
  write             may be in a plan; after a lost reply a person decides
A tool is a read only if read_tools names it, or trust_annotations is true and
the server marks it readOnlyHint. It is a repeatable_write only if
trust_annotations is true and the server marks it idempotentHint. Otherwise it
is a write. Annotations are hints from the server: set trust_annotations only
for a server you trust to describe its tools correctly.

tools lists every tool with its class and the digest of its definition.

investigate runs a program once in a V8 worker. The program calls read tools
with `await tools.call(name, arguments)` and returns
{summary?, operations: [{id, tool, arguments}]}. Each operation must name a
write tool, and its arguments are checked against the top level of the tool's
schema. The result is written as a plan for `codemode plan check`. --input
makes a JSON value available to the program as `input`. The program cannot
write, import, or reach anything except the read tools.
  --timeout-ms <n>        Deadline for the program, startup included (default 60000)
  --max-calls <n>         Tool calls the program may make (default 32)
  --concurrency <n>       Tool calls in flight at once (default 4)
  --max-output-bytes <n>  Tool results plus the returned value (default 1048576)
  --heap-mb <n>           V8 heap budget in MiB (default 256)

declare prints the provider declaration `codemode plan check` and
`codemode plan approve` take with --provider: the server's identity, each write
tool's recovery contract, and the digest of each write tool's name, argument
schema, and annotations.

run executes an approved plan once; resume continues it after an interruption.
Both connect to the server again and refuse the session when its identity, a
contract, or a tool definition differs from what was approved. MCP has no way
to ask whether an earlier call took effect, so resume sends a repeatable_write
again and leaves every other uncertain operation for `codemode plan review`.
A tool error (isError) does not say whether part of the work was done, so it
also leaves the operation uncertain. Exit statuses are those of
`codemode plan run`.
";

fn server(flags: &Flags) -> Result<ServerConfig, Box<dyn Error>> {
    let path = PathBuf::from(flags.require("--server")?);
    let text = read_input(&path, MAX_SERVER_CONFIG_BYTES as u64, "server file")?;
    Ok(ServerConfig::parse(&text, |name| std::env::var(name).ok())?)
}

async fn connect(flags: &Flags) -> Result<McpProvider, Box<dyn Error>> {
    let provider = McpProvider::connect(&server(flags)?).await?;
    for warning in provider.warnings() {
        eprintln!("codemode: left out a tool: {warning}");
    }
    Ok(provider)
}

pub(crate) async fn execute(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let mut args = args.into_iter();
    let command = args.next().and_then(|arg| arg.into_string().ok());
    let rest: Vec<OsString> = args.collect();
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        io::stdout().write_all(HELP.as_bytes())?;
        return Ok(0);
    }
    match command.as_deref() {
        None | Some("-h" | "--help") => {
            io::stdout().write_all(HELP.as_bytes())?;
            Ok(0)
        }
        Some("tools") => tools(Flags::parse(rest, &["--server", "--json"])?).await,
        Some("declare") => {
            let provider = connect(&Flags::parse(rest, &["--server"])?).await?;
            let declaration = provider.declaration();
            provider.close().await;
            print_json(&declaration)?;
            Ok(0)
        }
        Some("investigate") => {
            investigate(Flags::parse(
                rest,
                &[
                    "--server",
                    "--program",
                    "--out",
                    "--input",
                    "--trace",
                    "--timeout-ms",
                    "--max-calls",
                    "--concurrency",
                    "--max-output-bytes",
                    "--heap-mb",
                ],
            )?)
            .await
        }
        Some(kind @ ("run" | "resume")) => {
            let flags = Flags::parse(rest, &["--server", "--journal", "--deadline-ms"])?;
            let kind = if kind == "run" {
                SessionKind::Run
            } else {
                SessionKind::Resume
            };
            session(flags, kind).await
        }
        Some(other) => Err(CliError(format!(
            "unknown mcp command '{other}'; use codemode mcp --help"
        ))
        .into()),
    }
}

async fn tools(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let provider = connect(&flags).await?;
    provider.close().await;
    if flags.has("--json") {
        let tools: Vec<Value> = provider
            .tools()
            .map(|(tool, class)| {
                json!({
                    "name": tool.name,
                    "class": class,
                    "definition": tool.definition_digest(),
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                    "outputSchema": tool.output_schema,
                    "annotations": tool.annotations,
                })
            })
            .collect();
        print_json(&json!({
            "server": provider.identity(),
            "protocol_version": provider.protocol_version(),
            "tools": tools,
        }))?;
        return Ok(0);
    }
    let mut out = format!(
        "server {:?}, MCP {}\n",
        provider.identity(),
        provider.protocol_version()
    );
    for (tool, class) in provider.tools() {
        let class = match class {
            ToolClass::Read => "read",
            ToolClass::RepeatableWrite => "repeatable_write",
            ToolClass::Write => "write",
        };
        out.push_str(&format!(
            "  {class:<16} {:?}  {}\n",
            tool.name,
            tool.definition_digest().short()
        ));
    }
    io::stdout().write_all(out.as_bytes())?;
    Ok(0)
}

async fn investigate(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let program = read_input(
        &PathBuf::from(flags.require("--program")?),
        MAX_PROGRAM_BYTES,
        "program",
    )?;
    let out = PathBuf::from(flags.require("--out")?);
    // On the first line, so the program's own line numbers hold.
    let input = match flags.get("--input") {
        Some(path) => {
            let text = read_input(&PathBuf::from(path), MAX_PLAN_BYTES as u64, "input")?;
            codemode::parse_json(&text)
                .map_err(|error| CliError(format!("--input is not JSON: {error}")))?
        }
        None => Value::Null,
    };
    // Parsed at run time, so a key such as "__proto__" stays plain data.
    let literal = serde_json::to_string(&input.to_string())?;
    let code = format!("const input = JSON.parse({literal}); {program}");
    let provider = connect(&flags).await?;
    let defaults = RunConfig::default();
    let config = RunConfig {
        timeout_ms: flags.number("--timeout-ms", 60_000)?,
        max_calls: flags.number("--max-calls", defaults.max_calls)?,
        max_concurrency: flags.number("--concurrency", defaults.max_concurrency)?,
        max_output_bytes: flags.number("--max-output-bytes", 1024 * 1024)?,
        heap_mb: flags.number("--heap-mb", defaults.heap_mb)?,
        worker: Some(std::env::current_exe().map_err(|error| {
            CliError(format!(
                "cannot locate this executable for the worker: {error}"
            ))
        })?),
        allowed_tools: provider.read_tools(),
        process_memory_mb: None,
    };
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let report = run(&code, config, provider.clone(), cancellation).await;
    signal_task.abort();
    provider.close().await;
    let report = report?;
    if let Some(path) = flags.get("--trace") {
        write_file(Path::new(path), &serde_json::to_string(&report)?)?;
    }
    if !report.status.is_success() {
        eprintln!(
            "codemode: the program ended with status {}: {}",
            report.status.as_str(),
            report.diagnostic.as_deref().unwrap_or("no diagnostic")
        );
        return Ok(report.status.exit_code().max(1));
    }
    let plan = proposal(report.output.as_ref(), &provider)?;
    let frozen = plan.freeze()?;
    write_file(&out, &serde_json::to_string_pretty(frozen.plan())?)?;
    print_json(&json!({
        "plan": out.display().to_string(),
        "digest": frozen.digest(),
        "operations": frozen.plan().operations.len(),
        "calls": report.calls.len(),
    }))?;
    Ok(0)
}

/// Turn what the program returned into a plan whose every operation is a write
/// tool of this server with arguments its schema accepts.
fn proposal(output: Option<&Value>, provider: &McpProvider) -> Result<Plan, CliError> {
    let shape = "the program must return {summary?, operations: [{id, tool, arguments}]}";
    let object = output
        .and_then(Value::as_object)
        .ok_or_else(|| CliError(shape.into()))?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "summary" | "operations"))
    {
        return Err(CliError(format!("unknown key {key:?}: {shape}")));
    }
    let summary = match object.get("summary") {
        None | Some(Value::Null) => None,
        Some(Value::String(summary)) => Some(summary.clone()),
        Some(_) => return Err(CliError(format!("summary must be a string: {shape}"))),
    };
    let proposed = object
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError(shape.into()))?;
    let mut operations = Vec::new();
    for (index, entry) in proposed.iter().enumerate() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str);
        let (Some(id), Some(tool)) = (field("id"), field("tool")) else {
            return Err(CliError(format!(
                "operation {index} needs a string id and tool"
            )));
        };
        if let Some(key) = entry.as_object().and_then(|entry| {
            entry
                .keys()
                .find(|key| !matches!(key.as_str(), "id" | "tool" | "arguments"))
        }) {
            return Err(CliError(format!(
                "operation {id:?} has unknown key {key:?}; an MCP server cannot check \
                 preconditions or restrict a destination"
            )));
        }
        let arguments = entry.get("arguments").cloned().unwrap_or_else(|| json!({}));
        match provider.tool(tool) {
            None => {
                return Err(CliError(format!(
                    "operation {id:?} names {tool:?}, which the server does not list"
                )));
            }
            Some((_, ToolClass::Read)) => {
                return Err(CliError(format!(
                    "operation {id:?} names {tool:?}, a read tool; a plan holds writes only"
                )));
            }
            Some((definition, _)) => definition
                .check_arguments(&arguments)
                .map_err(|reason| CliError(format!("operation {id:?}: {reason}")))?,
        }
        operations.push(Operation {
            id: id.to_string(),
            tool: tool.to_string(),
            scope: provider.scope(),
            arguments,
            preconditions: Vec::new(),
        });
    }
    Ok(Plan {
        version: PLAN_FORMAT_VERSION,
        summary,
        operations,
    })
}

/// Write `text` to `path` through a temporary file in the same directory, so a
/// reader never sees a partial file.
fn write_file(path: &Path, text: &str) -> Result<(), Box<dyn Error>> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, format!("{text}\n"))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

async fn session(flags: Flags, kind: SessionKind) -> Result<u8, Box<dyn Error>> {
    let defaults = SessionOptions::default();
    let default_ms = u64::try_from(defaults.deadline.as_millis()).unwrap_or(u64::MAX);
    let deadline_ms = flags.number("--deadline-ms", default_ms)?;
    if deadline_ms == 0 {
        return Err(CliError("--deadline-ms must be positive".into()).into());
    }
    let options = SessionOptions {
        kind,
        deadline: Duration::from_millis(deadline_ms),
    };
    // Take the journal first: a second executor must fail before it reaches the server.
    let journal = Journal::open(flags.require("--journal")?, Durability::Storage)?;
    let provider = connect(&flags).await?;
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let result = run_session(journal, provider.clone(), options, cancellation).await;
    signal_task.abort();
    provider.close().await;
    match result {
        Ok(outcome) => {
            print_json(&outcome)?;
            Ok(outcome.exit_code())
        }
        Err(SessionError::Refused { session, reason }) => {
            print_json(&json!({"session": session, "status": "refused", "detail": reason}))?;
            eprintln!("codemode: session {session} refused: {reason}");
            Ok(4)
        }
        Err(error) => Err(error.into()),
    }
}
