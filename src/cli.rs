//! The five commands. Process I/O only: each one reads its arguments and
//! files, calls the library, prints, and returns an exit code.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use codemode::mcp::{MAX_SERVER_CONFIG_BYTES, McpProvider, ServerConfig};
use codemode::plan_file::{PlanFile, ServerFacts};
use codemode::render::{self, ReviewOptions, escape};
use codemode::status::{
    PlanState, ServerFileState, StatusReport, WriteState, WriteStatus, approved_digest, plural,
};
use codemode::{
    Approval, Attempt, AttemptError, CallContext, Digest, Durability, FrozenPlan, History, Journal,
    JournalError, JournalReader, MAX_PLAN_BYTES, Observed, Operation, ProbeContext,
    ProviderDeclaration, ProviderError, Reconciliation, RecoverableProvider, RecoveryPolicy,
    Resolution, RunCancellation, RunConfig, Scope, SessionError, SessionKind, SessionOptions,
    approval_binding, run, run_session,
};
use serde_json::{Value, json};

use crate::CliError;
use crate::approval::{self, NOT_YES};

const MAX_PROGRAM_BYTES: u64 = 256 * 1024;
/// What a program may read and return in all: the largest plan, plus 1 MiB
/// for the results of its tool calls.
const PROGRAM_OUTPUT_BYTES: usize = MAX_PLAN_BYTES + 1024 * 1024;

pub(crate) const TOOLS_HELP: &str = "codemode tools — list the server's tools

Usage:
  codemode tools --server <server.json> [--json]

The server file names one MCP server and what you trust about it:
  {\"url\": \"https://host/mcp\", \"headers\": {\"x-api-key\": \"${API_KEY}\"},
   \"trust_annotations\": true, \"read_tools\": [], \"write_tools\": [\"SOME_TOOL\"]}
  {\"command\": \"my-server\", \"args\": [\"--flag\"], \"env\": {\"TOKEN\": \"${TOKEN}\"}}
${NAME} is read from the environment, so the file holds no credential.

Every tool has one class:
  read              A program may call it. A plan cannot hold it.
  repeatable write  A plan may hold it. After a lost reply it is sent again.
  write             A plan may hold it. After a lost reply a person decides.
A tool is a read when read_tools names it, or when trust_annotations is true
and the server marks it read-only. It is a repeatable write when
trust_annotations is true and the server marks it idempotent. Every other tool
is a write. With write_tools, a plan may hold only the tools it names.

Example:
  codemode tools --server server.json
";

pub(crate) const PLAN_HELP: &str = "codemode plan — make a plan file and print its review

Usage:
  codemode plan --server <server.json> --program <file.ts> [--input <file>] [--out <plan file>]
  codemode plan --server <server.json> --operations <file|-> [--out <plan file>]

The writes come from one of two sources:
  --program <file.ts>     TypeScript that runs once in a sandbox. It calls read
                          tools with `await tools.call(name, arguments)` and
                          returns {summary?, operations: [{id, tool, arguments}]}.
                          `input` holds the JSON value of --input, or null.
  --operations <file|->   The same JSON object, from a file or from stdin.

Every operation must name a write tool the server lists. Its arguments are
checked against the top level of the tool's schema. Nothing is sent.

The plan goes to <program or operations file name>.plan.json in the current
directory, or to codemode.plan.json for stdin, unless --out names a file.
A plan that was approved is never overwritten. No operations prints
\"No changes.\" and writes no file.

Options:
  --full                  Print every write with every argument in full
  --json                  Print one JSON document instead of the review
For --program:
  --trace <file>          Save the program's execution record
  --timeout-ms <n>        Deadline for the program (default 60000)
  --max-calls <n>         Tool calls the program may make (default 32)
  --concurrency <n>       Tool calls in flight at once (default 4)
  --max-output-bytes <n>  Tool results plus the returned value (default 34603008)
  --heap-mb <n>           V8 heap budget in MiB (default 256)

Example:
  codemode plan --server server.json --program triage.ts
";

pub(crate) const STATUS_HELP: &str = "codemode status — show where a plan stands

Usage:
  codemode status <plan file> [--full] [--json]

Before approval it prints the review. After approval it prints what happened to
each write and the exact command to run next. It reads the plan file, its
journal, and the server file only: no network, no change. It works while
`codemode apply` runs. The `config` line says whether the server file still is
the one the plan was made with.

Options:
  --full            Print every write and every identity, nothing shortened
  --json            Print one JSON document: digest, server, approval, counts,
                    and for each write its state, attempts, last error, and
                    next step

Example:
  codemode status triage.plan.json
";

pub(crate) const APPLY_HELP: &str = "codemode apply — approve a plan and send its writes

Usage:
  codemode apply <plan file> [--approve <digest>] [options]

The first run reads the server file, prints the review, and needs an approval:
  When stdin, stdout, and stderr are terminals it asks. Only the exact answer
  \"yes\" approves.
  Otherwise it asks nobody. Pass --approve sha256:<the full digest>.
Only then it connects to the server. It sends the first write alone, and the
others once it has an answer. A later run continues the same plan without a
question.

It refuses an edited plan file, a server file that is not the one the plan was
made with, and a server whose identity or tools changed since `codemode plan`.

The journal is the plan file's path with .json replaced by .journal.db. It is
the only memory of what was sent. Keep it next to the plan file.

Options:
  --approve <digest>    Approve the plan with this full digest; ask no question
  --approver <name>     Recorded with the approval, not authenticated
                        (default $USER)
  --server <file>       Use this server file, not the path the plan records.
                        It must be the file the plan was made with.
  --full, --json        As for `codemode status`
First run only; the approval binds them:
  --concurrency <n>     Writes in flight at once (default 4)
  --max-attempts <n>    Times one write may be sent, the first included (default 3)
Every run:
  --deadline-ms <n>     Deadline for this run (default 300000)
  --max-failures <n>    Stop sending after this many writes did not end
                        applied; 0 never stops (default 1)

Example:
  codemode apply triage.plan.json
";

pub(crate) const RESOLVE_HELP: &str = "codemode resolve — record what happened to a write

Usage:
  codemode resolve <plan file> (<id>... | --errors | --no-reply) --as applied|not-applied

When a write fails or gets no answer, codemode cannot tell whether it changed
anything. Check the target yourself, then record what you found:
  --as applied      The change is there. The write is done.
  --as not-applied  The change is not there. `codemode apply` sends it again.

Select writes by identity, or:
  --errors          Every write whose last attempt got an error answer
  --no-reply        Every write whose last attempt got no answer
It prints what it selected, with the server's error text, and records it.

Options:
  --reviewer <name>  Recorded with the finding, not authenticated (default $USER)

Example:
  codemode resolve triage.plan.json comment-101 --as not-applied
";

const BOOLEAN_FLAGS: &[&str] = &["--json", "--full", "--errors", "--no-reply"];

struct Flags {
    positional: Vec<String>,
    values: HashMap<String, String>,
}

impl Flags {
    fn parse(args: Vec<OsString>, allowed: &[&str]) -> Result<Self, CliError> {
        let mut positional = Vec::new();
        let mut values = HashMap::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg
                .into_string()
                .map_err(|_| CliError("arguments must be valid UTF-8".into()))?;
            if !arg.starts_with("--") {
                positional.push(arg);
                continue;
            }
            if !allowed.contains(&arg.as_str()) {
                return Err(CliError(format!("unknown option '{arg}'")));
            }
            let value = if BOOLEAN_FLAGS.contains(&arg.as_str()) {
                String::new()
            } else {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or_else(|| CliError(format!("missing value for {arg}")))?
            };
            if values.insert(arg.clone(), value).is_some() {
                return Err(CliError(format!("{arg} can only be supplied once")));
            }
        }
        Ok(Self { positional, values })
    }

    fn get(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }

    fn has(&self, flag: &str) -> bool {
        self.values.contains_key(flag)
    }

    fn require(&self, flag: &str) -> Result<&str, CliError> {
        self.get(flag)
            .ok_or_else(|| CliError(format!("{flag} is required")))
    }

    fn number<T: std::str::FromStr>(&self, flag: &str, default: T) -> Result<T, CliError> {
        match self.get(flag) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| CliError(format!("{flag} requires a nonnegative integer"))),
        }
    }
}

