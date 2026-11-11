//! `codemode plan`: check, approve, run, resume, inspect, review, and trace an
//! operation plan's journal. Process I/O only; behavior lives in the library.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use codemode::{
    Approval, Durability, Journal, JournalError, JournalInspectOptions, JournalReader, Plan,
    PlanBridgeError, ProviderDeclaration, RecoveryPolicy, Resolution, RunCancellation,
    SessionError, SessionKind, SessionOptions, approval_binding, inspect_journal,
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
  codemode plan run     --journal <file> --bridge [--deadline-ms <n>]
  codemode plan resume  --journal <file> --bridge [--deadline-ms <n>]
  codemode plan inspect --journal <file> [--json] [--messages]
  codemode plan review  --journal <file> --operation <id> --as applied|not-applied
                        --reviewer <name> [--note <text>]
  codemode plan trace   --journal <file> --session <n>

check validates a plan and prints its digest and every operation exactly as
approval binds it: tool, scope, and canonical arguments and preconditions in
full, never truncated (a plan is at most 1 MiB). With a provider declaration it
also shows each tool's recovery contract and the retention policy; it always
shows the default policy. --json prints the same as one object whose plan field
is the canonical plan the digest covers. Plans hold numbers only within
-(2^53-1)..=2^53-1, which every JSON host reads exactly. approve records an
approval bound to the plan digest (which --digest must repeat), the provider's
identity and contracts, and the policy. run executes an approved plan once;
resume reconciles uncertain operations with the provider and continues the
safely recoverable work. Both revalidate the approval and the provider's
current authorization first. inspect answers what was approved, attempted,
changed, uncertain, and what can safely happen next, without argument or
result values. It reads the journal read-only, so it works while a session
runs; an open session may be running or may have been killed. review records
a person's finding for an operation that needs one. trace prints a session's
execution record for `codemode inspect`.

run and resume act through a parent host (--bridge): stdin and stdout carry
protocol version 3 frames (start, authorize, call, reconcile, their replies,
cancel, outcome). The host's start frame supplies its provider declaration; it
authorizes, executes, and reconciles operations while Rust keeps the journal.
Credentials stay with the host. Approved plans run natively.

Policy options (approve; a change needs a new approval):
  --concurrency <n>          Operations in flight at once (default 4)
  --max-attempts <n>         Dispatches per operation, first included (default 3)
  --max-probes <n>           Reconciliation probes per operation (default 3)

Session option (run, resume; not approved, so each session may choose its own):
  --deadline-ms <n>          One deadline for the whole session: authorization,
                             reconciliation probes, and dispatch, including host
                             permission waits inside a call (default 300000).
                             Reached before an attempt's intent is recorded, it
                             withdraws the attempt; after, the attempt is unknown.
                             A probe it cuts off still counts against --max-probes.

Every journal commit asks the OS to flush to storage before it returns.

Exit status for run and resume: 0 every operation is applied or satisfied;
3 work remains (a review, or another resume); 4 the session was refused;
124 the deadline interrupted the session, in any phase; 130 cancelled;
1 the dispatch run stopped for another reason; 2 usage, journal, or startup error.
";

const BOOLEAN_FLAGS: &[&str] = &["--json", "--messages", "--bridge"];

pub(crate) struct Flags {
    positional: Vec<String>,
    values: HashMap<String, String>,
}

