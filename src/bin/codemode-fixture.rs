//! Command-line process I/O for the offline issue-tracker fixture.
//!
//! The logic lives in [`codemode::fixture`]; this binary parses flags, calls it,
//! and prints results. `call` and `reconcile` let a host in another language use
//! the fixture as a separate process. `session` hosts one plan session in this
//! process with the fixture as provider, so tests and evaluations can kill it.

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use codemode::fixture::{
    FIXTURE_PROVIDER, FaultMode, Fixture, FixtureError, FixtureProvider, LabelEdit, Ledger, Seed,
    State,
};
use codemode::{
    Attempt, Durability, Journal, Operation, ProbeContext, RecoverableProvider, RunCancellation,
    Scope, SessionError, SessionKind, SessionOptions, SessionOutcome, run_session,
};
use serde_json::{Value, json};
use tokio::time::Instant;

const HELP: &str = "codemode-fixture — an offline issue tracker with a side-effect ledger

Usage:
  codemode-fixture init --db PATH --repo NAME --account NAME... --issues LIST
                        [--closed LIST] [--label N:LABEL]...
  codemode-fixture declare --db PATH --account NAME
  codemode-fixture fault --db PATH --tool TOOL --issue N --mode lose-response|hold [--count N]
  codemode-fixture latency --db PATH --ms N
  codemode-fixture revoke --db PATH --account NAME
  codemode-fixture restore --db PATH --account NAME
  codemode-fixture edit --db PATH --issue N (--add-label L | --remove-label L)
  codemode-fixture ledger --db PATH [--json]
  codemode-fixture state --db PATH [--json]
  codemode-fixture call --db PATH --account NAME --tool TOOL --key KEY --arguments JSON
                        [--preconditions JSON-ARRAY] [--scope JSON]
  codemode-fixture authorize --db PATH --account NAME --scope JSON
  codemode-fixture reconcile --db PATH --account NAME --tool TOOL --arguments JSON --keys K1,K2
                             [--scope JSON]
  codemode-fixture session run|resume --journal PATH --db PATH --account NAME

Tools: issues.add_label {issue, label}, issues.remove_label {issue, label},
issues.add_comment {issue, body}. Precondition: {\"issue_state\": \"open\"|\"closed\"}.
LIST is comma-separated numbers or ranges, such as 101-110 or 101,105.

init fails if the file exists. edit acts as 'external', outside codemode.
call prints one JSON line, {\"ok\":true,\"value\":...} or
{\"ok\":false,\"error\":{\"code\",\"message\"},\"effect\":\"refused|not_applied|unknown\"};
a matching fault makes it never return. With --scope, call refuses a scope that is
not this account and repository (permission_denied), and reconcile one (unauthorized). authorize prints
{\"ok\":true} or {\"ok\":false,\"error\":{...}}. reconcile prints {\"verdict\",\"evidence\"}
or {\"error\":{...}}.

session runs or resumes the approved plan in a journal (see `codemode plan`) in
this process, with the fixture acting as the account, and prints the session
outcome as one JSON line.

Exit status: 0 success; 1 a tool error or refusal from call, authorize, or reconcile;
2 a usage, storage, or journal error (message on stderr). session exits as
`codemode plan run` does: 0 completed, 3 work remains, 4 refused, 124 deadline,
130 cancelled.
";

enum Failure {
    Usage(String),
    Fixture(FixtureError),
    /// The journal could not be opened, or the session failed other than by refusal.
    Session(String),
    /// The JSON line was already printed.
    Tool,
}

impl From<FixtureError> for Failure {
    fn from(error: FixtureError) -> Self {
        Self::Fixture(error)
    }
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(message.into())
}

/// Parsed `--flag value` pairs plus `--json`.
struct Flags {
    values: Vec<(String, String)>,
    json: bool,
}

impl Flags {
    fn parse(args: &[String], allowed: &[&str], json: bool) -> Result<Self, Failure> {
        let mut flags = Self {
            values: Vec::new(),
            json: false,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if json && arg == "--json" {
                flags.json = true;
                continue;
            }
            let Some(name) = arg.strip_prefix("--").filter(|name| allowed.contains(name)) else {
                return Err(usage(format!("unexpected argument '{arg}'")));
            };
            let value = args
                .next()
                .ok_or_else(|| usage(format!("--{name} requires a value")))?;
            flags.values.push((name.to_string(), value.clone()));
        }
        Ok(flags)
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn optional(&self, name: &str) -> Result<Option<&str>, Failure> {
        match self.all(name).as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value)),
            _ => Err(usage(format!("--{name} may appear once"))),
        }
    }

    fn required(&self, name: &str) -> Result<&str, Failure> {
        self.optional(name)?
            .ok_or_else(|| usage(format!("--{name} is required")))
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, Failure> {
        self.optional(name)?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| usage(format!("--{name} requires a nonnegative integer")))
            })
            .transpose()
    }

    fn json_value(&self, name: &str) -> Result<Option<Value>, Failure> {
        self.optional(name)?
            .map(|text| {
                codemode::parse_json(text).map_err(|error| usage(format!("--{name}: {error}")))
            })
            .transpose()
    }
}

/// Parse `101-110,115` into issue numbers.
fn issue_list(text: &str) -> Result<Vec<u64>, Failure> {
    let invalid = || usage(format!("'{text}' is not a list of issue numbers or ranges"));
    let mut issues = Vec::new();
    for part in text.split(',') {
        let number = |value: &str| value.trim().parse::<u64>().map_err(|_| invalid());
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (number(first)?, number(last)?);
                if first > last || last - first >= 100_000 {
                    return Err(invalid());
                }
                issues.extend(first..=last);
            }
            None => issues.push(number(part)?),
        }
    }
    Ok(issues)
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value)
            .unwrap_or_else(|error| json!({"error": error.to_string()}).to_string())
    );
}

