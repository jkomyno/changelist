//! The offline dispatch evaluation runs end to end on every fixture.

use std::process::Command;

use serde_json::Value;

#[test]
fn evaluation_checks_every_fixture_under_every_policy() {
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/eval/fixtures.json");
    let deno = std::env::var_os("CODEMODE_TEST_DENO").unwrap_or_else(|| "deno".into());
    let output = Command::new(env!("CARGO_BIN_EXE_codemode-eval"))
        .args(["--reps", "1", "--warmup", "1", "--fixtures", manifest_path])
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

    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(manifest_path).expect("manifest is readable"),
    )
    .expect("manifest is JSON");
    let fixtures = manifest["fixtures"].as_array().expect("fixture list");
    assert!(!fixtures.is_empty());
    for fixture in fixtures {
        let name = fixture["name"].as_str().expect("fixture name");
        for policy in ["sequential", "batch", "rolling"] {
            assert!(
                summary.contains(&format!("| {name} | {policy} |")),
                "summary lacks {name} / {policy}:\n{summary}"
            );
        }
    }
}
