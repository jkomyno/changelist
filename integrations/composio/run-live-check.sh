#!/bin/sh
# Live, read-only investigation through the logged-in Composio CLI: run
# examples/triage.ts against one repository (default jkomyno/codemode), then
# check any proposed plan. Nothing is written to GitHub. Usage:
#   COMPOSIO_GITHUB_ACCOUNT=<ca_...> run-live-check.sh [owner/repo]
# The account is the connected-account id from `composio connections list`.
# The plan and trace stay in a private temporary directory removed on exit; the
# trace is checked for the CLI credential without printing it.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
command -v composio >/dev/null || { echo "composio CLI not found; see https://docs.composio.dev/docs/cli" >&2; exit 2; }
account=${COMPOSIO_GITHUB_ACCOUNT:-}
[ -n "$account" ] || { echo "set COMPOSIO_GITHUB_ACCOUNT to a connected-account id (ca_...); see composio connections list" >&2; exit 2; }
composio --version
mise exec -- cargo build --quiet --locked
deno=$(mise which deno)
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio-live.XXXXXX")
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT

status=0
TMPDIR=$work composio run --file integrations/composio/host.ts -- investigate \
  --account "$account" --repo "${1:-jkomyno/codemode}" --program integrations/composio/examples/triage.ts \
  --out "$work/plan.json" --trace "$work/trace.json" --bin target/debug/codemode </dev/null \
  >"$work/summary.json" 2>"$work/stderr" || status=$?
grep -v '^RUN_LOG_FILE=' "$work/stderr" >&2 || true
cat "$work/summary.json"
[ "$status" -eq 0 ] || exit "$status"
if [ -f "$work/plan.json" ]; then
  echo "--- the proposed plan, exactly as approval would bind it"
  target/debug/codemode plan check "$work/plan.json"
fi
echo "--- credential containment (values are never printed)"
"$deno" run --allow-read --allow-env integrations/composio/testing/credential-check.ts \
  "$work/trace.json" "$work/summary.json" "$work/stderr"