/// Read a file or stdin (`-`), refusing more than `limit` bytes.
fn read_input(path: &Path, limit: u64, what: &str) -> Result<String, Box<dyn Error>> {
    let input: Box<dyn Read> =
        if path.as_os_str() == "-" {
            Box::new(io::stdin())
        } else {
            Box::new(File::open(path).map_err(|error| {
                CliError(format!("cannot read {what} {}: {error}", path.display()))
            })?)
        };
    let mut text = String::new();
    input
        .take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|error| CliError(format!("cannot read {what} {}: {error}", path.display())))?;
    if text.len() as u64 > limit {
        return Err(CliError(format!("{what} exceeds the {} KiB limit", limit / 1024)).into());
    }
    Ok(text)
}

/// Write `text` to `path` through a temporary file in the same directory, so a
/// reader never sees a partial file.
fn write_file(path: &Path, text: &str) -> Result<(), Box<dyn Error>> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, text)
        .map_err(|error| CliError(format!("cannot write {}: {error}", path.display())))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

/// Print one JSON document on one line. A character a terminal could act on
/// is written as a JSON escape, so the document is safe to look at.
fn print_json(value: &impl serde::Serialize) -> Result<(), Box<dyn Error>> {
    let text = render::escape_json(&serde_json::to_string(value)?);
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}")?;
    Ok(())
}

fn print(text: &str) -> Result<(), Box<dyn Error>> {
    io::stdout().lock().write_all(text.as_bytes())?;
    Ok(())
}

