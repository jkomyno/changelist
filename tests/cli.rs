//! The command line without a server: the help, the commands that exist, and
//! usage errors.

use std::process::{Command, Output, Stdio};

fn codemode(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codemode"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("CLI starts")
}

fn help(args: &[&str]) -> String {
    let output = codemode(args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).expect("UTF-8")
}

const COMMANDS: [&str; 5] = ["tools", "plan", "status", "apply", "resolve"];

#[test]
fn the_help_shows_exactly_the_five_commands_and_the_exit_codes_once() {
    let main = help(&["--help"]);
    assert_eq!(main, help(&[]), "no arguments prints the help");
    assert_eq!(main, help(&["-h"]));
    let usage: Vec<&str> = main
        .lines()
        .skip_while(|line| *line != "Usage:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .collect();
    assert_eq!(
        usage,
        [
            "  codemode tools   --server <server.json>",
            "  codemode plan    --server <server.json> (--program <file.ts> | --operations <file|->)",
            "  codemode status  <plan file>",
            "  codemode apply   <plan file>",
            "  codemode resolve <plan file> (<id>... | --errors | --no-reply) --as applied|not-applied",
            "  codemode <command> --help",
            "  codemode --version",
        ]
    );
    assert_eq!(main.matches("Exit codes:").count(), 1);
    for code in ["\n  0  ", "\n  2  ", "\n  3  ", "\n  4  Not approved."] {
        assert!(main.contains(code), "{code:?} in {main}");
    }
    assert_eq!(main.matches("Example:").count(), 1);

    for command in COMMANDS {
        let text = help(&[command, "--help"]);
        assert_eq!(text, help(&[command, "-h"]));
        assert!(
            text.starts_with(&format!("codemode {command} — ")),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nUsage:\n  codemode {command} ")),
            "{text}"
        );
        assert_eq!(
            text.matches("Example:").count(),
            1,
            "{command}: one example"
        );
        assert!(
            !text.contains("Exit code"),
            "{command}: the exit codes are in the main help"
        );
        assert!(
            text.lines().count() <= 40,
            "{command}: the help stays short"
        );
    }
    // The help speaks of writes, tools, and servers, and names no removed command.
    let all: String = COMMANDS
        .iter()
        .map(|command| help(&[command, "--help"]))
        .chain([main])
        .collect();
    for gone in [
        "codemode mcp",
        "codemode inspect",
        "investigate",
        "declare",
        "resume",
        "plan check",
        "plan approve",
        "plan review",
        "plan trace",
        "bridge",
        "probe",
        "reconcile",
        "contract",
        "binding",
        "scope",
        "provider",
        "--journal",
    ] {
        assert!(!all.contains(gone), "the help mentions {gone:?}");
    }
    assert!(all.lines().all(|line| line.chars().count() <= 100));
}

#[test]
fn the_version_is_printed() {
    for flag in ["--version", "-V"] {
        assert_eq!(
            help(&[flag]),
            format!("codemode {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn removed_commands_and_options_are_usage_errors() {
    let cases: &[(&[&str], &str)] = &[
        (&["mcp", "tools", "--server", "x"], "unknown command 'mcp'"),
        (&["inspect", "trace.json"], "unknown command 'inspect'"),
        (&["run", "--code", "-"], "unknown command 'run'"),
        (&["bridge"], "unknown command 'bridge'"),
        (&["investigate"], "unknown command 'investigate'"),
        (
            &["plan", "check", "--server", "x", "--operations", "y"],
            "unexpected argument 'check'",
        ),
        (
            &["plan", "--provider", "declaration.json"],
            "unknown option '--provider'",
        ),
        (
            &["plan", "--server", "x"],
            "give exactly one of --program and --operations",
        ),
        (
            &["apply", "p.json", "--bridge"],
            "unknown option '--bridge'",
        ),
        (
            &["apply", "p.json", "--max-probes", "1"],
            "unknown option '--max-probes'",
        ),
        (
            &["apply", "p.json", "--digest", "sha256:0"],
            "unknown option '--digest'",
        ),
        (
            &["status", "p.json", "--messages"],
            "unknown option '--messages'",
        ),
        (
            &["resolve", "p.json", "--operation", "a"],
            "unknown option '--operation'",
        ),
        (
            &["apply", "p.json", "--journal", "elsewhere.db"],
            "unknown option '--journal'",
        ),
        (
            &["status", "p.json", "--journal", "elsewhere.db"],
            "unknown option '--journal'",
        ),
        (
            &[
                "resolve",
                "p.json",
                "a",
                "--as",
                "applied",
                "--journal",
                "x.db",
            ],
            "unknown option '--journal'",
        ),
        (&["tools"], "--server is required"),
        (&["apply"], "apply takes one plan file"),
        (&["apply", "a.json", "b.json"], "apply takes one plan file"),
        (&["status"], "status takes one plan file"),
        (&["resolve"], "a plan file is required"),
        (
            &["apply", "p.json", "--approve"],
            "missing value for --approve",
        ),
        (
            &["apply", "p.json", "--json", "--json"],
            "--json can only be supplied once",
        ),
    ];
    for (args, expected) in cases {
        let output = codemode(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}
