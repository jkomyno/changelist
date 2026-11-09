//! `codemode plan`: check, approve, run, resume, inspect, review, and trace an
//! operation plan's journal. Process I/O only; behavior lives in the library.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use codemode::fixture::FixtureProvider;
use codemode::{
    Approval, Durability, Journal, JournalError, JournalInspectOptions, Plan, PlanBridgeError,
    ProviderDeclaration, RecoveryPolicy, Resolution, RunCancellation, SessionError, SessionKind,
    SessionOptions, SessionOutcome, approval_binding, inspect_journal, run_session,
    serve_plan_bridge,
};

use crate::{CliError, cancel_on_interrupt, read_input};

const MAX_PLAN_BYTES: u64 = codemode::MAX_PLAN_BYTES as u64;
const MAX_DECLARATION_BYTES: u64 = 256 * 1024;

pub(crate) const HELP: &str =
    "codemode plan — review, execute, and recover a batch of external operations

Usage:
  codemode plan check <plan.json> [--provider <declaration.json>] [--json]
  codemode plan approve <plan.json> --journal <file> --provider <declaration.json>
                        --digest <sha256:...> --approver <name> [--note <text>] [policy options]
  codemode plan run     --journal <file> (--fixture <fixture.db> --account <name> | --bridge)
                        [--deno <path>]
  codemode plan resume  --journal <file> (--fixture <fixture.db> --account <name> | --bridge)
                        [--deno <path>]
  codemode plan inspect --journal <file> [--json] [--messages]
  codemode plan review  --journal <file> --operation <id> --as applied|not-applied
                        --reviewer <name> [--note <text>]
  codemode plan trace   --journal <file> --session <n>

check validates a plan and prints its digest and operations; with a provider
declaration it also shows each tool's recovery contract. approve records an
approval bound to the plan digest (which --digest must repeat), the provider's
identity and contracts, and the policy. run executes an approved plan once;
resume reconciles uncertain operations with the provider and continues the
safely recoverable work. Both revalidate the approval and the provider's
current authorization first. inspect answers what was approved, attempted,
changed, uncertain, and what can safely happen next, without argument or
result values. review records a person's finding for an operation that needs
one. trace prints a session's execution record for `codemode inspect`.

run and resume act through the offline fixture (--fixture, --account) or, with
--bridge, through a parent host: stdin and stdout carry protocol version 3
frames (start, authorize, call, reconcile, their replies, cancel, outcome). The
host's start frame supplies its provider declaration; it authorizes, executes,
and reconciles operations while Rust keeps the journal. Credentials stay with
the host. Approved plans run natively; --deno is accepted for compatibility
and has no effect.

Policy options (approve):
  --concurrency <n>          Operations in flight at once (default 4)
  --max-attempts <n>         Dispatches per operation, first included (default 3)
  --max-probes <n>           Reconciliation probes per operation (default 3)
  --attempt-timeout-ms <n>   Deadline per attempt (default 10000)
  --probe-timeout-ms <n>     Deadline per probe (default 5000)
  --session-timeout-ms <n>   Deadline per dispatch phase (default 60000)

Every journal command accepts --durability storage|process (default storage:
each commit asks the OS to flush to storage; process: survives a killed process,
not a power loss).

Exit status for run and resume: 0 every operation is applied or satisfied;
3 work remains (a review, or another resume); 4 the session was refused;
124 deadline; 130 cancelled; 2 usage, journal, or startup error.
";

const BOOLEAN_FLAGS: &[&str] = &["--json", "--messages", "--bridge"];

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

    fn durability(&self) -> Result<Durability, CliError> {
        match self.get("--durability") {
            None | Some("storage") => Ok(Durability::Storage),
            Some("process") => Ok(Durability::Process),
            Some(_) => Err(CliError("--durability must be storage or process".into())),
        }
    }

    fn one_positional(&self, what: &str) -> Result<PathBuf, CliError> {
        match self.positional.as_slice() {
            [path] => Ok(PathBuf::from(path)),
            _ => Err(CliError(format!("expected one {what}"))),
        }
    }
}

fn read_plan(path: &PathBuf) -> Result<codemode::FrozenPlan, Box<dyn Error>> {
    let text = read_input(path, MAX_PLAN_BYTES, "plan")?;
    Ok(Plan::parse(&text)?)
}

fn read_declaration(path: &PathBuf) -> Result<ProviderDeclaration, Box<dyn Error>> {
    let text = read_input(path, MAX_DECLARATION_BYTES, "provider declaration")?;
    serde_json::from_str(&text)
        .map_err(|error| CliError(format!("not a provider declaration: {error}")).into())
}

