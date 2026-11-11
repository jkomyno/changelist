//! Command-line process I/O for codemode: arguments, files, the terminal, and
//! exit codes. Behavior lives in the library.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;

mod approval;
mod cli;

const HELP: &str = "codemode — many writes, one change list, one approval

A program or a script proposes writes for one MCP server. codemode puts them in
one plan file. A person reads it and approves it once. codemode then sends
exactly those writes, stops at the first failure, and never repeats a write it
cannot show is safe to repeat.

Usage:
  codemode tools   --server <server.json>
  codemode plan    --server <server.json> (--program <file.ts> | --operations <file|->)
  codemode status  <plan file>
  codemode apply   <plan file>
  codemode resolve <plan file> (<id>... | --errors | --no-reply) --as applied|not-applied|skipped
  codemode <command> --help
  codemode --version

Commands:
  tools    List the server's tools and what a plan may do with each one.
  plan     Make a plan file and print its review.
  status   Show a plan: its review before approval, each write's outcome after.
  apply    Ask for approval, then send the writes. Run it again to continue.
  resolve  Record what happened to a write whose outcome codemode does not know,
           or skip a write that must not be sent.

Example:
  codemode plan --server server.json --program triage.ts
  codemode apply triage.plan.json

Exit codes:
  0  Every write is applied or skipped, or there is nothing to change, or a
     command that only reads succeeded.
  2  Usage error, unreadable file, invalid plan, or a server or program failure
     before any write.
  3  The plan needs attention: a write failed, has no answer, or was refused,
     or the session stopped with writes left to send.
  4  Not approved.
";

#[derive(Debug)]
pub(crate) struct CliError(pub(crate) String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

async fn execute() -> Result<u8, Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let command = args.next();
    let rest: Vec<OsString> = args.collect();
    let command = command.as_deref().and_then(|command| command.to_str());
    let help = |text: &str| -> Result<u8, Box<dyn Error>> {
        io::stdout().write_all(text.as_bytes())?;
        Ok(0)
    };
    let asks_help = rest.iter().any(|arg| arg == "-h" || arg == "--help");
    match command {
        None | Some("-h" | "--help") => help(HELP),
        Some("-V" | "--version") => {
            println!("codemode {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("tools") if asks_help => help(cli::TOOLS_HELP),
        Some("plan") if asks_help => help(cli::PLAN_HELP),
        Some("status") if asks_help => help(cli::STATUS_HELP),
        Some("apply") if asks_help => help(cli::APPLY_HELP),
        Some("resolve") if asks_help => help(cli::RESOLVE_HELP),
        Some("tools") => cli::tools(rest).await,
        Some("plan") => cli::plan(rest).await,
        Some("status") => cli::status(rest),
        Some("apply") => cli::apply(rest).await,
        Some("resolve") => cli::resolve(rest),
        Some(other) => Err(CliError(format!(
            "unknown command '{other}'; the commands are tools, plan, status, apply, and \
             resolve; use --help"
        ))
        .into()),
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
            // An error can quote a plan, a program, or a server.
            eprintln!("codemode: {}", codemode::render::escape(&error.to_string()));
            2
        }
    };
    // Do not let a blocking thread that cannot be interrupted delay process exit.
    runtime.shutdown_background();
    ExitCode::from(code)
}