/// Cancel on Ctrl-C and, on Unix, on SIGTERM, so a session that is signalled
/// still records how it ended.
fn cancel_on_interrupt(cancellation: &RunCancellation) -> tokio::task::JoinHandle<()> {
    let signal = cancellation.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! {
            result = tokio::signal::ctrl_c() => if result.is_err() { return; },
            () = terminate => {}
        }
        signal.cancel();
    })
}

/// A server file as a command read it, once: the digest of its bytes and
/// what they say.
struct ServerFile {
    digest: Digest,
    config: ServerConfig,
}

fn server_file(path: &Path) -> Result<ServerFile, Box<dyn Error>> {
    let text = read_input(path, MAX_SERVER_CONFIG_BYTES as u64, "server file")?;
    Ok(ServerFile {
        digest: Digest::of(text.as_bytes()),
        config: ServerConfig::parse(&text, |name| std::env::var(name).ok())?,
    })
}

/// The path of the server file a plan records, from where the command runs.
fn recorded_server_path(plan_path: &Path, recorded: &str) -> PathBuf {
    match directory(plan_path) {
        here if here == Path::new(".") => PathBuf::from(recorded),
        directory => directory.join(recorded),
    }
}

/// Look at the server file a plan records, without a network and without
/// the environment: whether it still is the file the plan was made with,
/// and the whole command of a stdio server whose identity was shortened.
fn look_at_server_file(report: &mut StatusReport, plan_path: &Path, plan: &PlanFile) {
    let Some(recorded) = &plan.server.file else {
        return;
    };
    let path = recorded_server_path(plan_path, recorded);
    report.server.file = Some(path.display().to_string());
    let Ok(text) = read_input(&path, MAX_SERVER_CONFIG_BYTES as u64, "server file") else {
        report.server.file_state = ServerFileState::Unreadable;
        return;
    };
    if Digest::of(text.as_bytes()) != plan.server.config {
        report.server.file_state = ServerFileState::Changed;
        return;
    }
    report.server.file_state = ServerFileState::Same;
    // Only the command and its arguments are wanted, so a variable that is
    // not set here does not matter.
    report.server.command = ServerConfig::parse(&text, |_| Some(String::new()))
        .ok()
        .and_then(|config| config.full_command());
}

async fn connect(config: &ServerConfig) -> Result<McpProvider, Box<dyn Error>> {
    let provider = McpProvider::connect(config).await?;
    for warning in provider.warnings() {
        eprintln!("codemode: left out a tool: {}", escape(warning));
    }
    Ok(provider)
}

/// The journal of a plan file: the plan's path with `.json` replaced by
/// `.journal.db`. It is always next to the plan, so that no command can be
/// pointed at another journal, or at none, for a plan that was applied.
fn journal_path(plan: &Path) -> PathBuf {
    let text = plan.as_os_str().to_string_lossy();
    let stem = text.strip_suffix(".json").unwrap_or(&text);
    PathBuf::from(format!("{stem}.journal.db"))
}

