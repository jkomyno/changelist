#!/bin/sh
# Live, read-only preflight of a plan through the logged-in Composio CLI: check
# each operation's scope, the account's permission, labels, preconditions, and
# whether the change already holds. Nothing is written. Usage:
#   COMPOSIO_GITHUB_ACCOUNT=<ca_...> run-preflight.sh <owner/repo> <plan.json>
set -eu

[ $# -eq 2 ] || { echo "usage: $0 <owner/repo> <plan.json>" >&2; exit 2; }
plan=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
command -v composio >/dev/null || { echo "composio CLI not found; see https://docs.composio.dev/docs/cli" >&2; exit 2; }
account=${COMPOSIO_GITHUB_ACCOUNT:-}
[ -n "$account" ] || { echo "set COMPOSIO_GITHUB_ACCOUNT to a connected-account id (ca_...); see composio connections list" >&2; exit 2; }
composio --version
mise exec -- cargo build --quiet --locked
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio-preflight.XXXXXX")
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT
TMPDIR=$work composio run --file integrations/composio/host.ts -- preflight \
  --account "$account" --repo "$1" --plan "$plan" --bin target/debug/codemode </dev/null
