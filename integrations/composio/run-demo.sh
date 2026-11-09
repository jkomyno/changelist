#!/bin/sh
# Run the GitHub triage workflow offline from the repository root: investigate
# with examples/triage.ts, review, approve, run, and inspect. A stand-in for the
# `composio run` helper keeps a JSON ledger of GitHub state; no account,
# credential, or network is used.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --locked
deno=$(mise which deno)
bin="$root/target/debug/codemode"
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-composio-demo.XXXXXX")
trap 'rm -rf "$work"' EXIT

cat > "$work/github.json" <<'JSON'
{"accounts": {"ca_demo_account": "demo-bot"}, "faults": {}, "calls": [],
 "repositories": {"acme/widgets": {"id": 1001, "labels": ["bug", "documentation"],
   "permissions": {"push": true, "triage": true},
   "issues": {
     "1": {"title": "CI fails on main", "state": "open", "labels": [], "comments": []},
     "2": {"title": "Docs typo", "state": "open", "labels": ["documentation"], "comments": []},
     "3": {"title": "CI fails on main!", "state": "open", "labels": [], "comments": []}},
   "runs": [{"id": 77, "name": "CI", "conclusion": "failure", "head_sha": "9359a12c0ffee",
             "jobs": [{"id": 5, "name": "check", "conclusion": "failure"}]}]}}}
JSON
host() {
  "$deno" run --allow-run --allow-read --allow-write --allow-env integrations/composio/testing/host.ts "$@" \
    --account ca_demo_account --repo acme/widgets --offline "$work/github.json" --bin "$bin"
}

echo "--- investigate (read-only)"
host investigate --program integrations/composio/examples/triage.ts --out "$work/plan.json" --trace "$work/trace.json"
echo "--- review: codemode plan check"
host declare > "$work/declaration.json"
"$bin" plan check "$work/plan.json" --provider "$work/declaration.json"
digest=$("$bin" plan check "$work/plan.json" --json | "$deno" eval 'console.log(JSON.parse(await new Response(Deno.stdin.readable).text()).digest)')
"$bin" plan approve "$work/plan.json" --journal "$work/journal.db" --provider "$work/declaration.json" \
  --digest "$digest" --approver demo
echo "--- preflight, run, inspect"
host preflight --plan "$work/plan.json"
host run --declaration "$work/declaration.json" --journal "$work/journal.db"
"$bin" plan inspect --journal "$work/journal.db"
