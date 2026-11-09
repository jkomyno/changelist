//! Command-line process I/O for the codemode runtime.

use std::env;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use codemode::{
    BridgeError, InspectOptions, RunCancellation, RunConfig, RunReport, Sections, inspect,
    serve_bridge,
};

mod plan_cli;

const MAX_TRACE_BYTES: u64 = 64 * 1024 * 1024;
const HELP: &str = "codemode — bounded agent-written TypeScript/JavaScript workflows

Usage:
  codemode bridge [options]
  codemode inspect <trace.json|-> [inspect options]
  codemode plan <check|approve|run|resume|inspect|review|trace> ...  (see codemode plan --help)

Options:
  -h, --help             Show this help
  -V, --version          Show the version

bridge lets a parent host execute tools: stdin and stdout carry protocol
version 2 frames (start, call, result, fact, cancel, report). The host's start
frame supplies the source and authorized tool names. Credentials stay with the host.
Each program runs in a fresh V8 worker process started from this executable.
  --timeout-ms <n>        Total run deadline, including startup (default: 5000)
  --max-calls <n>         Maximum helper-call attempts (default: 32)
  --concurrency <n>       Maximum active provider calls (default: 4)
  --max-output-bytes <n>  Cumulative JSON response/output bytes (default: 262144)

inspect reads a saved execution record and answers: why was this slow, which
calls ran, and which outcomes require reconciliation. It prints tool names,
error codes, argument keys, and sizes, never argument or result values.
  --why-slow, --calls, --reconcile   Print only these sections (default: all)
  --limit <n>             Calls listed per section (default: 10)
  --messages              Also print error messages and the diagnostic
  --json                  Print the answers as one JSON object
";

#[derive(Debug)]
pub(crate) struct CliError(pub(crate) String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

#[derive(Debug)]
struct InspectArgs {
    trace: PathBuf,
    options: InspectOptions,
    sections: Sections,
    json: bool,
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    Bridge(RunConfig),
    Inspect(InspectArgs),
    Plan(Vec<std::ffi::OsString>),
}

fn parse_inspect(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Command, CliError> {
    let mut trace = None;
    let mut options = InspectOptions::default();
    let mut chosen = Sections {
        timing: false,
        calls: false,
        reconcile: false,
    };
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("--why-slow") => chosen.timing = true,
            Some("--calls") => chosen.calls = true,
            Some("--reconcile") => chosen.reconcile = true,
            Some("--messages") => options.messages = true,
            Some("--json") => json = true,
            Some("--limit") => {
                options.limit = args
                    .next()
                    .and_then(|value| value.to_str()?.parse().ok())
                    .ok_or_else(|| CliError("--limit requires a nonnegative integer".into()))?;
            }
            Some(flag) if flag.starts_with("--") => {
                return Err(CliError(format!("unknown option '{flag}'")));
            }
            _ => {
                if trace.replace(PathBuf::from(arg)).is_some() {
                    return Err(CliError("inspect takes one trace file".into()));
                }
            }
        }
    }
    let none = !(chosen.timing || chosen.calls || chosen.reconcile);
    Ok(Command::Inspect(InspectArgs {
        trace: trace.ok_or_else(|| CliError("inspect requires a trace file or -".into()))?,
        options,
        sections: if none { Sections::ALL } else { chosen },
        json,
    }))
}

