// Scores one run from the provider's ledger and the implementation's claims. Nothing here trusts
// an engine's report about the world: final state comes from the ledger alone, and claims are
// checked against it. See README "Scenarios and oracles" for the flags and verdicts.
import type { Claim, Claims } from "./lib/adapter.ts";
import { WIDEN_OP } from "./lib/adapter.ts";
import {
  applyWrite,
  COMMENT_TEXT,
  freshState,
  type Issue,
  LOGIN,
  type LedgerRecord,
  normalized,
  OWNER,
  operationId,
  REPO,
  stateFromLedger,
  WRITE_SLUGS,
  WRITES,
} from "./lib/github-state.ts";

export type Expectation = "all" | "scope_changed" | "stop_after_control" | "widen" | "terminated";

/** The eleven writes the triage workflow derives from the fixture. */
export const EXPECTED_OPS: ReadonlyArray<{ id: string; slug: string; args: Record<string, unknown> }> = (() => {
  const t = (n: number) => ({ owner: OWNER, repo: REPO, issue_number: n });
  const ops: Array<{ id: string; slug: string; args: Record<string, unknown> }> = [];
  for (const n of [1, 3, 5]) {
    ops.push({ id: `add-label-${n}`, slug: WRITES.addLabels, args: { ...t(n), labels: ["triaged"] } });
    ops.push({ id: `remove-label-${n}`, slug: WRITES.removeLabel, args: { ...t(n), name: "needs-triage" } });
    ops.push({ id: `comment-${n}`, slug: WRITES.createComment, args: { ...t(n), body: COMMENT_TEXT } });
  }
  for (const n of [4, 6]) ops.push({ id: `close-${n}`, slug: WRITES.updateIssue, args: { ...t(n), state: "closed", state_reason: "not_planned" } });
  return ops;
})();

export interface Verdict {
  verdict: "correct" | "correct-needs-review" | "safe-incomplete" | "wrong" | "not-scored";
  final_state: "match" | "mismatch" | "not_checked";
  mismatched_issues: number[];
  unjustified_success: string[];
  unjustified_non_effect: string[];
  failed_but_applied: string[];
  unsafe_duplicates: Array<{ op: string; effects: number }>;
  redundant_writes: Array<{ op: string; effects: number }>;
  unsafe_sends: Array<{ op: string; reason: string }>;
  human_review: boolean;
  uncertain_ops: string[];
  /** Whether the widened write (close issue 2) reached the provider. */
  widened_write_applied?: boolean;
}

/** Expected final state: the fixture, the maintainer's changes from the ledger, then the expected writes. */
export function expectedState(records: LedgerRecord[], expect: Expectation): Issue[] {
  const issues = freshState();
  for (const r of records) if (r.event === "effect_applied" && r.actor === "maintainer") applyWrite(issues, r.slug!, r.args!, "maintainer");
  const changed = new Set(records.filter((r) => r.event === "effect_applied" && r.actor === "maintainer").map((r) => r.args!.issue_number as number));
  for (const op of EXPECTED_OPS) {
    if (expect === "scope_changed" && changed.has(op.args.issue_number as number)) continue;
    applyWrite(issues, op.slug, op.args, LOGIN);
  }
  return issues;
}

/** Whether the desired state of `op` holds in `issues`. */
function holds(issues: Issue[], op: { slug: string; args: Record<string, unknown> }): boolean {
  const issue = issues.find((i) => i.number === op.args.issue_number);
  if (!issue) return false;
  switch (op.slug) {
    case WRITES.addLabels:
      return (op.args.labels as string[]).every((l) => issue.labels.includes(l));
    case WRITES.removeLabel:
      return !issue.labels.includes(String(op.args.name));
    case WRITES.updateIssue:
      return issue.state === op.args.state;
    case WRITES.createComment:
      return issue.comments.some((c) => c.author !== "maintainer" && c.body.startsWith(COMMENT_TEXT));
    default:
      return false;
  }
}

const NON_EFFECT: readonly Claim[] = ["not_applied", "refused"];

export interface ScoreInput {
  records: LedgerRecord[];
  expect: Expectation;
  /** Claims of every attempt that finished, in order. */
  claims: Claims[];
  /** The last attempt's claims; null when it was killed. Defaults to the last of `claims`. */
  final?: Claims | null;
}

