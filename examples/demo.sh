#!/bin/sh
# The whole flow, offline: list a server's tools, make a plan, read its review,
# approve and apply it, then see a failed write stop a second plan and finish
# it by hand.
#
#   mise run demo            # or: examples/demo.sh
#
# The server is codemode-mcp-fixture, an offline MCP server with a few
# note-taking tools. No account or network is used. Everything happens in a
# temporary directory that is removed at the end.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --locked --features testkit --bin codemode --bin codemode-mcp-fixture
bin="$root/target/debug"
work=$(mktemp -d "${TMPDIR:-/tmp}/codemode-demo.XXXXXX")
server_pid=
cleanup() {
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
cd "$work"

step() { printf '\n== %s\n\n' "$1"; }
# Run `codemode` with the given arguments and require the exit code in $1.
expect() {
  want=$1; shift
  printf '$ codemode %s\n' "$*"
  code=0
  "$bin/codemode" "$@" </dev/null || code=$?
  if [ "$code" -ne "$want" ]; then
    echo "demo: expected exit code $want, got $code" >&2
    exit 1
  fi
}
digest_of() {
  "$bin/codemode" status "$1" --json | sed -n 's/^{"file":"[^"]*","digest":"\(sha256:[0-9a-f]*\)".*/\1/p'
}

# One note to start with. The first call of notes.append will fail.
printf '{"notes":{"a":"alpha","b":"beta"}}\n' > state.json
"$bin/codemode-mcp-fixture" --http server.addr --state state.json \
  --fault notes.append=is-error 2>/dev/null &
server_pid=$!
tries=0
until [ -s server.addr ]; do
  tries=$((tries + 1))
  if [ "$tries" -gt 200 ]; then echo "demo: the server did not start" >&2; exit 1; fi
  sleep 0.05
done
printf '{"url": "%s", "trust_annotations": true}\n' "$(cat server.addr)" > server.json

step "1. The server's tools, and what a plan may do with each"
expect 0 tools --server server.json

step "2. A program reads the notes and proposes one write for each"
cat > mark.ts <<'PROGRAM'
const { notes } = await tools.call("notes.list", {});
return {
  summary: "Mark every note as reviewed",
  operations: notes.map((note) => ({
    id: `mark-${note.id}`,
    tool: "notes.put",
    arguments: { id: note.id, text: `${note.text} (reviewed)` },
  })),
};
PROGRAM
printf '$ cat mark.ts\n'; cat mark.ts; printf '\n'
expect 0 plan --server server.json --program mark.ts

step "3. The plan is one file. Without a terminal and without a digest, nothing is sent"
printf '$ cat mark.plan.json\n'; cat mark.plan.json; printf '\n'
expect 4 apply mark.plan.json

step "4. Approve exactly that plan by its digest, and apply it"
expect 0 apply mark.plan.json --approve "$(digest_of mark.plan.json)" --approver demo

step "5. A second plan, from a list of operations. Its second write will fail"
cat > tidy.json <<'OPERATIONS'
{
  "summary": "Tidy the notes",
  "operations": [
    { "id": "put-c", "tool": "notes.put", "arguments": { "id": "c", "text": "gamma" } },
    { "id": "log", "tool": "notes.append", "arguments": { "id": "log", "text": "tidied" } },
    { "id": "put-d", "tool": "notes.put", "arguments": { "id": "d", "text": "delta" } },
    { "id": "drop-b", "tool": "notes.delete", "arguments": { "id": "b" } }
  ]
}
OPERATIONS
expect 0 plan --server server.json --operations tidy.json
expect 3 apply tidy.plan.json --approve "$(digest_of tidy.plan.json)" --approver demo --concurrency 1

step "6. The session stopped at the failure. A person checks, records what happened, and continues"
expect 0 status tidy.plan.json
expect 0 resolve tidy.plan.json log --as not-applied --reviewer demo
expect 0 apply tidy.plan.json

step "7. The notes, as the server holds them now"
cat state.json; printf '\n'