fn parse_arguments() -> Result<Command, CliError> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        None | Some("-h" | "--help") => return Ok(Command::Help),
        Some("-V" | "--version") => return Ok(Command::Version),
        Some("bridge") => {}
        Some("inspect") => return parse_inspect(args),
        Some("plan") => return Ok(Command::Plan(args.collect())),
        Some(other) => {
            return Err(CliError(format!(
                "unknown command '{other}'; expected 'bridge', 'inspect', or 'plan'; use --help for usage"
            )));
        }
    }

    let mut config = RunConfig {
        worker: Some(env::current_exe().map_err(|error| {
            CliError(format!(
                "cannot locate this executable for the worker: {error}"
            ))
        })?),
        ..RunConfig::default()
    };
    while let Some(flag) = args.next() {
        let flag = flag
            .to_str()
            .ok_or_else(|| CliError("option names must be valid UTF-8".into()))?;
        if matches!(flag, "-h" | "--help") {
            return Ok(Command::Help);
        }
        if !matches!(
            flag,
            "--timeout-ms" | "--max-calls" | "--concurrency" | "--max-output-bytes"
        ) {
            return Err(CliError(format!("unknown option '{flag}'")));
        }
        let value = args
            .next()
            .ok_or_else(|| CliError(format!("missing value for {flag}")))?;
        let number = value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| CliError(format!("{flag} requires a nonnegative integer")))?;
        if flag == "--timeout-ms" {
            config.timeout_ms = number;
            continue;
        }
        let number = usize::try_from(number)
            .map_err(|_| CliError(format!("{flag} is too large on this platform")))?;
        match flag {
            "--max-calls" => config.max_calls = number,
            "--concurrency" => config.max_concurrency = number,
            "--max-output-bytes" => config.max_output_bytes = number,
            _ => unreachable!("numeric flags were checked above"),
        }
    }
    Ok(Command::Bridge(config))
}

/// Read a file or stdin (`-`), refusing more than `limit` bytes.
pub(crate) fn read_input(path: &PathBuf, limit: u64, what: &str) -> Result<String, Box<dyn Error>> {
    let input: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path)?)
    };
    let mut text = String::new();
    input.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(CliError(format!("{what} exceeds the {} KiB limit", limit / 1024)).into());
    }
    Ok(text)
}

/// Cancel on Ctrl-C and, on Unix, on SIGTERM, so a host that signals its whole
/// process group still receives a report.
pub(crate) fn cancel_on_interrupt(cancellation: &RunCancellation) -> tokio::task::JoinHandle<()> {
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

async fn execute() -> Result<u8, Box<dyn Error>> {
    match parse_arguments()? {
        Command::Help => {
            io::stdout().write_all(HELP.as_bytes())?;
            Ok(0)
        }
        Command::Version => {
            println!("codemode {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Command::Plan(args) => plan_cli::execute(args).await,
        Command::Inspect(args) => {
            let text = read_input(&args.trace, MAX_TRACE_BYTES, "trace")?;
            let report: RunReport = serde_json::from_str(&text)
                .map_err(|error| CliError(format!("not an execution record: {error}")))?;
            let inspection = inspect(&report, &args.options);
            let mut stdout = io::stdout().lock();
            if args.json {
                serde_json::to_writer(&mut stdout, &inspection)?;
                writeln!(stdout)?;
            } else {
                stdout.write_all(inspection.render(args.sections).as_bytes())?;
            }
            Ok(0)
        }
        Command::Bridge(config) => {
            let cancellation = RunCancellation::default();
            let signal_task = cancel_on_interrupt(&cancellation);
            let result: Result<RunReport, BridgeError> = serve_bridge(
                tokio::io::stdin(),
                tokio::io::stdout(),
                config,
                cancellation,
            )
            .await;
            signal_task.abort();
            Ok(result?.status.exit_code())
        }
    }
}

fn main() -> ExitCode {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-embedded-guest")) {
        return match codemode::run_embedded_worker() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("codemode: {error}");
                ExitCode::from(1)
            }
        };
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("codemode: cannot start the async runtime: {error}");
            return ExitCode::from(2);
        }
    };
    let code = match runtime.block_on(execute()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("codemode: {error}");
            2
        }
    };
    // Tokio reads stdin on a blocking thread that cannot be interrupted; do not
    // let a host that keeps the bridge input open delay process exit.
    runtime.shutdown_background();
    ExitCode::from(code)
}
