#!/bin/sh
# Live, read-only preflight of the Composio plan host through the logged-in
# Composio CLI: for one issue and label, check the scope, the account's
# permission on the repository, that the label exists, and whether the issue has
# it. Nothing is written. Usage:
#   COMPOSIO_GITHUB_ACCOUNT=<ca_...> run-plan-preflight.sh <owner/repo> <issue> <label>
# The account is the stable connected-account id from `composio connections list`;
# there is no default. It is kept in a temporary directory; output names no account.
set -eu

[ $# -eq 3 ] || { echo "usage: $0 <owner/repo> <issue> <label>" >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
command -v composio >/dev/null || { echo "composio CLI not found; see https://docs.composio.dev/docs/cli" >&2; exit 2; }
composio --version
mise exec -- cargo build --quiet --locked
deno=$(mise which deno)
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio-plan.XXXXXX")
trap 'rm -rf "$work"' EXIT

account=${COMPOSIO_GITHUB_ACCOUNT:-}
[ -n "$account" ] || { echo "set COMPOSIO_GITHUB_ACCOUNT to a connected-account id (ca_...); see composio connections list" >&2; exit 2; }

"$deno" eval '
const [work, account, destination, issue, label] = Deno.args;
const [owner, repo] = destination.split("/");
Deno.writeTextFileSync(`${work}/config.json`, JSON.stringify({ account, destinations: [destination] }));
Deno.writeTextFileSync(`${work}/plan.json`, JSON.stringify({ version: 1, operations: [{
  id: "preflight", tool: "GITHUB_ADD_LABELS_TO_AN_ISSUE",
  scope: { provider: "composio.github", account, destination },
  arguments: { owner, repo, issue_number: Number(issue), labels: [label] } }] }));
' "$work" "$account" "$1" "$2" "$3"

status=0
composio run --file integrations/composio/plan-host.ts -- preflight \
  --config "$work/config.json" --plan "$work/plan.json" </dev/null \
  >"$work/out.json" 2>"$work/stderr" || status=$?
grep -v '^RUN_LOG_FILE=' "$work/stderr" >&2 || true
cat "$work/out.json"
exit "$status"
