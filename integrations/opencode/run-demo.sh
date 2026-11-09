#!/bin/sh
# Run the offline OpenCode integration example from the repository root.
# Requires mise; OpenCode 1.19.5 is fetched by mise on first use. No credentials are used.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --locked
opencode=$(mise which --tool aqua:anomalyco/opencode@1.19.5 opencode 2>/dev/null) || {
  mise install --quiet aqua:anomalyco/opencode@1.19.5
  opencode=$(mise which --tool aqua:anomalyco/opencode@1.19.5 opencode)
}
deno=$(mise which deno)
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-opencode.XXXXXX")
trap 'rm -rf "$work"' EXIT
demo="$root/integrations/opencode/demo"
program=$(realpath "${1:-$demo/workflow.ts}")

# Isolate OpenCode from user configuration, state, and the network. The seeded
# config directory stops OpenCode from installing its plugin package from npm.
mkdir -p "$work/home/.config/opencode/node_modules" "$work/project"
echo '{"dependencies":{"@opencode-ai/plugin":"1.19.5"}}' >"$work/home/.config/opencode/package.json"
echo '{"lockfileVersion":3,"packages":{"":{"dependencies":{"@opencode-ai/plugin":"1.19.5"}}}}' \
  >"$work/home/.config/opencode/package-lock.json"
config=$("$deno" eval 'const c = JSON.parse(Deno.readTextFileSync(Deno.args[0])); delete c.$comment; c.plugin = [Deno.args[1]]; console.log(JSON.stringify(c))' \
  "$demo/opencode.json" "$demo/host.ts")

cd "$work/project"
HOME="$work/home" XDG_CONFIG_HOME="$work/home/.config" XDG_DATA_HOME="$work/home/.local/share" \
XDG_STATE_HOME="$work/home/.local/state" XDG_CACHE_HOME="$work/home/.cache" \
OPENCODE_CONFIG_CONTENT="$config" OPENCODE_DISABLE_PROJECT_CONFIG=1 OPENCODE_DISABLE_AUTOUPDATE=1 \
OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_DEFAULT_PLUGINS=1 OPENCODE_DISABLE_LSP_DOWNLOAD=1 \
OPENCODE_DISABLE_EXTERNAL_SKILLS=1 OPENCODE_DISABLE_CLAUDE_CODE=1 OPENCODE_AUTH_CONTENT='{}' OPENCODE_DB=':memory:' \
HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 NO_PROXY=127.0.0.1,localhost \
CODEMODE_OC_PROGRAM="$program" CODEMODE_OC_AUDIT="$work/audit.jsonl" \
CODEMODE_OC_TRACE="$work/trace.json" CODEMODE_OC_BIN="$root/target/debug/codemode" \
CODEMODE_DEMO_TOKEN=demo-inventory-token \
"$opencode" run --model codemode-demo/scripted "Run the codemode demo workflow." </dev/null || true

echo "--- OpenCode permission events and model requests"
cat "$work/audit.jsonl" 2>/dev/null || true
echo "--- call outcomes recorded by Rust"
"$deno" eval 'const r = JSON.parse(Deno.readTextFileSync(Deno.args[0])); console.log("status", r.status); for (const c of r.calls) console.log([c.call.id, c.call.tool, c.outcome.status, c.outcome.error?.code].filter(Boolean).join(" "))' "$work/trace.json"