fn print_json(value: &impl serde::Serialize) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
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
        Some("check") => check(Flags::parse(rest, &["--provider", "--json"])?),
        Some("approve") => approve(Flags::parse(
            rest,
            &[
                "--journal",
                "--provider",
                "--digest",
                "--approver",
                "--note",
                "--durability",
                "--concurrency",
                "--max-attempts",
                "--max-probes",
                "--attempt-timeout-ms",
                "--probe-timeout-ms",
                "--session-timeout-ms",
            ],
        )?),
        Some(kind @ ("run" | "resume")) => {
            let flags = Flags::parse(
                rest,
                &[
                    "--journal",
                    "--fixture",
                    "--account",
                    "--bridge",
                    "--deno",
                    "--durability",
                ],
            )?;
            let kind = if kind == "run" {
                SessionKind::Run
            } else {
                SessionKind::Resume
            };
            session(flags, kind).await
        }
        Some("inspect") => inspect(Flags::parse(
            rest,
            &["--journal", "--json", "--messages", "--durability"],
        )?),
        Some("review") => review(Flags::parse(
            rest,
            &[
                "--journal",
                "--operation",
                "--as",
                "--reviewer",
                "--note",
                "--durability",
            ],
        )?),
        Some("trace") => trace(Flags::parse(
            rest,
            &["--journal", "--session", "--durability"],
        )?),
        Some(other) => Err(CliError(format!(
            "unknown plan command '{other}'; use codemode plan --help"
        ))
        .into()),
    }
}

fn check(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let plan = read_plan(&flags.one_positional("plan file")?)?;
    let declaration = flags
        .get("--provider")
        .map(|path| read_declaration(&PathBuf::from(path)))
        .transpose()?;
    if let Some(declaration) = &declaration {
        declaration
            .admits(&plan)
            .map_err(|error| CliError(error.to_string()))?;
    }
    if flags.has("--json") {
        let operations: Vec<_> = plan
            .plan()
            .operations
            .iter()
            .map(|operation| {
                serde_json::json!({
                    "operation": operation.id,
                    "tool": operation.tool,
                    "scope": operation.scope,
                    "argument_keys": operation.arguments.as_object().map(|map| map.keys().collect::<Vec<_>>()),
                    "preconditions": operation.preconditions.len(),
                    "contract": declaration.as_ref().and_then(|d| d.contracts.get(&operation.tool)),
                })
            })
            .collect();
        print_json(&serde_json::json!({
            "digest": plan.digest(),
            "summary": plan.plan().summary,
            "operations": operations,
        }))?;
        return Ok(0);
    }
    let mut out = String::new();
    out.push_str(&format!("plan {}\n", plan.digest()));
    if let Some(summary) = &plan.plan().summary {
        out.push_str(&format!("summary: {summary}\n"));
    }
    out.push_str(&format!("{} operations:\n", plan.plan().operations.len()));
    for operation in &plan.plan().operations {
        let keys: Vec<&String> = operation
            .arguments
            .as_object()
            .map(|map| map.keys().collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "  {} {} {} {{{}}}",
            operation.id,
            operation.tool,
            operation.scope,
            keys.iter()
                .map(|key| key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !operation.preconditions.is_empty() {
            out.push_str(&format!(
                ", {} precondition(s)",
                operation.preconditions.len()
            ));
        }
        if let Some(contract) = declaration
            .as_ref()
            .and_then(|declaration| declaration.contracts.get(&operation.tool))
        {
            let value = serde_json::to_value(contract)?;
            out.push_str(&format!(
                "; if its response is lost: reconcile by {}, repeat {}",
                value["reconcile"].as_str().unwrap_or("?"),
                value["repeat"].as_str().unwrap_or("?")
            ));
        }
        out.push('\n');
    }
    out.push_str("Arguments are not printed; read the plan file to review values.\n");
    out.push_str(&format!(
        "To approve exactly this plan: codemode plan approve <plan> --digest {} ...\n",
        plan.digest()
    ));
    io::stdout().write_all(out.as_bytes())?;
    Ok(0)
}

fn approve(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let plan = read_plan(&flags.one_positional("plan file")?)?;
    let journal_path = PathBuf::from(flags.require("--journal")?);
    let declaration = read_declaration(&PathBuf::from(flags.require("--provider")?))?;
    let confirmed = flags.require("--digest")?;
    if confirmed != plan.digest().as_str() {
        return Err(CliError(format!(
            "--digest {confirmed} does not match the plan's digest {}; review the plan again",
            plan.digest()
        ))
        .into());
    }
    declaration
        .admits(&plan)
        .map_err(|error| CliError(error.to_string()))?;
    let defaults = RecoveryPolicy::default();
    let policy = RecoveryPolicy {
        concurrency: flags.number("--concurrency", defaults.concurrency)?,
        max_attempts: flags.number("--max-attempts", defaults.max_attempts)?,
        max_probes: flags.number("--max-probes", defaults.max_probes)?,
        attempt_timeout_ms: flags.number("--attempt-timeout-ms", defaults.attempt_timeout_ms)?,
        probe_timeout_ms: flags.number("--probe-timeout-ms", defaults.probe_timeout_ms)?,
        session_timeout_ms: flags.number("--session-timeout-ms", defaults.session_timeout_ms)?,
    };
    policy
        .validate()
        .map_err(|error| CliError(error.to_string()))?;
    let durability = flags.durability()?;
    let mut journal = if journal_path.exists() {
        Journal::open(&journal_path, durability)?
    } else {
        Journal::create(&journal_path, &plan, durability)?
    };
    let declaration = declaration.for_plan(&plan);
    let approval = Approval {
        binding: approval_binding(plan.digest(), &declaration, &policy),
        plan: plan.digest().clone(),
        declaration,
        policy,
        approver: flags.require("--approver")?.to_string(),
        note: flags.get("--note").map(String::from),
        approved_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0)),
    };
    journal.approve(&approval)?;
    print_json(&serde_json::json!({
        "journal": journal_path.display().to_string(),
        "plan": approval.plan,
        "binding": approval.binding,
        "operations": plan.plan().operations.len(),
    }))?;
    Ok(0)
}