fn operation(account: &str, tool: &str, arguments: Value, preconditions: Vec<Value>) -> Operation {
    Operation {
        id: "cli".into(),
        tool: tool.into(),
        scope: Scope {
            provider: FIXTURE_PROVIDER.into(),
            account: account.into(),
            destination: None,
        },
        arguments,
        preconditions,
    }
}

fn scope(value: Value) -> Result<Scope, Failure> {
    serde_json::from_value(value).map_err(|error| usage(format!("--scope is not a scope: {error}")))
}

fn context() -> ProbeContext {
    ProbeContext {
        // The caller ends a call that never returns, for example by killing this process.
        deadline: Instant::now() + Duration::from_secs(365 * 24 * 60 * 60),
        cancellation: RunCancellation::default(),
    }
}

async fn run(command: &str, args: &[String]) -> Result<u8, Failure> {
    match command {
        "session" => return session(args).await,
        "init" => {
            let flags = Flags::parse(
                args,
                &["db", "repo", "account", "issues", "closed", "label"],
                false,
            )?;
            let labels = flags
                .all("label")
                .into_iter()
                .map(|value| {
                    let (issue, label) = value
                        .split_once(':')
                        .ok_or_else(|| usage(format!("--label '{value}' is not N:LABEL")))?;
                    let issue = issue
                        .parse()
                        .map_err(|_| usage(format!("--label '{value}' is not N:LABEL")))?;
                    Ok((issue, label.to_string()))
                })
                .collect::<Result<_, Failure>>()?;
            let seed = Seed {
                repository: flags.required("repo")?.to_string(),
                accounts: flags.all("account").into_iter().map(String::from).collect(),
                issues: issue_list(flags.required("issues")?)?,
                closed: flags
                    .optional("closed")?
                    .map_or(Ok(Vec::new()), issue_list)?,
                labels,
            };
            Fixture::create(flags.required("db")?, &seed)?;
        }
        "declare" => {
            let flags = Flags::parse(args, &["db", "account"], false)?;
            let provider =
                FixtureProvider::open(flags.required("db")?, flags.required("account")?)?;
            print_json(&provider.declaration());
        }
        "fault" => {
            let flags = Flags::parse(args, &["db", "tool", "issue", "mode", "count"], false)?;
            let mode = match flags.required("mode")? {
                "lose-response" => FaultMode::LoseResponse,
                "hold" => FaultMode::Hold,
                other => return Err(usage(format!("unknown fault mode '{other}'"))),
            };
            let issue = flags
                .number("issue")?
                .ok_or_else(|| usage("--issue is required"))?;
            let count = flags.number("count")?.unwrap_or(1);
            Fixture::open(flags.required("db")?)?.add_fault(
                flags.required("tool")?,
                issue,
                mode,
                count,
            )?;
        }
        "latency" => {
            let flags = Flags::parse(args, &["db", "ms"], false)?;
            let ms = flags
                .number("ms")?
                .ok_or_else(|| usage("--ms is required"))?;
            Fixture::open(flags.required("db")?)?.set_latency(ms)?;
        }
        "revoke" | "restore" => {
            let flags = Flags::parse(args, &["db", "account"], false)?;
            Fixture::open(flags.required("db")?)?
                .set_revoked(flags.required("account")?, command == "revoke")?;
        }
        "edit" => {
            let flags = Flags::parse(args, &["db", "issue", "add-label", "remove-label"], false)?;
            let issue = flags
                .number("issue")?
                .ok_or_else(|| usage("--issue is required"))?;
            let (label, edit) = match (
                flags.optional("add-label")?,
                flags.optional("remove-label")?,
            ) {
                (Some(label), None) => (label, LabelEdit::Add),
                (None, Some(label)) => (label, LabelEdit::Remove),
                _ => {
                    return Err(usage(
                        "edit takes exactly one of --add-label or --remove-label",
                    ));
                }
            };
            let changed =
                Fixture::open(flags.required("db")?)?.external_label(issue, label, edit)?;
            println!("{}", if changed { "changed" } else { "no change" });
        }
        "ledger" => {
            let flags = Flags::parse(args, &["db"], true)?;
            let ledger = Fixture::open(flags.required("db")?)?.ledger()?;
            if flags.json {
                print_json(&ledger);
            } else {
                print_ledger(&ledger);
            }
        }
        "state" => {
            let flags = Flags::parse(args, &["db"], true)?;
            let state = Fixture::open(flags.required("db")?)?.state()?;
            if flags.json {
                print_json(&state);
            } else {
                print_state(&state);
            }
        }
        "call" => {
            let flags = Flags::parse(
                args,
                &[
                    "db",
                    "account",
                    "tool",
                    "key",
                    "arguments",
                    "preconditions",
                    "scope",
                ],
                false,
            )?;
            let account = flags.required("account")?;
            let provider = FixtureProvider::open(flags.required("db")?, account)?;
            let scope = flags.json_value("scope")?.map(scope).transpose()?;
            let arguments = flags
                .json_value("arguments")?
                .ok_or_else(|| usage("--arguments is required"))?;
            let preconditions = match flags.json_value("preconditions")? {
                None => Vec::new(),
                Some(Value::Array(items)) => items,
                Some(_) => return Err(usage("--preconditions must be a JSON array")),
            };
            let mut operation =
                operation(account, flags.required("tool")?, arguments, preconditions);
            // The approved scope, when the caller has one: the provider refuses a
            // scope that is not its own account and repository.
            if let Some(scope) = scope {
                operation.scope = scope;
            }
            let attempt = Attempt {
                operation,
                key: flags.required("key")?.to_string(),
                number: 1,
            };
            match provider.apply(attempt, context()).await {
                Ok(value) => print_json(&json!({"ok": true, "value": value})),
                Err(failure) => {
                    print_json(&json!({
                        "ok": false,
                        "error": {"code": failure.error.code, "message": failure.error.message},
                        "effect": failure.effect,
                    }));
                    return Err(Failure::Tool);
                }
            }
        }
        "authorize" => {
            let flags = Flags::parse(args, &["db", "account", "scope"], false)?;
            let provider =
                FixtureProvider::open(flags.required("db")?, flags.required("account")?)?;
            let scope = scope(
                flags
                    .json_value("scope")?
                    .ok_or_else(|| usage("--scope is required"))?,
            )?;
            match provider.authorize(&scope, context()).await {
                Ok(()) => print_json(&json!({"ok": true})),
                Err(error) => {
                    print_json(&json!({"ok": false, "error": error}));
                    return Err(Failure::Tool);
                }
            }
        }
        "reconcile" => {
            let flags = Flags::parse(
                args,
                &["db", "account", "tool", "arguments", "keys", "scope"],
                false,
            )?;
            let account = flags.required("account")?;
            let provider = FixtureProvider::open(flags.required("db")?, account)?;
            let scope = flags.json_value("scope")?.map(scope).transpose()?;
            let arguments = flags
                .json_value("arguments")?
                .ok_or_else(|| usage("--arguments is required"))?;
            let keys = flags
                .required("keys")?
                .split(',')
                .filter(|key| !key.is_empty())
                .map(String::from)
                .collect();
            let mut operation = operation(account, flags.required("tool")?, arguments, Vec::new());
            if let Some(scope) = scope {
                operation.scope = scope;
            }
            match provider.reconcile(operation, keys, context()).await {
                Ok(found) => {
                    print_json(&json!({"verdict": found.verdict, "evidence": found.evidence}))
                }
                Err(error) => {
                    print_json(&json!({"error": {"code": error.code, "message": error.message}}));
                    return Err(Failure::Tool);
                }
            }
        }
        other => {
            return Err(usage(format!(
                "unknown command '{other}'; use --help for usage"
            )));
        }
    }
    Ok(0)
}

