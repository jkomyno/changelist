//! Shared host-client engine selection through real cleared-environment subprocesses.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn deno() -> PathBuf {
    let name = std::env::var_os("CODEMODE_TEST_DENO").unwrap_or_else(|| "deno".into());
    let path = PathBuf::from(name);
    if path.is_absolute() {
        return path;
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(&path))
        .find(|candidate| candidate.is_file())
        .expect("Deno is on the test PATH")
}

const PROGRAM: &str = r#"
const { pathToFileURL } = await import('node:url');
const { runBridge } = await import(pathToFileURL(Deno.args[3]).href);
const report = await runBridge({
  bin: Deno.args[0], cwd: Deno.args[1], timeoutMs: 5000, concurrency: 2, maxCalls: 1,
  ...(Deno.args[2] === 'embedded-v8' ? { backend: 'embedded-v8' } : {}),
  code: "return await tools.call('host.echo', 42);", tools: ['host.echo'],
  execute: async call => ({ok:true, value:call.arguments}),
});
if (report.status !== 'success' || report.output !== 42) throw new Error('unexpected report');
console.log(report.backend);
"#;

fn run_client(backend: &str, path: &std::ffi::OsStr) {
    let output = Command::new(deno())
        .args(["eval", "--no-config", PROGRAM])
        .arg(env!("CARGO_BIN_EXE_codemode"))
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(backend)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/integrations/shared/bridge-client.ts"
        ))
        .env_clear()
        .env("PATH", path)
        .output()
        .expect("TypeScript host starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        backend.replace('-', "_")
    );
}

#[test]
fn an_omitted_deno_path_is_resolved_in_the_trusted_host() {
    run_client("deno", deno().parent().expect("Deno directory").as_os_str());
}

#[cfg(feature = "embedded-v8")]
#[test]
fn embedded_selection_needs_no_deno_on_the_host_path() {
    run_client("embedded-v8", std::ffi::OsStr::new(""));
}
