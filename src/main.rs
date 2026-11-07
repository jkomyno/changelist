//! Command-line process I/O for the codemode runtime.

use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use codemode::{
    BridgeError, MockProvider, RunCancellation, RunConfig, RunReport, run, serve_bridge,
};

const MAX_CODE_BYTES: u64 = 64 * 1024;
const HELP: &str = "codemode — bounded agent-written TypeScript/JavaScript workflows

Usage:
  codemode run --code <file.ts|file.js|-> [options]
  codemode bridge [limit options]

Options:
  --timeout-ms <n>        Total run deadline, including startup (default: 5000)
  --max-calls <n>         Maximum helper-call attempts (default: 32)
  --concurrency <n>       Maximum active provider calls (default: 4)
  --max-output-bytes <n>  Cumulative JSON response/output bytes (default: 262144)
  --trace <file.json>     Save the full trace; print only status, output, and path (run only)
  --deno <executable>     Deno executable (default: deno from PATH)
  -h, --help             Show this help
  -V, --version          Show the version

run supplies mock.echo, mock.wait, and mock.fail. stdout contains one JSON
execution record; diagnostics use stderr. A dash reads source from stdin.

bridge lets a parent host execute tools: stdin and stdout carry protocol
version 1 frames (start, call, result, cancel, report). The host's start frame
supplies the source and authorized tool names. Credentials stay with the host.
";

#[derive(Debug)]
struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

#[derive(Debug)]
struct Options {
    code: PathBuf,
    trace: Option<PathBuf>,
    config: RunConfig,
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    Run(Options),
    Bridge(RunConfig),
}

fn parse_arguments() -> Result<Command, CliError> {
    let mut args = env::args_os().skip(1);
    let bridge = match args.next().as_deref().and_then(|arg| arg.to_str()) {
        None | Some("-h" | "--help") => return Ok(Command::Help),
        Some("-V" | "--version") => return Ok(Command::Version),
        Some("run") => false,
        Some("bridge") => true,
        _ => {
            return Err(CliError(
                "expected 'run' or 'bridge'; use --help for usage".into(),
            ));
        }
    };

    let mut config = RunConfig::default();
    let mut code = None;
    let mut trace = None;
    while let Some(flag) = args.next() {
        let flag = flag
            .to_str()
            .ok_or_else(|| CliError("option names must be valid UTF-8".into()))?;
        if matches!(flag, "-h" | "--help") {
            return Ok(Command::Help);
        }
        if !matches!(
            flag,
            "--code"
                | "--trace"
                | "--deno"
                | "--timeout-ms"
                | "--max-calls"
                | "--concurrency"
                | "--max-output-bytes"
        ) {
            return Err(CliError(format!("unknown option '{flag}'")));
        }
        if bridge && matches!(flag, "--code" | "--trace") {
            return Err(CliError(format!(
                "{flag} is not accepted by bridge; the host supplies source and receives the report"
            )));
        }
        let value = args
            .next()
            .ok_or_else(|| CliError(format!("missing value for {flag}")))?;
        match flag {
            "--code" => {
                if code.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError("--code can only be supplied once".into()));
                }
            }
            "--trace" => trace = Some(PathBuf::from(value)),
            "--deno" => config.deno = PathBuf::from(value),
            _ => {
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
        }
    }
    if bridge {
        return Ok(Command::Bridge(config));
    }
    Ok(Command::Run(Options {
        code: code.ok_or_else(|| CliError("run requires --code <file|->".into()))?,
        trace,
        config,
    }))
}

fn read_source(path: &PathBuf) -> Result<String, Box<dyn Error>> {
    let input: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path)?)
    };
    let mut source = String::new();
    input.take(MAX_CODE_BYTES + 1).read_to_string(&mut source)?;
    if source.len() as u64 > MAX_CODE_BYTES {
        return Err(CliError("source exceeds the 64 KiB limit".into()).into());
    }
    Ok(source)
}

fn cancel_on_interrupt(cancellation: &RunCancellation) -> tokio::task::JoinHandle<()> {
    let signal = cancellation.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
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
        Command::Run(options) => {
            let source = read_source(&options.code)?;
            let cancellation = RunCancellation::default();
            let signal_task = cancel_on_interrupt(&cancellation);
            let result = run(&source, options.config, MockProvider, cancellation).await;
            signal_task.abort();
            let report = result?;
            let mut json = serde_json::to_vec(&report)?;
            json.push(b'\n');
            if let Some(path) = options.trace {
                fs::write(&path, &json)?;
                let mut summary = serde_json::json!({
                    "status": report.status,
                    "trace": path.to_string_lossy(),
                });
                if let Some(output) = report.output {
                    summary["output"] = output;
                }
                writeln!(io::stdout().lock(), "{summary}")?;
            } else {
                io::stdout().lock().write_all(&json)?;
            }
            Ok(report.status.exit_code())
        }
    }
}

fn main() -> ExitCode {
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