/// Run or resume a journal's approved plan with the fixture as provider.
async fn session(args: &[String]) -> Result<u8, Failure> {
    let (kind, rest) = match args.split_first() {
        Some((kind, rest)) if kind == "run" => (SessionKind::Run, rest),
        Some((kind, rest)) if kind == "resume" => (SessionKind::Resume, rest),
        _ => return Err(usage("session takes run or resume")),
    };
    let flags = Flags::parse(rest, &["journal", "db", "account"], false)?;
    let journal = Journal::open(flags.required("journal")?, Durability::Storage)
        .map_err(|error| Failure::Session(error.to_string()))?;
    let provider = FixtureProvider::open(flags.required("db")?, flags.required("account")?)?;
    let cancellation = RunCancellation::default();
    let signal_task = cancel_on_interrupt(&cancellation);
    let result = run_session(
        journal,
        provider,
        SessionOptions {
            kind,
            ..SessionOptions::default()
        },
        cancellation,
    )
    .await;
    signal_task.abort();
    match result {
        Ok(outcome) => {
            print_json(&outcome);
            Ok(exit_code(&outcome))
        }
        Err(SessionError::Refused { session, reason }) => {
            print_json(&json!({"session": session, "status": "refused", "detail": reason}));
            eprintln!("codemode-fixture: session {session} refused: {reason}");
            Ok(4)
        }
        Err(error) => Err(Failure::Session(error.to_string())),
    }
}

