#!/bin/sh
# Live, read-only check through the logged-in Composio CLI. It executes two identity
# reads (GitHub, Slack) and one GitHub call with missing arguments that the CLI
# rejects locally; a fourth slug is refused by Rust without reaching Composio.
# Needs `composio login` and active GitHub and Slack connections. The trace stays in
# a temporary directory because it contains account data.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
command -v composio >/dev/null || { echo "composio CLI not found; see https://docs.composio.dev/docs/cli" >&2; exit 2; }
composio --version
mise exec -- cargo build --quiet --locked
deno=$(mise which deno)
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio-live.XXXXXX")
trap 'rm -rf "$work"' EXIT

status=0
expected='{"environment":"denied","github":true,"refused":["composio_tool_failed","tool_denied"],"slack":true}'
composio run --file integrations/composio/host.ts -- \
  --program integrations/composio/demo/workflow.ts \
  --tools GITHUB_GET_THE_AUTHENTICATED_USER,SLACK_TEST_AUTH,GITHUB_GET_A_REPOSITORY \
  --bin target/debug/codemode --deno "$deno" --trace "$work/trace.json" </dev/null \
  >"$work/summary.json" 2>"$work/stderr" || status=$?
grep -v '^RUN_LOG_FILE=' "$work/stderr" >&2 || true
cat "$work/summary.json"
[ -f "$work/trace.json" ] || { [ "$status" -ne 0 ] || status=1; exit "$status"; }

echo "--- call outcomes recorded by Rust"
"$deno" eval 'const r = JSON.parse(Deno.readTextFileSync(Deno.args[0])); for (const c of r.calls) console.log([c.call.id, c.call.tool, c.outcome.status, c.outcome.error?.code, `${c.started_ms ?? "-"}..${c.finished_ms}ms`].filter(Boolean).join(" "))' "$work/trace.json"
echo "--- credential containment (values are never printed)"
"$deno" run --allow-read --allow-env integrations/composio/demo/credential-check.ts \
  "$work/trace.json" "$work/summary.json" "$work/stderr" || status=1
echo "--- expected results"
"$deno" eval '
const summary = JSON.parse(Deno.readTextFileSync(Deno.args[0]));
const trace = JSON.parse(Deno.readTextFileSync(Deno.args[1]));
const [github, slack] = trace.calls;
const checks = {
  status: summary.status === "success",
  output: JSON.stringify(summary.output) === Deno.args[2],
  overlap: github.started_ms < slack.finished_ms && slack.started_ms < github.finished_ms,
};
for (const [name, ok] of Object.entries(checks)) console.log(`${name}: ${ok ? "ok" : "FAILED"}`);
if (!Object.values(checks).every(Boolean)) Deno.exit(1);
' "$work/summary.json" "$work/trace.json" "$expected" || status=1
exit "$status"