impl Flags {
    pub(crate) fn parse(args: Vec<OsString>, allowed: &[&str]) -> Result<Self, CliError> {
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

    pub(crate) fn get(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }

    pub(crate) fn has(&self, flag: &str) -> bool {
        self.values.contains_key(flag)
    }

    pub(crate) fn require(&self, flag: &str) -> Result<&str, CliError> {
        self.get(flag)
            .ok_or_else(|| CliError(format!("{flag} is required")))
    }

    pub(crate) fn number<T: std::str::FromStr>(
        &self,
        flag: &str,
        default: T,
    ) -> Result<T, CliError> {
        match self.get(flag) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| CliError(format!("{flag} requires a nonnegative integer"))),
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

pub(crate) fn print_json(value: &impl serde::Serialize) -> Result<(), Box<dyn Error>> {
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
                "--concurrency",
                "--max-attempts",
                "--max-probes",
            ],
        )?),
        Some(kind @ ("run" | "resume")) => {
            let flags = Flags::parse(rest, &["--journal", "--bridge", "--deadline-ms"])?;
            let kind = if kind == "run" {
                SessionKind::Run
            } else {
                SessionKind::Resume
            };
            session(flags, kind).await
        }
        Some("inspect") => inspect(Flags::parse(rest, &["--journal", "--json", "--messages"])?),
        Some("review") => review(Flags::parse(
            rest,
            &["--journal", "--operation", "--as", "--reviewer", "--note"],
        )?),
        Some("trace") => trace(Flags::parse(rest, &["--journal", "--session"])?),
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
    let declaration = declaration.map(|declaration| declaration.for_plan(&plan));
    let policy = RecoveryPolicy::default();
    if flags.has("--json") {
        // The canonical plan, whose bytes the digest covers.
        let mut canonical = serde_json::to_value(plan.plan())?;
        canonical.sort_all_objects();
        print_json(&serde_json::json!({
            "digest": plan.digest(),
            "plan": canonical,
            "declaration": declaration,
            "default_policy": policy,
        }))?;
        return Ok(0);
    }
    let mut out = String::new();
    out.push_str(&format!("plan {}\n", plan.digest()));
    if let Some(summary) = &plan.plan().summary {
        out.push_str(&format!("summary: {summary:?}\n"));
    }
    out.push_str(&format!(
        "{} operations, each exactly as approval binds it:\n",
        plan.plan().operations.len()
    ));
    for operation in &plan.plan().operations {
        out.push_str(&format!("  {:?}\n", operation.id));
        out.push_str(&format!("    tool {:?}\n", operation.tool));
        out.push_str(&format!("    scope {}\n", review_json(&operation.scope)?));
        out.push_str(&format!(
            "    arguments {}\n",
            review_json(&operation.arguments)?
        ));
        if !operation.preconditions.is_empty() {
            out.push_str(&format!(
                "    preconditions {}\n",
                review_json(&operation.preconditions)?
            ));
        }
        if let Some(contract) = declaration
            .as_ref()
            .and_then(|declaration| declaration.contracts.get(&operation.tool))
        {
            let value = serde_json::to_value(contract)?;
            out.push_str(&format!(
                "    if its response is lost: reconcile by {}, repeat {}\n",
                value["reconcile"].as_str().unwrap_or("?"),
                value["repeat"].as_str().unwrap_or("?")
            ));
        }
        if let Some(definition) = declaration
            .as_ref()
            .and_then(|declaration| declaration.definitions.get(&operation.tool))
        {
            out.push_str(&format!("    tool definition {definition}\n"));
        }
    }
    match &declaration {
        Some(declaration) => out.push_str(&format!(
            "provider {:?} as {:?}, retention {}\n",
            declaration.provider, declaration.account, declaration.retention
        )),
        None => out.push_str(
            "provider: not given; pass --provider to see the contracts and retention \
             approval binds\n",
        ),
    }
    out.push_str(&format!(
        "default policy: concurrency {}, max-attempts {}, max-probes {} \
         (approve's policy options change it)\n",
        policy.concurrency, policy.max_attempts, policy.max_probes
    ));
    out.push_str(&format!(
        "To approve exactly this plan: codemode plan approve <plan> --digest {} ...\n",
        plan.digest()
    ));
    io::stdout().write_all(out.as_bytes())?;
    Ok(0)
}

/// Canonical JSON of `value` for a terminal: keys sorted, and every character a
/// terminal could act on (controls, bidirectional overrides) written as a JSON
/// escape, so the line still parses to exactly the approved value.
fn review_json(value: &impl serde::Serialize) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    let mut text = String::new();
    for character in value.to_string().chars() {
        let printable = if character.is_ascii() {
            !character.is_ascii_control()
        } else {
            character.escape_debug().next() != Some('\\')
        };
        if printable {
            text.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                text.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    Ok(text)
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
    };
    policy
        .validate()
        .map_err(|error| CliError(error.to_string()))?;
    let mut journal = if journal_path.exists() {
        Journal::open(&journal_path, Durability::Storage)?
    } else {
        Journal::create(&journal_path, &plan, Durability::Storage)?
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

async fn session(flags: Flags, kind: SessionKind) -> Result<u8, Box<dyn Error>> {
    if !flags.has("--bridge") {
        return Err(CliError(
            "--bridge is required; the parent host executes the plan's operations".into(),
        )
        .into());
    }
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
    bridge_session(flags.require("--journal")?, options).await
}

/// Serve one session over stdio with the host as provider. stdout carries only frames.
async fn bridge_session(journal: &str, options: SessionOptions) -> Result<u8, Box<dyn Error>> {
    let journal = match Journal::open(journal, Durability::Storage) {
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
        Ok(outcome) => Ok(outcome.exit_code()),
        Err(PlanBridgeError::Session(SessionError::Refused { session, reason })) => {
            eprintln!("codemode: session {session} refused: {reason}");
            Ok(4)
        }
        Err(error) => Err(error.into()),
    }
}

fn inspect(flags: Flags) -> Result<u8, Box<dyn Error>> {
    let journal = JournalReader::open(flags.require("--journal")?)?;
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
    let mut journal = Journal::open(flags.require("--journal")?, Durability::Storage)?;
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
    let journal = JournalReader::open(flags.require("--journal")?)?;
    let session: u64 = flags
        .require("--session")?
        .parse()
        .map_err(|_| CliError("--session requires a session number".into()))?;
    let Some(report) = journal.trace(session)? else {
        return Err(JournalError::Conflict(format!(
            "session {session} has no stored execution record (it dispatched nothing, or has not finished)"
        ))
        .into());
    };
    print_json(&report)?;
    Ok(0)
}
