#!/bin/sh
# Recoverable agent actions, offline: review a batch of external changes,
# execute it, kill the executor, and continue safely.
#
#   mise run recovery-demo            # or: examples/recovery/demo.sh [DIR]
#
# The "remote" is codemode-fixture, an offline issue tracker in its own SQLite
# file with a ledger of every request and effect. No account or network is used.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
mise exec -- cargo build --quiet --release --locked --bin codemode --bin codemode-fixture
codemode="$root/target/release/codemode"
fixture="$root/target/release/codemode-fixture"
work=${1:-$(mktemp -d "${TMPDIR:-/tmp}/codemode-recovery.XXXXXX")}
mkdir -p "$work"
rm -f "$work/fixture.db" "$work/fixture.db-wal" "$work/fixture.db-shm" "$work/journal.db" "$work/journal.db-wal"
plan="$root/examples/recovery/plan.json"
db="$work/fixture.db"
journal="$work/journal.db"

step() { printf '\n== %s\n\n' "$1"; }
show() { printf '$ %s\n' "$*" | sed "s|$root/||g; s|$work|\$WORK|g"; "$@"; }

step "1. An issue tracker (offline) and a plan: ten label changes and one comment"
show "$fixture" init --db "$db" --repo acme/widgets --account triage-bot \
  --issues 101-110 --label 103:needs-info --label 107:needs-info
"$fixture" declare --db "$db" --account triage-bot > "$work/provider.json"
printf 'The provider declares how each tool can be recovered:\n'
cat "$work/provider.json"; printf '\n'

step "2. Preview the plan, then approve exactly that digest"
show "$codemode" plan check "$plan" --provider "$work/provider.json"
digest=$("$codemode" plan check "$plan" --json | sed -n 's/.*"digest":"\(sha256:[0-9a-f]*\)".*/\1/p')
show "$codemode" plan approve "$plan" --journal "$journal" --provider "$work/provider.json" \
  --digest "$digest" --approver demo --concurrency 4

step "3. Execute four at a time; some responses will be lost, then the executor is killed"
"$fixture" latency --db "$db" --ms 25
# Effects that happen but whose responses never arrive, and one request held before its effect.
"$fixture" fault --db "$db" --tool issues.add_label --issue 104 --mode lose-response
"$fixture" fault --db "$db" --tool issues.remove_label --issue 107 --mode lose-response
"$fixture" fault --db "$db" --tool issues.add_comment --issue 105 --mode lose-response
"$fixture" fault --db "$db" --tool issues.add_label --issue 109 --mode hold
printf '$ codemode plan run --journal $WORK/journal.db --fixture $WORK/fixture.db --account triage-bot &\n'
"$codemode" plan run --journal "$journal" --fixture "$db" --account triage-bot \
  > "$work/run.json" 2>&1 &
pid=$!
tries=0
crash_ready() {
  ledger=$("$fixture" ledger --db "$db" --json)
  printf '%s\n' "$ledger" | grep -q '"seq":10,' &&
    printf '%s\n' "$ledger" | grep -q '"outcome":"held"'
}
until crash_ready; do
  tries=$((tries + 1))
  if [ "$tries" -gt 400 ]; then echo "the ten-request crash checkpoint never arrived" >&2; kill -9 "$pid"; exit 1; fi
  sleep 0.05
done
kill -9 "$pid"
wait "$pid" 2>/dev/null || true
printf 'killed the executor (SIGKILL) with four requests in flight\n'

step "4. After the kill: what does the journal know?"
show "$codemode" plan inspect --journal "$journal"

step "5. Restart: reconcile what may have happened, then continue only what is safe"
status=0
printf '$ codemode plan resume --journal $WORK/journal.db --fixture $WORK/fixture.db --account triage-bot\n'
"$codemode" plan resume --journal "$journal" --fixture "$db" --account triage-bot \
  > "$work/resume.json" || status=$?
printf 'resume exited %s (3: a person must review something)\n' "$status"
show "$codemode" plan inspect --journal "$journal"

step "6. The provider's own ledger: requests and effects, independent of codemode"
show "$fixture" ledger --db "$db"

step "7. A person checks issue 105, finds the comment once, and records it"
printf '$ codemode-fixture state --db $WORK/fixture.db\n'
"$fixture" state --db "$db" | sed -n '/^#105/,/^#106/p' | sed '$d'
show "$codemode" plan review --journal "$journal" --operation comment-105 --as applied \
  --reviewer demo --note "comment present once on issue 105"
printf '$ codemode plan resume --journal $WORK/journal.db --fixture $WORK/fixture.db --account triage-bot\n'
"$codemode" plan resume --journal "$journal" --fixture "$db" --account triage-bot \
  > "$work/final.json"
show "$codemode" plan inspect --journal "$journal"
printf '\nFiles are in %s; the dispatch trace of a session: codemode plan trace --journal %s --session 2\n' \
  "$work" "$journal"