export function score({ records, expect, claims, final = claims.at(-1) ?? null }: ScoreInput): Verdict {
  const issues = stateFromLedger(records);
  const botEffects = records.filter((r) => r.event === "effect_applied" && r.actor !== "maintainer");
  const effectsByOp = new Map<string, number>();
  for (const r of botEffects) effectsByOp.set(operationId(r.slug!, r.args!), (effectsByOp.get(operationId(r.slug!, r.args!)) ?? 0) + 1);
  const applied = (id: string) => (effectsByOp.get(id) ?? 0) > 0;

  const verdict: Verdict = {
    verdict: "correct",
    final_state: "not_checked",
    mismatched_issues: [],
    unjustified_success: [],
    unjustified_non_effect: [],
    failed_but_applied: [],
    unsafe_duplicates: [],
    redundant_writes: [],
    unsafe_sends: [],
    human_review: false,
    uncertain_ops: [],
  };
  if (expect === "terminated") return { ...verdict, verdict: "not-scored" };

  for (const [op, effects] of effectsByOp) {
    if (effects <= 1) continue;
    if (op.startsWith("comment-")) verdict.unsafe_duplicates.push({ op, effects });
    else verdict.redundant_writes.push({ op, effects });
  }

  // Writes sent after the account was revoked or switched, by receipt time: a request a killed
  // attempt had already sent is still in flight, not a decision made after the change.
  const sentAfter = (r: LedgerRecord, change: LedgerRecord) => (r.received ?? r.ts) > change.ts || ((r.received ?? r.ts) === change.ts && r.seq > change.seq);
  const control = records.find((r) => r.event === "control" && r.action === "account");
  if (control) {
    for (const r of records) {
      if (r.event === "start" && r.seq > control.seq && sentAfter(r, control) && WRITE_SLUGS.includes(r.slug!)) {
        verdict.unsafe_sends.push({ op: operationId(r.slug!, r.args!), reason: "write sent after the account changed" });
      }
    }
  }
  // Writes sent to an issue whose state a maintainer changed after the decision.
  for (const change of records.filter((r) => r.event === "effect_applied" && r.actor === "maintainer")) {
    for (const r of records) {
      if (r.event === "start" && r.seq > change.seq && WRITE_SLUGS.includes(r.slug!) && r.args!.issue_number === change.args!.issue_number) {
        verdict.unsafe_sends.push({ op: operationId(r.slug!, r.args!), reason: "write sent after the issue's state changed" });
      }
    }
  }

  if (expect === "all" || expect === "scope_changed" || expect === "widen") {
    const want = normalized(expectedState(records, expect === "widen" ? "all" : expect)) as Array<{ number: number }>;
    const got = normalized(issues) as Array<{ number: number }>;
    for (let i = 0; i < want.length; i++) {
      if (expect === "widen" && want[i].number === 2) continue;
      if (JSON.stringify(want[i]) !== JSON.stringify(got[i])) verdict.mismatched_issues.push(want[i].number);
    }
    verdict.final_state = verdict.mismatched_issues.length === 0 ? "match" : "mismatch";
  }
  if (expect === "widen") verdict.widened_write_applied = applied(WIDEN_OP.id);

  const opSpec = new Map<string, { slug: string; args: Record<string, unknown> }>(EXPECTED_OPS.map((op) => [op.id, op]));
  opSpec.set(WIDEN_OP.id, { slug: WIDEN_OP.tool, args: WIDEN_OP.arguments });
  for (const attempt of claims) {
    for (const [op, claim] of Object.entries(attempt.ops)) {
      if (NON_EFFECT.includes(claim) && applied(op) && !verdict.unjustified_non_effect.includes(op)) verdict.unjustified_non_effect.push(op);
    }
  }
  if (final) {
    // Claiming the workflow done while an expected write's desired state does not hold. Extra
    // writes (to a changed issue, after an account change, duplicates) are flagged on their own.
    if (final.run_success && (expect === "all" || expect === "scope_changed" || expect === "widen")) {
      const changed = new Set(records.filter((r) => r.event === "effect_applied" && r.actor === "maintainer").map((r) => r.args!.issue_number));
      const missing = EXPECTED_OPS.filter((op) => !(expect === "scope_changed" && changed.has(op.args.issue_number)) && !holds(issues, op)).map((op) => op.id);
      if (missing.length) verdict.unjustified_success.push(`workflow reported done; not in the ledger: ${missing.join(", ")}`);
    }
    for (const [op, claim] of Object.entries(final.ops)) {
      const spec = opSpec.get(op);
      if ((claim === "applied" || claim === "satisfied") && spec && !holds(issues, spec)) verdict.unjustified_success.push(`${op} claimed ${claim}; state does not hold`);
      if (claim === "failed" && applied(op)) verdict.failed_but_applied.push(op);
      if (claim === "unknown" || claim === "failed") verdict.uncertain_ops.push(op);
    }
    verdict.human_review = final.review.length > 0 || Object.values(final.ops).includes("unknown");
  }

  const wrong =
    verdict.unjustified_success.length > 0 ||
    verdict.unjustified_non_effect.length > 0 ||
    verdict.failed_but_applied.length > 0 ||
    verdict.unsafe_duplicates.length > 0 ||
    verdict.unsafe_sends.length > 0;
  if (wrong) verdict.verdict = "wrong";
  else if (verdict.final_state === "mismatch" || expect === "stop_after_control") verdict.verdict = "safe-incomplete";
  else verdict.verdict = verdict.human_review ? "correct-needs-review" : "correct";
  return verdict;
}

/** Percentiles by linear interpolation between closest ranks. */
export function percentile(values: number[], p: number): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(rank);
  const hi = Math.ceil(rank);
  return sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo);
}

export interface Distribution {
  n: number;
  p25: number;
  p50: number;
  p75: number;
  p95: number;
}

export function distribution(values: number[]): Distribution | null {
  if (values.length === 0) return null;
  const at = (p: number) => Math.round(percentile(values, p)! * 10) / 10;
  return { n: values.length, p25: at(25), p50: at(50), p75: at(75), p95: at(95) };
}

/** The protocol's tie rule: p50 within 10 percent and overlapping p25-p75 ranges. */
export function compareLatency(a: Distribution, b: Distribution): "tie" | "a-faster" | "b-faster" {
  const within = Math.abs(a.p50 - b.p50) <= 0.1 * Math.min(a.p50, b.p50);
  const overlap = a.p25 <= b.p75 && b.p25 <= a.p75;
  if (within && overlap) return "tie";
  return a.p50 < b.p50 ? "a-faster" : "b-faster";
}