fn exit_code(outcome: &SessionOutcome) -> u8 {
    match outcome.status.as_str() {
        "completed" => 0,
        "interrupted" => match outcome.trace.as_ref().map(|trace| trace.status) {
            Some(status) => status.exit_code().max(1),
            None => 130,
        },
        _ => 3,
    }
}

async fn session(flags: Flags, kind: SessionKind) -> Result<u8, Box<dyn Error>> {
    let durability = flags.durability()?;
    let options = SessionOptions {
        kind,
        deno: flags
            .get("--deno")
            .map_or_else(|| "deno".into(), PathBuf::from),
    };
    if flags.has("--bridge") {
        if flags.has("--fixture") || flags.has("--account") {
            return Err(CliError(
                "--bridge cannot be combined with --fixture or --account; the host is the provider"
                    .into(),
            )
            .into());
        }
        return bridge_session(flags.require("--journal")?, durability, options).await;
    }
    let journal = Journal::open(flags.require("--journal")?, durability)?;
    let provider = FixtureProvider::open(flags.require("--fixture")?, flags.require("--account")?)?;
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let result = run_session(journal, provider, options, cancellation).await;
    signal_task.abort();
    match result {
        Ok(outcome) => {
            print_json(&outcome)?;
            Ok(exit_code(&outcome))
        }
        Err(SessionError::Refused { session, reason }) => {
            print_json(&serde_json::json!({
                "session": session, "status": "refused", "detail": reason,
            }))?;
            eprintln!("codemode: session {session} refused: {reason}");
            Ok(4)
        }
        Err(error) => Err(error.into()),
    }
}

/// Serve one session over stdio with the host as provider. stdout carries only frames.
async fn bridge_session(
    journal: &str,
    durability: Durability,
    options: SessionOptions,
) -> Result<u8, Box<dyn Error>> {
    let journal = match Journal::open(journal, durability) {
        Ok(journal) => journal,
        Err(error) => {
            print_json(&serde_json::json!({"type": "error", "message": error.to_string()}))?;
            return Err(error.into());
        }
    };
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let result = serve_plan_bridge(
        tokio::io::stdin(),
        tokio::io::stdout(),
        journal,
        options,
        cancellation,
    )
    .await;
    signal_task.abort();
    match result {
        Ok(outcome) => Ok(exit_code(&outcome)),
        Err(PlanBridgeError::Session(SessionError::Refused { session, reason })) => {
            eprintln!("codemode: session {session} refused: {reason}");
            Ok(4)
        }
        Err(error) => Err(error.into()),
    }
}

fn inspect(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let journal = Journal::open(flags.require("--journal")?, flags.durability()?)?;
    let options = JournalInspectOptions {
        messages: flags.has("--messages"),
    };
    let inspection = inspect_journal(&journal, &options)?;
    if flags.has("--json") {
        print_json(&inspection)?;
    } else {
        io::stdout().write_all(inspection.render().as_bytes())?;
    }
    Ok(0)
}

fn review(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let mut journal = Journal::open(flags.require("--journal")?, flags.durability()?)?;
    let resolution = match flags.require("--as")? {
        "applied" => Resolution::Applied,
        "not-applied" => Resolution::NotApplied,
        _ => return Err(CliError("--as must be applied or not-applied".into()).into()),
    };
    let operation = flags.require("--operation")?;
    journal.record_review(
        operation,
        resolution,
        flags.require("--reviewer")?,
        flags.get("--note"),
    )?;
    print_json(&serde_json::json!({"operation": operation, "recorded": resolution}))?;
    Ok(0)
}

fn trace(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let journal = Journal::open(flags.require("--journal")?, flags.durability()?)?;
    let session: u64 = flags
        .require("--session")?
        .parse()
        .map_err(|_| CliError("--session requires a session number".into()))?;
    let Some(report) = journal.trace(session)? else {
        return Err(JournalError::Conflict(format!(
            "session {session} has no stored execution record (it dispatched nothing, or did not finish)"
        ))
        .into());
    };
    print_json(&report)?;
    Ok(0)
}
