#!/bin/sh
# Run the offline Pi integration example from the repository root.
# Requires mise; Pi 1.4.0 is fetched by mise on first use. No credentials are used.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --locked
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-pi.XXXXXX")
trap 'rm -rf "$work"' EXIT

PI_CODING_AGENT_DIR="$work/agent" \
CODEMODE_PI_PROGRAM="${1:-integrations/pi/demo/workflow.ts}" \
CODEMODE_PI_AUDIT="$work/audit.jsonl" \
mise exec npm:@earendil-works/pi-coding-agent@1.4.0 -- pi \
  --print --offline --no-session --no-extensions --no-context-files --no-skills \
  --no-prompt-templates --no-themes \
  --extension integrations/pi/codemode-bridge.ts \
  --extension integrations/pi/demo/offline-host.ts \
  --extension integrations/pi/demo/approval-gate.ts \
  --model codemode-demo/scripted \
  --tools rust_codemode,read,ls,bash,write \
  --codemode-tools read,ls,bash \
  --codemode-bin target/debug/codemode \
  --codemode-deno "$(mise which deno)" \
  --codemode-trace "$work/trace.json" \
  "Run the codemode demo workflow." </dev/null

echo "--- Pi approval gate audit"
cat "$work/audit.jsonl"
echo "--- call outcomes recorded by Rust"
node -e 'const r = require(process.argv[1]); for (const c of r.calls) console.log([c.call.id, c.call.tool, c.outcome.status, c.outcome.error?.code].filter(Boolean).join(" "))' "$work/trace.json"