/// `target` relative to the directory `base`, when both can be resolved and
/// share a root; otherwise the absolute path of `target`.
fn relative_to(base: &Path, target: &Path) -> PathBuf {
    let (Ok(base), Ok(target)) = (base.canonicalize(), target.canonicalize()) else {
        return std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
    };
    let base: Vec<Component<'_>> = base.components().collect();
    let parts: Vec<Component<'_>> = target.components().collect();
    let shared = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    if shared == 0 {
        return target;
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for part in &parts[shared..] {
        relative.push(part);
    }
    relative
}

/// The directory that holds `path`.
fn directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

struct LoadedPlan {
    path: PathBuf,
    /// The path as the person named it, for messages and next commands.
    label: String,
    plan: PlanFile,
    frozen: FrozenPlan,
}

fn load_plan(flags: &Flags) -> Result<LoadedPlan, Box<dyn Error>> {
    let label = flags
        .positional
        .first()
        .ok_or_else(|| CliError("a plan file is required".into()))?
        .clone();
    let path = PathBuf::from(&label);
    let text = read_input(&path, MAX_PLAN_BYTES as u64, "plan file")?;
    let plan =
        PlanFile::parse(&text).map_err(|error| CliError(format!("{}: {error}", escape(&label))))?;
    let frozen = plan.frozen()?;
    Ok(LoadedPlan {
        path,
        label,
        plan,
        frozen,
    })
}

/// The history a journal holds, read without writing; `None` when there is no
/// journal yet.
fn history_of(journal: &Path) -> Result<Option<History>, Box<dyn Error>> {
    if !journal.exists() {
        return Ok(None);
    }
    // An `apply` in another terminal creates the file a moment before it
    // commits the journal's first content. A reader that arrives in between
    // finds a database without a journal in it, so it looks again.
    let mut tries = 0;
    loop {
        match JournalReader::open(journal) {
            Ok(reader) => return Ok(Some(reader.history()?)),
            Err(JournalError::Incompatible { .. }) if tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn default_policy() -> RecoveryPolicy {
    RecoveryPolicy::default()
}

/// The options a review or a status is printed with: what the approval bound,
/// or `policy` before there is one.
fn review_options(report: &StatusReport, policy: RecoveryPolicy, full: bool) -> ReviewOptions {
    let (concurrency, max_attempts) = report
        .approval
        .as_ref()
        .map_or((policy.concurrency, policy.max_attempts), |approval| {
            (approval.concurrency, approval.max_attempts)
        });
    ReviewOptions {
        full,
        concurrency,
        max_attempts,
    }
}

fn user(flag: Option<&str>) -> String {
    flag.map(String::from)
        .or_else(|| std::env::var("USER").ok().filter(|name| !name.is_empty()))
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) async fn tools(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let flags = Flags::parse(args, &["--server", "--json"])?;
    let config = server_file(Path::new(flags.require("--server")?))?.config;
    let provider = connect(&config).await?;
    provider.close().await;
    let facts = ServerFacts::of(&provider, &config);
    if !flags.has("--json") {
        return print(&render::tools(&facts)).map(|()| 0);
    }
    let tools: Vec<Value> = provider
        .tools()
        .map(|(tool, class)| {
            let write = class != codemode::mcp::ToolClass::Read;
            json!({
                "name": tool.name,
                "class": class,
                "destructive": write && tool.annotations.destructive_hint == Some(true),
                "plannable": write && facts.allows_write(&tool.name),
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
        "trust_annotations": config.trust_annotations,
        "write_tools": config.write_tools,
        "tools": tools,
    }))?;
    Ok(0)
}

const PROGRAM_FLAGS: &[&str] = &[
    "--input",
    "--trace",
    "--timeout-ms",
    "--max-calls",
    "--concurrency",
    "--max-output-bytes",
    "--heap-mb",
];

pub(crate) async fn plan(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let mut allowed = vec![
        "--server",
        "--program",
        "--operations",
        "--out",
        "--full",
        "--json",
    ];
    allowed.extend_from_slice(PROGRAM_FLAGS);
    let flags = Flags::parse(args, &allowed)?;
    if let Some(extra) = flags.positional.first() {
        return Err(CliError(format!("unexpected argument '{extra}'")).into());
    }
    let server_path = PathBuf::from(flags.require("--server")?);
    let source = match (flags.get("--program"), flags.get("--operations")) {
        (Some(program), None) => PathBuf::from(program),
        (None, Some(operations)) => {
            if let Some(flag) = PROGRAM_FLAGS.iter().find(|flag| flags.has(flag)) {
                return Err(CliError(format!("{flag} belongs to --program")).into());
            }
            PathBuf::from(operations)
        }
        _ => {
            return Err(CliError("give exactly one of --program and --operations".into()).into());
        }
    };
    let out = match flags.get("--out") {
        Some(out) => PathBuf::from(out),
        None => match source.file_stem().filter(|_| source.as_os_str() != "-") {
            Some(stem) => PathBuf::from(format!("{}.plan.json", stem.to_string_lossy())),
            None => PathBuf::from("codemode.plan.json"),
        },
    };
    let label = out.display().to_string();
    let journal = journal_path(&out);
    if journal.exists() {
        return Err(CliError(format!(
            "{} was approved or applied: its journal {} exists. A plan that was approved is \
             never overwritten; choose another file with --out",
            escape(&label),
            escape(&journal.display().to_string())
        ))
        .into());
    }
    let server = server_file(&server_path)?;
    let config = &server.config;
    // Read every input before the server is reached.
    let program = match flags.get("--program") {
        Some(_) => {
            let program = read_input(&source, MAX_PROGRAM_BYTES, "program")?;
            let input = match flags.get("--input") {
                Some(path) => {
                    let text = read_input(Path::new(path), MAX_PLAN_BYTES as u64, "input")?;
                    codemode::parse_json(&text)
                        .map_err(|error| CliError(format!("--input is not JSON: {error}")))?
                }
                None => Value::Null,
            };
            // Parsed at run time, so a key such as "__proto__" stays plain data.
            // On the first line, so the program's own line numbers hold.
            let literal = serde_json::to_string(&input.to_string())?;
            Some(format!("const input = JSON.parse({literal}); {program}"))
        }
        None => None,
    };
    let provider = connect(config).await?;
    let facts = ServerFacts::of(&provider, config);
    let change_list = match &program {
        Some(code) => {
            let defaults = RunConfig::default();
            let run_config = RunConfig {
                timeout_ms: flags.number("--timeout-ms", 60_000)?,
                max_calls: flags.number("--max-calls", defaults.max_calls)?,
                max_concurrency: flags.number("--concurrency", defaults.max_concurrency)?,
                max_output_bytes: flags.number("--max-output-bytes", PROGRAM_OUTPUT_BYTES)?,
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
            let report = run(code, run_config, provider.clone(), cancellation).await;
            signal_task.abort();
            provider.close().await;
            let report = report?;
            if let Some(path) = flags.get("--trace") {
                write_file(
                    Path::new(path),
                    &format!("{}\n", serde_json::to_string(&report)?),
                )?;
            }
            if !report.status.is_success() {
                return Err(CliError(format!(
                    "the program ended with status {}: {}",
                    report.status.as_str(),
                    escape(report.diagnostic.as_deref().unwrap_or("no diagnostic"))
                ))
                .into());
            }
            report.output.unwrap_or(Value::Null)
        }
        None => {
            provider.close().await;
            let text = read_input(&source, MAX_PLAN_BYTES as u64, "operations")?;
            codemode::parse_json(&text)
                .map_err(|error| CliError(format!("--operations is not JSON: {error}")))?
        }
    };
    let recorded = relative_to(directory(&out), &server_path);
    let proposed = PlanFile::propose(
        &change_list,
        &facts,
        Some(recorded.to_string_lossy().into_owned()),
        server.digest.clone(),
    )?;
    let Some(plan) = proposed else {
        if flags.has("--json") {
            print_json(&json!({"file": null, "state": "no_changes", "writes": []}))?;
        } else {
            println!("No changes.");
        }
        return Ok(0);
    };
    let frozen = plan.frozen()?;
    // Escaping can make the file larger than the change list. A file that
    // no command would read back is not written.
    let text = plan.checked_text().map_err(|error| {
        CliError(format!(
            "the plan file would be too large: {error}. Nothing was written"
        ))
    })?;
    write_file(&out, &text)?;
    let mut report = StatusReport::new(&label, &plan, &frozen, None);
    // The path as `status` will show it: the recorded one, seen from here.
    let seen = recorded_server_path(&out, &recorded.to_string_lossy());
    report.server.file = Some(seen.display().to_string());
    report.server.file_state = ServerFileState::Same;
    report.server.command = config.full_command();
    if flags.has("--json") {
        print_json(&report)?;
    } else {
        let options = review_options(&report, default_policy(), flags.has("--full"));
        print(&render::review(&report, &plan, options))?;
    }
    Ok(0)
}

pub(crate) fn status(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let flags = Flags::parse(args, &["--full", "--json"])?;
    if flags.positional.len() != 1 {
        return Err(CliError("status takes one plan file".into()).into());
    }
    let loaded = load_plan(&flags)?;
    let history = history_of(&journal_path(&loaded.path))?;
    let mut report = StatusReport::new(
        &loaded.label,
        &loaded.plan,
        &loaded.frozen,
        history.as_ref(),
    );
    look_at_server_file(&mut report, &loaded.path, &loaded.plan);
    if flags.has("--json") {
        print_json(&report)?;
    } else {
        let options = review_options(&report, default_policy(), flags.has("--full"));
        print(&render::status(&report, &loaded.plan, options))?;
    }
    Ok(0)
}

/// Prints one line to stderr for each attempt, once the journal holds what
/// was observed of it.
struct Progress {
    provider: McpProvider,
    show: bool,
}

impl RecoverableProvider for Progress {
    fn declaration(&self) -> ProviderDeclaration {
        self.provider.declaration()
    }

    async fn authorize(&self, scope: &Scope, context: ProbeContext) -> Result<(), ProviderError> {
        self.provider.authorize(scope, context).await
    }

    async fn execute(&self, attempt: Attempt, context: CallContext) -> Result<Value, AttemptError> {
        self.provider.execute(attempt, context).await
    }

    fn recorded(&self, operation: &Operation, _number: u32, observed: &Observed) {
        if !self.show {
            return;
        }
        let outcome = match observed {
            Observed::Succeeded { .. } => "applied".to_string(),
            Observed::Failed { error, .. } if error.code == "rate_limited" => {
                "rate limited".to_string()
            }
            Observed::Failed { error, .. } => format!("failed ({})", escape(&error.code)),
            Observed::Unobserved { .. } | Observed::Withdrawn => return,
        };
        eprintln!(
            "  {}  {}  {outcome}",
            escape(&operation.id),
            escape(&operation.tool)
        );
    }

    async fn reconcile(
        &self,
        operation: Operation,
        keys: Vec<String>,
        context: ProbeContext,
    ) -> Result<Reconciliation, ProviderError> {
        self.provider.reconcile(operation, keys, context).await
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0))
}

/// The exit code for a session that ended with `error`. A journal error can
/// come after a write was sent, so the plan needs attention (3). Every other
/// error ends the session before anything is dispatched (2).
fn session_error_exit(error: &SessionError) -> u8 {
    match error {
        SessionError::Journal(_) => 3,
        SessionError::Refused { .. } | SessionError::AlreadyStarted | SessionError::Run(_) => 2,
    }
}

pub(crate) async fn apply(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let flags = Flags::parse(
        args,
        &[
            "--server",
            "--approve",
            "--approver",
            "--concurrency",
            "--max-attempts",
            "--deadline-ms",
            "--max-failures",
            "--full",
            "--json",
        ],
    )?;
    if flags.positional.len() != 1 {
        return Err(CliError("apply takes one plan file".into()).into());
    }
    let loaded = load_plan(&flags)?;
    let LoadedPlan {
        path,
        label,
        plan,
        frozen,
    } = &loaded;
    let digest = plan.digest();
    let json = flags.has("--json");
    let full = flags.has("--full");
    let shown = escape(label);

    let defaults = SessionOptions::default();
    let default_ms = u64::try_from(defaults.deadline.as_millis()).unwrap_or(u64::MAX);
    let deadline_ms = flags.number("--deadline-ms", default_ms)?;
    if deadline_ms == 0 {
        return Err(CliError("--deadline-ms must be positive".into()).into());
    }
    let max_failures = flags.number("--max-failures", defaults.max_failures)?;
    let policy_given = ["--concurrency", "--max-attempts"]
        .into_iter()
        .find(|flag| flags.has(flag));
    let policy = RecoveryPolicy {
        concurrency: flags.number("--concurrency", default_policy().concurrency)?,
        max_attempts: flags.number("--max-attempts", default_policy().max_attempts)?,
        ..default_policy()
    };
    policy.validate().map_err(|error| {
        CliError(format!(
            "--{} must be positive",
            error.field.replace('_', "-")
        ))
    })?;

    let finish = |report: &StatusReport| -> Result<u8, Box<dyn Error>> {
        if json {
            print_json(report)?;
        } else {
            let options = review_options(report, policy, full);
            print(&render::status(report, plan, options))?;
        }
        Ok(report.exit_code())
    };
    let refuse = |report: &StatusReport, message: &str| -> Result<u8, Box<dyn Error>> {
        if json {
            print_json(report)?;
        }
        eprintln!("codemode: {message}");
        Ok(4)
    };

    // Take the journal first: a second `apply` must fail before it reaches the
    // server.
    let journal_file = journal_path(path);
    let journal = if journal_file.exists() {
        Some(Journal::open(&journal_file, Durability::Storage)?)
    } else {
        None
    };
    let history = journal.as_ref().map(Journal::history).transpose()?;
    let approved = history
        .as_ref()
        .is_some_and(|history| !history.approvals.is_empty());
    let mut report = StatusReport::new(label, plan, frozen, history.as_ref());
    if approved {
        if report.state == PlanState::Edited {
            let was = history
                .as_ref()
                .and_then(approved_digest)
                .map_or_else(|| "unknown".to_string(), |digest| digest.to_string());
            return refuse(
                &report,
                &format!(
                    "not approved. {shown} was edited after its approval:\n  \
                     approved   {was}\n  \
                     this file  {digest}\n\
                     Nothing was sent. Restore the approved file to continue. To apply the \
                     edited plan as a new plan,\nsave it under another name; every write in it \
                     is then sent, also one that was applied before."
                ),
            );
        }
        if let Some(flag) = policy_given {
            return Err(CliError(format!(
                "{flag} is part of the approval and is accepted on the first run only"
            ))
            .into());
        }
        // The plan is approved already, so nobody is asked; a digest that is
        // passed anyway must still be this plan's.
        if let approval::Approval::Refused(message) =
            approval::approval(flags.get("--approve"), &digest, true, &shown)
        {
            return refuse(&report, &message);
        }
        if !report.work_remains() {
            drop(journal);
            look_at_server_file(&mut report, path, plan);
            return finish(&report);
        }
    } else if let Some(journal) = &journal
        && journal.plan().digest() != frozen.digest()
    {
        return Err(CliError(format!(
            "the journal {} holds another plan; move it away, or save the plan under another \
             name",
            escape(&journal_file.display().to_string())
        ))
        .into());
    }

    // The server file, read once. The plan binds its digest, so an approval
    // cannot be carried to another server file: `--server` or the recorded
    // path only say where the file is.
    let server_path = match (flags.get("--server"), &plan.server.file) {
        (Some(given), _) => PathBuf::from(given),
        (None, Some(recorded)) => recorded_server_path(path, recorded),
        (None, None) => {
            return Err(
                CliError("the plan file names no server file; pass --server".into()).into(),
            );
        }
    };
    let server_label = escape(&server_path.display().to_string());
    let server = server_file(&server_path)?;
    if server.digest != plan.server.config {
        return Err(CliError(format!(
            "the server file {server_label} is not the one this plan was made with: the plan \
             records sha256:{} and this file is sha256:{}. Pass the plan's server file with \
             --server, or run `codemode plan` again. Nothing was sent",
            plan.server.config.short(),
            server.digest.short()
        ))
        .into());
    }
    let config = &server.config;
    if config.identity() != plan.server.identity {
        return Err(CliError(format!(
            "this plan was made for {}, but {server_label} names {}. Run `codemode plan` \
             again. Nothing was sent",
            escape(&plan.server.identity),
            escape(&config.identity())
        ))
        .into());
    }
    // The operator's list of writes needs no server to be checked.
    if let Some(write) = plan
        .writes
        .iter()
        .find(|write| !config.allows_write(&write.tool))
    {
        return Err(CliError(format!(
            "write {:?} names {:?}, which \"write_tools\" in {server_label} does not allow. \
             Nothing was sent",
            write.id, write.tool
        ))
        .into());
    }
    report.server.file = Some(server_path.display().to_string());
    report.server.file_state = ServerFileState::Same;
    report.server.command = config.full_command();

    // The approval comes before the server is reached: a stdio server is a
    // command, and it is not started for a plan nobody approved.
    if !approved {
        if !json {
            let options = review_options(&report, policy, full);
            print(&render::review(&report, plan, options))?;
        }
        // The question needs the review on screen and a person to answer it.
        // With --json nobody is asked: stdout must hold one document only.
        let terminal = !json
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && io::stderr().is_terminal();
        match approval::approval(flags.get("--approve"), &digest, terminal, &shown) {
            approval::Approval::Given => {}
            approval::Approval::Ask => {
                eprint!("\n{}", render::question(plan));
                io::stderr().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                if !approval::answer_approves(&line) {
                    return refuse(&report, NOT_YES);
                }
            }
            approval::Approval::Refused(message) => return refuse(&report, &message),
        }
    }
    // An approval given just now is recorded only once the server is known
    // to be the one the plan was made for.
    let unrecorded = |error: Box<dyn Error>| -> Box<dyn Error> {
        if approved {
            return error;
        }
        CliError(format!(
            "the plan was approved, but nothing was recorded or sent: {error}"
        ))
        .into()
    };

    // Before any write: the server must still be the one the plan was made for.
    let provider = connect(config).await.map_err(unrecorded)?;
    if let Err(error) = plan.check_server(&ServerFacts::of(&provider, config)) {
        provider.close().await;
        return Err(unrecorded(error.into()));
    }
    let recorded = (|| -> Result<Journal, Box<dyn Error>> {
        let mut journal = match journal {
            Some(journal) => journal,
            None => Journal::create(&journal_file, frozen, Durability::Storage)?,
        };
        if !approved {
            let declaration = plan.declaration();
            journal.approve(&Approval {
                binding: approval_binding(frozen.digest(), &declaration, &policy),
                plan: frozen.digest().clone(),
                declaration,
                policy,
                approver: user(flags.get("--approver")),
                // The digest of the plan file this approval names.
                note: Some(digest.to_string()),
                approved_unix_ms: now_ms(),
            })?;
        }
        Ok(journal)
    })();
    let journal = match recorded {
        Ok(journal) => journal,
        Err(error) => {
            provider.close().await;
            return Err(unrecorded(error));
        }
    };
    let started = history
        .as_ref()
        .is_some_and(|history| !history.sessions.is_empty());
    let options = SessionOptions {
        kind: if started {
            SessionKind::Resume
        } else {
            SessionKind::Run
        },
        deadline: Duration::from_millis(deadline_ms),
        max_failures,
    };
    let progress = Progress {
        provider: provider.clone(),
        show: !json && io::stderr().is_terminal(),
    };
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let result = run_session(journal, progress, options, cancellation).await;
    signal_task.abort();
    provider.close().await;
    // From here on a write may have been sent: a failure is reported with
    // the exit code that says so.
    let after = result
        .map_err(|error| {
            let code = session_error_exit(&error);
            let text = match error {
                SessionError::Refused { .. } => {
                    "the server changed while the session started, so nothing was sent. Run \
                     `codemode plan` again"
                        .to_string()
                }
                other => other.to_string(),
            };
            (code, text)
        })
        .and_then(|_| history_of(&journal_file).map_err(|error| (3, error.to_string())));
    let history = match after {
        Ok(history) => history,
        Err((code, text)) => {
            eprintln!("codemode: {}", escape(&text));
            if code == 3 {
                eprintln!(
                    "codemode: the session had started, so a write may have been sent. Run \
                     `codemode status {shown}`."
                );
            }
            return Ok(code);
        }
    };
    let mut after = StatusReport::new(label, plan, frozen, history.as_ref());
    after.server = report.server.clone();
    if !json && !approved {
        println!();
    }
    finish(&after)
}

pub(crate) fn resolve(args: Vec<OsString>) -> Result<u8, Box<dyn Error>> {
    let flags = Flags::parse(args, &["--errors", "--no-reply", "--as", "--reviewer"])?;
    let loaded = load_plan(&flags)?;
    let shown = escape(&loaded.label);
    let ids = &flags.positional[1..];
    if ids.is_empty() && !flags.has("--errors") && !flags.has("--no-reply") {
        return Err(
            CliError("name the writes to resolve, or pass --errors or --no-reply".into()).into(),
        );
    }
    let (resolution, applied) = match flags.require("--as")? {
        "applied" => (Resolution::Applied, true),
        "not-applied" => (Resolution::NotApplied, false),
        _ => return Err(CliError("--as must be applied or not-applied".into()).into()),
    };
    let journal_file = journal_path(&loaded.path);
    if !journal_file.exists() {
        return Err(CliError(format!(
            "{shown} was not approved yet, so no write was sent and there is nothing to resolve"
        ))
        .into());
    }
    let mut journal = Journal::open(&journal_file, Durability::Storage)?;
    let report = StatusReport::new(
        &loaded.label,
        &loaded.plan,
        &loaded.frozen,
        Some(&journal.history()?),
    );
    if report.state == PlanState::Edited {
        eprintln!(
            "codemode: {shown} was edited after its approval, so its writes are not the ones \
             the journal describes. Restore the approved file first. Nothing was recorded."
        );
        return Ok(4);
    }
    let open = |write: &WriteStatus| {
        matches!(
            write.state,
            WriteState::Failed | WriteState::NoReply | WriteState::Refused
        )
    };
    for id in ids {
        let Some(write) = report.writes.iter().find(|write| &write.id == id) else {
            return Err(CliError(format!("the plan has no write '{}'", escape(id))).into());
        };
        if !open(write) {
            let why = match write.state {
                WriteState::Applied => "it is applied",
                WriteState::Resolved => "a person already resolved it as applied",
                _ => "it is known not to be applied, and `codemode apply` sends it",
            };
            return Err(CliError(format!(
                "write '{}' has nothing to resolve: {why}. Nothing was recorded",
                escape(id)
            ))
            .into());
        }
    }
    let selected: Vec<&WriteStatus> = report
        .writes
        .iter()
        .filter(|write| {
            ids.contains(&write.id)
                || (flags.has("--errors") && write.state == WriteState::Failed)
                || (flags.has("--no-reply") && write.state == WriteState::NoReply)
        })
        .collect();
    if selected.is_empty() {
        println!("Nothing to resolve: no write matches.");
        return Ok(0);
    }
    let reviewer = user(flags.get("--reviewer"));
    print(&render::resolution(&selected, applied, &reviewer))?;
    for write in &selected {
        journal.record_review(&write.id, resolution, &reviewer, None)?;
    }
    let after = StatusReport::new(
        &loaded.label,
        &loaded.plan,
        &loaded.frozen,
        Some(&journal.history()?),
    );
    println!(
        "\nRecorded {}. {}",
        plural(selected.len(), "write"),
        if after.work_remains() {
            format!("Next: codemode apply {shown}")
        } else if after.state == PlanState::Done {
            "Every write is applied or resolved.".to_string()
        } else {
            format!("Next: codemode status {shown}")
        }
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_journal_lives_next_to_the_plan() {
        for (plan, journal) in [
            ("triage.plan.json", "triage.plan.journal.db"),
            ("dir/x.json", "dir/x.journal.db"),
            ("plan", "plan.journal.db"),
            ("a.json/b.txt", "a.json/b.txt.journal.db"),
        ] {
            assert_eq!(journal_path(Path::new(plan)), PathBuf::from(journal));
        }
    }

    #[test]
    fn an_error_after_a_session_started_needs_attention() {
        let journal = SessionError::Journal(JournalError::Storage("disk full".into()));
        assert_eq!(session_error_exit(&journal), 3);
        let refused = SessionError::Refused {
            session: 1,
            reason: "changed".into(),
        };
        assert_eq!(session_error_exit(&refused), 2);
        assert_eq!(session_error_exit(&SessionError::AlreadyStarted), 2);
    }

    #[test]
    fn a_recorded_server_path_is_found_from_where_the_command_runs() {
        let found = |plan: &str, recorded: &str| recorded_server_path(Path::new(plan), recorded);
        assert_eq!(
            found("p.plan.json", "server.json"),
            PathBuf::from("server.json")
        );
        assert_eq!(
            found("plans/p.plan.json", "../server.json"),
            PathBuf::from("plans/../server.json")
        );
        assert_eq!(
            found("plans/p.plan.json", "/etc/s.json"),
            PathBuf::from("/etc/s.json")
        );
    }

    #[test]
    fn the_server_file_is_recorded_relative_to_the_plan() {
        let root = std::env::temp_dir().join(format!("codemode-relative-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("plans/deep")).expect("directories");
        std::fs::create_dir_all(root.join("servers")).expect("directories");
        std::fs::write(root.join("servers/s.json"), "{}").expect("file");
        std::fs::write(root.join("plans/s.json"), "{}").expect("file");
        let relative = |base: &str, target: &str| relative_to(&root.join(base), &root.join(target));
        assert_eq!(relative("plans", "plans/s.json"), PathBuf::from("s.json"));
        assert_eq!(
            relative("plans", "servers/s.json"),
            PathBuf::from("../servers/s.json")
        );
        assert_eq!(
            relative("plans/deep", "servers/s.json"),
            PathBuf::from("../../servers/s.json")
        );
        // What cannot be resolved stays an absolute path.
        assert!(relative("plans", "servers/missing.json").is_absolute());
        let _ = std::fs::remove_dir_all(&root);
    }
}