/// The exit status of `codemode plan run|resume` for the same outcome.
fn exit_code(outcome: &SessionOutcome) -> u8 {
    match outcome.status.as_str() {
        "completed" => 0,
        "interrupted" => outcome
            .interruption
            .map_or(1, |status| status.exit_code().max(1)),
        _ => 3,
    }
}

/// Cancel the session on Ctrl-C or SIGTERM, as `codemode plan run` does.
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

/// The last characters of an attempt key, which hold its operation and number.
fn key_tail(key: Option<&str>) -> String {
    let Some(key) = key else {
        return "-".into();
    };
    let count = key.chars().count();
    if count <= 20 {
        key.to_string()
    } else {
        format!("…{}", key.chars().skip(count - 19).collect::<String>())
    }
}

fn print_ledger(ledger: &Ledger) {
    println!(
        "{:>4}  {:<12} {:<20} {:<20} {:>6}  {:<28} effect",
        "seq", "actor", "tool", "key", "issue", "outcome"
    );
    for row in &ledger.rows {
        let issue = row.issue.map_or("-".into(), |issue| issue.to_string());
        println!(
            "{:>4}  {:<12} {:<20} {:<20} {:>6}  {:<28} {}",
            row.seq,
            row.actor,
            row.tool,
            key_tail(row.key.as_deref()),
            issue,
            row.outcome,
            u8::from(row.effect)
        );
    }
    println!("\nChanges:");
    let mut duplicates = 0;
    for change in &ledger.changes {
        let issue = change.issue.map_or("?".into(), |issue| format!("#{issue}"));
        let detail = change.detail.as_deref().unwrap_or("(invalid arguments)");
        let flag = if change.duplicates > 0 {
            format!("  <- {} DUPLICATE effect(s)", change.duplicates)
        } else {
            String::new()
        };
        println!(
            "  {} {issue} \"{detail}\": {} request(s), {} effect(s){flag}",
            change.tool, change.requests, change.effects
        );
        duplicates += change.duplicates;
    }
    println!("\nDuplicate effects: {duplicates}");
}

fn print_state(state: &State) {
    println!(
        "repository {}, latency {} ms",
        state.repository, state.latency_ms
    );
    let accounts: Vec<String> = state
        .accounts
        .iter()
        .map(|account| {
            let status = if account.revoked { "revoked" } else { "active" };
            format!("{} ({status})", account.name)
        })
        .collect();
    println!("accounts: {}", accounts.join(", "));
    for issue in &state.issues {
        println!(
            "#{} {:<6} labels: {}",
            issue.number,
            issue.state,
            if issue.labels.is_empty() {
                "-".into()
            } else {
                issue.labels.join(", ")
            }
        );
        for comment in &issue.comments {
            println!("    comment {}: {}", comment.id, comment.body);
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = match env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<_, _>>()
    {
        Ok(args) => args,
        Err(_) => {
            eprintln!("codemode-fixture: arguments must be valid UTF-8");
            return ExitCode::from(2);
        }
    };
    let Some((command, rest)) = args.split_first() else {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match run(command, rest).await {
        Ok(code) => ExitCode::from(code),
        Err(Failure::Tool) => ExitCode::from(1),
        Err(Failure::Usage(message)) => {
            eprintln!("codemode-fixture: {message}");
            ExitCode::from(2)
        }
        Err(Failure::Fixture(error)) => {
            eprintln!("codemode-fixture: {error}");
            ExitCode::from(2)
        }
        Err(Failure::Session(message)) => {
            eprintln!("codemode-fixture: {message}");
            ExitCode::from(2)
        }
    }
}
