//! The offline recovery evaluation runs end to end: a real killed run, three
//! recovery strategies, and the no-fault overhead comparison.

use std::process::Command;

#[test]
fn recovery_evaluation_checks_every_strategy_and_mode() {
    let deno = std::env::var_os("CODEMODE_TEST_DENO").unwrap_or_else(|| "deno".into());
    let output = Command::new(env!("CARGO_BIN_EXE_codemode-recovery-eval"))
        .args([
            "--reps",
            "1",
            "--warmup",
            "0",
            "--latency",
            "0",
            "--large",
            "0",
        ])
        .arg("--bin")
        .arg(env!("CARGO_BIN_EXE_codemode"))
        .arg("--deno")
        .arg(deno)
        .output()
        .expect("evaluation binary starts");
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{summary}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        summary.contains("All correctness checks passed."),
        "{summary}"
    );
    // Stop sends nothing; blind retry duplicates the comment; codemode leaves one review.
    for row in [
        "| 0 ms | stop on uncertainty | 0/1 | 0/1 | 5 | 0 | 0 | 0 |",
        "| 0 ms | blind retry | 0/1 | 1/1 | 4 | 5 | 1 | 3 |",
        "| 0 ms | codemode resume | 1/1 | 0/1 | 1 | 2 | 0 | 1 |",
    ] {
        assert!(summary.contains(row), "summary lacks {row}:\n{summary}");
    }
    for mode in ["runtime only", "journal, storage", "journal, process"] {
        assert!(
            summary.contains(&format!("| recovery-plan | 11 | 0 ms | {mode} | 1/1 |")),
            "summary lacks {mode}:\n{summary}"
        );
    }
}
