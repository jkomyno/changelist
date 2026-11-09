// Checks that the oracle catches what it must: node --test bench/workflow/oracle.test.ts
// The end-to-end counterparts (a blind-retry script run, and every adapter's no-fault ledger)
// are recorded by run.ts under `harness_checks` in each results directory.
import assert from "node:assert/strict";
import { test } from "node:test";
import type { Claims } from "./lib/adapter.ts";
import { ACCOUNT, LOGIN, type LedgerRecord } from "./lib/github-state.ts";
import { compareLatency, EXPECTED_OPS, score } from "./oracle.ts";

/** A ledger in which each listed operation was applied `times` times by the bot. */
function ledger(times: Record<string, number> = {}, skip: string[] = []): LedgerRecord[] {
  const records: LedgerRecord[] = [];
  let seq = 0;
  let call = 0;
  for (const op of EXPECTED_OPS) {
    if (skip.includes(op.id)) continue;
    for (let i = 0; i < (times[op.id] ?? 1); i++) {
      const call_id = `c${++call}`;
      const body = op.id.startsWith("comment-") ? { ...op.args, body: `${op.args.body}\n<!-- test:${op.id} -->` } : op.args;
      records.push({ seq: ++seq, ts: seq, event: "start", call_id, slug: op.slug, args: body, account: ACCOUNT, actor: LOGIN });
      records.push({ seq: ++seq, ts: seq, event: "effect_applied", call_id, slug: op.slug, args: body, actor: LOGIN });
      records.push({ seq: ++seq, ts: seq, event: "terminal", call_id, status: "ok", delivered: true });
    }
  }
  return records;
}

function claims(overrides: Partial<Claims> = {}): Claims {
  return {
    engine: "test",
    attempt: 0,
    run_success: true,
    run_status: "done",
    ops: Object.fromEntries(EXPECTED_OPS.map((op) => [op.id, "applied"])),
    review: [],
    ...overrides,
  };
}

test("a complete, once-each ledger with matching claims is correct", () => {
  const verdict = score({ records: ledger(), expect: "all", claims: [claims()] });
  assert.equal(verdict.final_state, "match");
  assert.equal(verdict.verdict, "correct");
});

test("a blind retry that posts the comment twice is an unsafe duplicate", () => {
  const verdict = score({ records: ledger({ "comment-1": 2 }), expect: "all", claims: [claims()] });
  assert.deepEqual(verdict.unsafe_duplicates, [{ op: "comment-1", effects: 2 }]);
  assert.equal(verdict.verdict, "wrong");
});

test("a repeated label write is redundant, not unsafe", () => {
  const verdict = score({ records: ledger({ "add-label-1": 2 }), expect: "all", claims: [claims()] });
  assert.deepEqual(verdict.redundant_writes, [{ op: "add-label-1", effects: 2 }]);
  assert.equal(verdict.verdict, "correct");
});

test("reported success that the ledger contradicts is failed", () => {
  const verdict = score({ records: ledger({}, ["comment-3"]), expect: "all", claims: [claims()] });
  assert.equal(verdict.final_state, "mismatch");
  assert.ok(verdict.unjustified_success.some((reason) => reason.includes("not in the ledger: comment-3")));
  assert.ok(verdict.unjustified_success.some((reason) => reason.startsWith("comment-3")));
  assert.equal(verdict.verdict, "wrong");
});

test("not applied for an applied operation is an unjustified non-effect claim, at any attempt", () => {
  const early = claims({ run_success: false, ops: { "comment-1": "not_applied" } });
  const verdict = score({ records: ledger(), expect: "all", claims: [early, claims()] });
  assert.deepEqual(verdict.unjustified_non_effect, ["comment-1"]);
  assert.equal(verdict.verdict, "wrong");
});

test("a final failed claim for an applied operation is flagged; unknown is review, not wrong", () => {
  const failed = score({ records: ledger(), expect: "all", claims: [claims({ run_success: false, ops: { "comment-1": "failed" } })] });
  assert.deepEqual(failed.failed_but_applied, ["comment-1"]);
  assert.equal(failed.verdict, "wrong");
  const unknown = score({ records: ledger(), expect: "all", claims: [claims({ run_success: false, ops: { "comment-1": "unknown" }, review: ["comment-1"] })] });
  assert.equal(unknown.verdict, "correct-needs-review");
});

test("an honest incomplete run is safe-incomplete", () => {
  const verdict = score({ records: ledger({}, ["close-6"]), expect: "all", claims: [claims({ run_success: false, ops: { "close-6": "pending" } })] });
  assert.equal(verdict.verdict, "safe-incomplete");
});

test("a write after the account changed is an unsafe send", () => {
  const records = ledger({}, ["close-6"]);
  const last = records.at(-1)!.seq;
  records.push({ seq: last + 1, ts: last + 1, event: "control", action: "account", detail: { status: "revoked" } });
  const close = EXPECTED_OPS.find((op) => op.id === "close-6")!;
  records.push({ seq: last + 2, ts: last + 2, event: "start", call_id: "late", slug: close.slug, args: close.args, account: ACCOUNT, actor: LOGIN });
  const verdict = score({ records, expect: "stop_after_control", claims: [claims({ run_success: false })] });
  assert.equal(verdict.unsafe_sends.length, 1);
  assert.equal(verdict.verdict, "wrong");
  // The same write received before the change (a killed attempt's request in flight) is not one.
  records.at(-1)!.received = last - 1;
  assert.equal(score({ records, expect: "stop_after_control", claims: [claims({ run_success: false })] }).unsafe_sends.length, 0);
});

test("the tie rule needs both close medians and overlapping quartiles", () => {
  const a = { n: 15, p25: 95, p50: 100, p75: 105, p95: 120 };
  assert.equal(compareLatency(a, { n: 15, p25: 100, p50: 108, p75: 115, p95: 130 }), "tie");
  assert.equal(compareLatency(a, { n: 15, p25: 106, p50: 108, p75: 115, p95: 130 }), "a-faster");
  assert.equal(compareLatency(a, { n: 15, p25: 60, p50: 70, p75: 104, p95: 130 }), "b-faster");
});
