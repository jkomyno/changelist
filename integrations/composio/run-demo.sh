#!/bin/sh
# Run the offline Composio example from the repository root. Fake results stand in
# for the `composio run` helper; no account, credential, or network is used.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --locked
deno=$(mise which deno)
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio.XXXXXX")
trap 'rm -rf "$work"' EXIT

status=0
"$deno" run --allow-run --allow-read --allow-write --allow-env integrations/composio/testing/offline-host.ts \
  --program "${1:-integrations/composio/demo/workflow.ts}" \
  --tools GITHUB_GET_THE_AUTHENTICATED_USER,SLACK_TEST_AUTH,GITHUB_GET_A_REPOSITORY \
  --bin target/debug/codemode --trace "$work/trace.json" || status=$?
[ -f "$work/trace.json" ] || exit "$status"

echo "--- call outcomes recorded by Rust"
"$deno" eval 'const r = JSON.parse(Deno.readTextFileSync(Deno.args[0])); for (const c of r.calls) console.log([c.call.id, c.call.tool, c.outcome.status, c.outcome.error?.code, `${c.started_ms ?? "-"}..${c.finished_ms}ms`].filter(Boolean).join(" "))' "$work/trace.json"
exit "$status"
