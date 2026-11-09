/**
 * One measured run of one engine on one scenario, plus the independent checks applied
 * to it. Shared by the in-process driver (run.ts) and fresh-process samples (child.ts).
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { EngineModule, EngineResult, Limits } from "./engines/types.ts";
import { Ledger, type OpState } from "./fixtures/ledger.ts";
import { type AddLabelVariant, type Profile, makeTools } from "./fixtures/tools.ts";
import { canonicalJson, jsonBytes } from "./fixtures/util.ts";

export const BENCH = dirname(fileURLToPath(import.meta.url));
export const ROOT = join(BENCH, "..");

export interface Scenario {
  name: string;
  program: string;
  dataset: "A" | "B";
  tools: string[];
  profiles: Profile[];
  policy?: boolean;
  add_label?: AddLabelVariant;
  limits?: Partial<Limits>;
  expect: {
    kind: string;
    output?: string;
    message_includes_any?: string[];
    ledger?: { effect_applied_without_complete?: number };
  };
  known_limits?: Record<string, string>;
  holdout: boolean;
}

export interface EngineSpec {
  /** Adapter module, relative to bench/. */
  module: string;
  /** Limits this configuration overrides. */
  overrides: Partial<Limits>;
  /** Whether this configuration can run under Node (OpenCode needs Bun). */
  node: boolean;
  /** Concurrency the engine actually applies, when it ignores the requested limit. */
  effectiveConcurrency?: number;
}

export const ENGINES: Record<string, EngineSpec> = {
  codemode: { module: "engines/codemode.ts", overrides: {}, node: true },
  "codemode-c4": { module: "engines/codemode.ts", overrides: { concurrency: 4 }, node: true },
  "codemode-prewarm": { module: "engines/codemode-prewarm.ts", overrides: {}, node: true },
  "codemode-embedded": { module: "engines/codemode-embedded.ts", overrides: {}, node: true },
  "codemode-embedded-prewarm": { module: "engines/codemode-embedded-prewarm.ts", overrides: {}, node: true },
  pi: { module: "engines/pi.ts", overrides: {}, node: true, effectiveConcurrency: Number.POSITIVE_INFINITY },
  opencode: { module: "engines/opencode.ts", overrides: {}, node: false, effectiveConcurrency: 8 },
};

const manifest = JSON.parse(readFileSync(join(BENCH, "scenarios.json"), "utf8")) as {
  defaults: { limits: Limits };
  scenarios: Scenario[];
};
export const SCENARIOS: Scenario[] = manifest.scenarios;
export const DEFAULT_LIMITS: Limits = manifest.defaults.limits;
export const EXPECTED = JSON.parse(readFileSync(join(BENCH, "expected.json"), "utf8")) as Record<string, unknown>;

export function scenario(name: string): Scenario {
  const found = SCENARIOS.find((s) => s.name === name);
  if (!found) throw new Error(`unknown scenario ${name}`);
  return found;
}

export function program(s: Scenario): string {
  return readFileSync(join(BENCH, s.program), "utf8");
}

export function limitsFor(s: Scenario, engine: string): Limits {
  return { ...DEFAULT_LIMITS, ...s.limits, ...ENGINES[engine].overrides };
}

export async function loadEngine(engine: string): Promise<EngineModule> {
  const spec = ENGINES[engine];
  if (!spec) throw new Error(`unknown engine ${engine}`);
  return (await import(join(BENCH, spec.module))) as EngineModule;
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** After the run returns: watch for late starts, then wait (bounded) for open ops to end. */
export const GRACE_MS = 50;
const OBSERVE_MS = 100;
const SETTLE_MAX_MS = 1000;
const WATCHDOG_EXTRA_MS = 10_000;

export interface RunOptions {
  /** Called right before runEngine; cold samples use it to time the engine import. */
  onStart?: () => void;
}

export interface RunRecord {
  engine: string;
  scenario: string;
  profile: Profile;
  limits: Limits;
  ok: boolean;
  errorKind?: string;
  engineKind?: string;
  message?: string;
  correct: boolean;
  correctness: string[];
  setupMs: number;
  runMs: number;
  taskMs: number;
  /** codemode only: the report's elapsed_ms (Rust clock, whole run). */
  engineElapsedMs?: number;
  bytes: {
    ledgerArgs: number;
    ledgerResults: number;
    payload: number;
    returned: number;
    /** codemode only: JSON size of the execution report. */
    report?: number;
    /** codemode, reserved for trace schema v2 (bytes crossing the Rust/guest boundary). */
    engineBoundary: number | null;
  };
  ledger: LedgerSummary;
  reconciliation: Reconciliation;
  raw: {
    output?: unknown;
    error?: EngineResult["error"];
    ledger: Ledger["entries"];
    engineTrace?: unknown;
    policyLog: unknown[];
    store: Record<string, string[]>;
  };
}

export interface LedgerSummary {
  started: number;
  completed: number;
  thrown: number;
  aborted: number;
  effectApplied: number;
  openAtEnd: number;
  /** Ops started more than GRACE_MS after the run returned. */
  startedAfterReturn: number;
  /** Ops started before the run returned whose tool was still running afterwards. */
  stillRunningAfterReturn: { completedLate: number; threwLate: number; abortedLate: number; open: number };
  effectAppliedWithoutComplete: number;
  maxInFlight: number;
  returnedAtMs: number;
}

export interface Reconciliation {
  method: string;
  violations: string[];
  notes: string[];
  counts: Record<string, number>;
}

export async function runOnce(
  engineName: string,
  mod: EngineModule,
  s: Scenario,
  profile: Profile,
  code: string,
  options: RunOptions = {},
): Promise<RunRecord> {
  const limits = limitsFor(s, engineName);
  const ledger = new Ledger();
  const set = makeTools({
    dataset: s.dataset,
    profile,
    policy: s.policy === true,
    addLabel: s.add_label ?? "normal",
    ledger,
    only: s.tools,
  });
  const watchdog = new AbortController();
  const timer = setTimeout(() => watchdog.abort(), limits.timeoutMs + WATCHDOG_EXTRA_MS);
  options.onStart?.();
  const start = performance.now();
  ledger.t0 = start;
  let result: EngineResult;
  try {
    result = await mod.runEngine({ code, tools: set.tools, limits, signal: watchdog.signal });
  } catch (error) {
    result = {
      ok: false,
      error: { kind: "harness_error", message: error instanceof Error ? error.message : String(error) },
      setupMs: 0,
      runMs: performance.now() - start,
    };
  }
  const end = performance.now();
  clearTimeout(timer);
  if (watchdog.signal.aborted && result.error) result.error.message = `[watchdog] ${result.error.message}`;
  const returnedAtMs = end - start;

  await sleep(OBSERVE_MS);
  const settleDeadline = performance.now() + SETTLE_MAX_MS - OBSERVE_MS;
  while (ledger.open().length > 0 && performance.now() < settleDeadline) await sleep(10);
  ledger.finalize();

  const summary = summarizeLedger(ledger, returnedAtMs);
  const reconciliation = reconcile(engineName, result, ledger, returnedAtMs);
  const correctness = checkCorrectness(s, result, summary);
  const report = engineName.startsWith("codemode") ? (result.engineTrace as Record<string, unknown> | undefined) : undefined;
  const ledgerArgs = ledger.ops.reduce((sum, op) => sum + op.argsBytes, 0);
  const ledgerResults = ledger.ops.reduce((sum, op) => sum + (op.resultBytes ?? 0), 0);
  const store: Record<string, string[]> = {};
  for (const [id, labels] of set.store) store[String(id)] = labels;

  return {
    engine: engineName,
    scenario: s.name,
    profile,
    limits,
    ok: result.ok,
    errorKind: result.error?.kind,
    engineKind: result.error?.engineKind,
    message: result.error?.message?.slice(0, 400),
    correct: correctness.length === 0,
    correctness,
    setupMs: result.setupMs,
    runMs: result.runMs,
    taskMs: returnedAtMs,
    engineElapsedMs: typeof report?.elapsed_ms === "number" ? (report.elapsed_ms as number) : undefined,
    bytes: {
      ledgerArgs,
      ledgerResults,
      payload: ledgerArgs + ledgerResults,
      returned: result.ok ? jsonBytes(result.output) : 0,
      ...(report ? { report: jsonBytes(report) } : {}),
      engineBoundary: null,
    },
    ledger: summary,
    reconciliation,
    raw: {
      output: result.output,
      error: result.error,
      ledger: ledger.entries,
      engineTrace: result.engineTrace,
      policyLog: set.policyLog,
      store,
    },
  };
}

function summarizeLedger(ledger: Ledger, returnedAtMs: number): LedgerSummary {
  const ops = ledger.ops;
  const late = { completedLate: 0, threwLate: 0, abortedLate: 0, open: 0 };
  for (const op of ops) {
    if (op.start > returnedAtMs) continue;
    if (!op.terminal) late.open++;
    else if (op.terminalAt! > returnedAtMs) {
      if (op.terminal === "complete") late.completedLate++;
      else if (op.terminal === "throw") late.threwLate++;
      else late.abortedLate++;
    }
  }
  const points: Array<[number, number]> = [];
  for (const op of ops) {
    points.push([op.start, 1]);
    if (op.terminalAt !== undefined) points.push([op.terminalAt, -1]);
  }
  points.sort((x, y) => x[0] - y[0] || x[1] - y[1]);
  let inFlight = 0;
  let maxInFlight = 0;
  for (const [, delta] of points) {
    inFlight += delta;
    maxInFlight = Math.max(maxInFlight, inFlight);
  }
  return {
    started: ops.length,
    completed: ops.filter((op) => op.terminal === "complete").length,
    thrown: ops.filter((op) => op.terminal === "throw").length,
    aborted: ops.filter((op) => op.terminal === "abort").length,
    effectApplied: ops.filter((op) => op.effectApplied !== undefined).length,
    openAtEnd: ops.filter((op) => !op.terminal).length,
    startedAfterReturn: ops.filter((op) => op.start > returnedAtMs + GRACE_MS).length,
    stillRunningAfterReturn: late,
    effectAppliedWithoutComplete: ops.filter((op) => op.effectApplied !== undefined && op.terminal !== "complete").length,
    maxInFlight,
    returnedAtMs: +returnedAtMs.toFixed(3),
  };
}

function checkCorrectness(s: Scenario, result: EngineResult, ledger: LedgerSummary): string[] {
  const problems: string[] = [];
  const expect = s.expect;
  if (expect.kind === "success") {
    if (!result.ok) problems.push(`expected success, got ${result.error?.kind}: ${result.error?.message?.slice(0, 160)}`);
    else if (canonicalJson(result.output) !== canonicalJson(EXPECTED[expect.output!])) problems.push("output differs from expected.json");
  } else {
    if (result.ok) problems.push(`expected ${expect.kind}, got success`);
    else if (result.error?.kind !== expect.kind) problems.push(`expected ${expect.kind}, got ${result.error?.kind}`);
    else if (expect.message_includes_any && !expect.message_includes_any.some((m) => result.error!.message.includes(m))) {
      problems.push(`error message lacks any of ${JSON.stringify(expect.message_includes_any)}`);
    }
  }
  const want = expect.ledger?.effect_applied_without_complete;
  if (want !== undefined && ledger.effectAppliedWithoutComplete !== want) {
    problems.push(`ledger: expected ${want} effect_applied without complete, saw ${ledger.effectAppliedWithoutComplete}`);
  }
  if (ledger.startedAfterReturn > 0) problems.push(`ledger: ${ledger.startedAfterReturn} op(s) started after the run returned`);
  return problems;
}

const key = (tool: string, args: string) => `${tool} ${args}`;

/** Describe what the ledger knows about an op whose engine-side outcome is uncertain. */
function ledgerState(op: OpState, returnedAtMs: number): string {
  const parts = ["started"];
  if (op.effectApplied !== undefined) parts.push("effect_applied");
  if (op.terminal) parts.push(`${op.terminal}${op.terminalAt! > returnedAtMs ? "_after_return" : ""}`);
  else parts.push("open");
  return parts.join("+");
}

function reconcile(engine: string, result: EngineResult, ledger: Ledger, returnedAtMs: number): Reconciliation {
  if (engine.startsWith("codemode")) return reconcileCodemode(result, ledger, returnedAtMs);
  if (engine === "pi") return reconcilePi(result, ledger, returnedAtMs);
  if (engine === "opencode") return reconcileOpenCode(result, ledger, returnedAtMs);
  return { method: "none", violations: [], notes: [], counts: {} };
}

function groupOps(ledger: Ledger): Map<string, OpState[]> {
  const byKey = new Map<string, OpState[]>();
  for (const op of ledger.ops) {
    const k = key(op.tool, op.args);
    byKey.set(k, [...(byKey.get(k) ?? []), op]);
  }
  return byKey;
}

/**
 * codemode: match trace calls to ledger ops by (tool, canonical arguments) multisets.
 * succeeded/provider_failed => a ledger complete/throw; cancelled_before_dispatch/rejected
 * => no ledger start; ledger effect_applied without complete => trace unknown; trace
 * unknown => report the ledger state.
 */
function reconcileCodemode(result: EngineResult, ledger: Ledger, returnedAtMs: number): Reconciliation {
  const violations: string[] = [];
  const notes: string[] = [];
  const counts: Record<string, number> = {};
  const report = result.engineTrace as { calls?: any[] } | undefined;
  if (!report?.calls) return { method: "codemode trace vs ledger", violations: ["no execution report"], notes, counts };
  const remaining = groupOps(ledger);
  const take = (k: string, want: (op: OpState) => boolean): OpState | undefined => {
    const list = remaining.get(k) ?? [];
    const index = list.findIndex(want);
    if (index === -1) return undefined;
    return list.splice(index, 1)[0];
  };
  const uncertain: Array<{ k: string; status: string }> = [];
  for (const call of report.calls) {
    const status = String(call.outcome?.status);
    counts[`trace_${status}`] = (counts[`trace_${status}`] ?? 0) + 1;
    const k = key(call.call.tool, canonicalJson(call.call.arguments ?? null));
    if (status === "succeeded") {
      if (!take(k, (op) => op.terminal === "complete")) violations.push(`trace succeeded without ledger complete: ${k.slice(0, 120)}`);
    } else if (status === "provider_failed") {
      if (!take(k, (op) => op.terminal === "throw")) violations.push(`trace provider_failed without ledger throw: ${k.slice(0, 120)}`);
    } else if (status === "cancelled_before_dispatch" || status === "rejected") {
      // Checked after matching: no unmatched ledger op may remain for this key.
      uncertain.push({ k, status });
    } else {
      uncertain.push({ k, status });
    }
  }
  // Dispatched-but-unobserved calls (unknown, result_too_large) take the remaining ops.
  for (const { k, status } of uncertain) {
    if (status === "cancelled_before_dispatch" || status === "rejected") continue;
    const op = take(k, () => true);
    if (!op) {
      notes.push(`trace ${status} with no ledger start (call never reached the tool): ${k.slice(0, 120)}`);
      counts[`${status}_no_ledger_start`] = (counts[`${status}_no_ledger_start`] ?? 0) + 1;
      continue;
    }
    const state = ledgerState(op, returnedAtMs);
    counts[`${status}:${state}`] = (counts[`${status}:${state}`] ?? 0) + 1;
    if (status !== "unknown" && status !== "result_too_large") violations.push(`unexpected trace status ${status}`);
  }
  for (const { k, status } of uncertain) {
    if (status !== "cancelled_before_dispatch" && status !== "rejected") continue;
    if ((remaining.get(k) ?? []).length > 0) violations.push(`trace ${status} but the ledger shows a start: ${k.slice(0, 120)}`);
  }
  for (const [k, ops] of remaining) {
    for (const op of ops) {
      violations.push(`ledger op ${op.op} (${ledgerState(op, returnedAtMs)}) has no trace call: ${k.slice(0, 120)}`);
    }
  }
  // effect_applied without complete must never be claimed as not run.
  for (const op of ledger.ops) {
    if (op.effectApplied !== undefined && op.terminal !== "complete") {
      const traced = report.calls.find(
        (call) => key(call.call.tool, canonicalJson(call.call.arguments ?? null)) === key(op.tool, op.args),
      );
      const status = traced?.outcome?.status;
      if (status !== "unknown") violations.push(`effect applied without complete, but trace says ${status ?? "nothing"}`);
    }
  }
  return { method: "codemode trace vs ledger, matched by (tool, canonical args) multisets", violations, notes, counts };
}

/**
 * Pi: result.calls holds {name, status: ok|error|cancelled} without arguments, in the order
 * Pi's host invoked the tools. Match the k-th Pi record for a tool with the k-th ledger op
 * for that tool (an inference about ordering, stated in the README).
 */
function reconcilePi(result: EngineResult, ledger: Ledger, returnedAtMs: number): Reconciliation {
  const violations: string[] = [];
  const notes: string[] = [];
  const counts: Record<string, number> = {};
  const calls = ((result.engineTrace as { calls?: any[] } | undefined)?.calls ?? []) as Array<{ name: string; status: string }>;
  const byTool = new Map<string, OpState[]>();
  for (const op of ledger.ops) byTool.set(op.tool, [...(byTool.get(op.tool) ?? []), op]);
  const seen = new Map<string, number>();
  for (const call of calls) {
    const index = seen.get(call.name) ?? 0;
    seen.set(call.name, index + 1);
    const op = byTool.get(call.name)?.[index];
    counts[`pi_${call.status}`] = (counts[`pi_${call.status}`] ?? 0) + 1;
    if (!op) {
      violations.push(`Pi recorded ${call.name} #${index + 1} (${call.status}) with no ledger start`);
      continue;
    }
    const state = ledgerState(op, returnedAtMs);
    if (call.status === "ok" && op.terminal !== "complete") violations.push(`Pi ok but ledger ${state} (${call.name})`);
    else if (call.status === "error" && op.terminal !== "throw") violations.push(`Pi error but ledger ${state} (${call.name})`);
    else if (call.status === "cancelled") {
      counts[`cancelled:${state}`] = (counts[`cancelled:${state}`] ?? 0) + 1;
      if (op.effectApplied !== undefined) notes.push(`Pi marks ${call.name} cancelled; the ledger shows its effect applied (${state})`);
      if (op.terminal === "complete" && op.terminalAt! <= returnedAtMs) {
        notes.push(`Pi marks ${call.name} cancelled; the ledger shows it completed before the run returned`);
      }
    }
  }
  for (const [tool, ops] of byTool) {
    const recorded = seen.get(tool) ?? 0;
    if (ops.length > recorded) violations.push(`${ops.length - recorded} ledger op(s) for ${tool} missing from Pi's calls`);
  }
  return { method: "Pi result.calls vs ledger, matched by tool name in call order (no arguments exposed)", violations, notes, counts };
}

/**
 * OpenCode: onToolCallStart/onToolCallEnd hooks carry {index, name, input, outcome}.
 * Match by (tool, canonical input). Interruption fires no end hook, so a start without an
 * end is an interrupted call; report the ledger state for it.
 */
function reconcileOpenCode(result: EngineResult, ledger: Ledger, returnedAtMs: number): Reconciliation {
  const violations: string[] = [];
  const notes: string[] = [];
  const counts: Record<string, number> = {};
  const trace = result.engineTrace as { events?: any[]; toolCalls?: any[] } | undefined;
  const events = trace?.events ?? [];
  counts.admitted = trace?.toolCalls?.length ?? 0;
  const ends = new Map<number, any>();
  for (const event of events) if (event.ev === "end") ends.set(event.index, event);
  const remaining = groupOps(ledger);
  for (const event of events) {
    if (event.ev !== "start") continue;
    counts.hook_started = (counts.hook_started ?? 0) + 1;
    const k = key(event.name, canonicalJson(event.input ?? null));
    const list = remaining.get(k) ?? [];
    const end = ends.get(event.index);
    const want = end?.outcome === "success" ? "complete" : end?.outcome === "failure" ? "throw" : undefined;
    let index = want ? list.findIndex((op) => op.terminal === want) : -1;
    if (index === -1) index = list.length > 0 ? 0 : -1;
    if (index === -1) {
      violations.push(`OpenCode start hook with no ledger start: ${k.slice(0, 120)}`);
      continue;
    }
    const op = list.splice(index, 1)[0];
    const state = ledgerState(op, returnedAtMs);
    if (!end) {
      counts[`interrupted:${state}`] = (counts[`interrupted:${state}`] ?? 0) + 1;
      if (op.effectApplied !== undefined) notes.push(`OpenCode reports no outcome for ${event.name}; the ledger shows its effect applied (${state})`);
      if (op.terminal === "complete" && op.terminalAt! <= returnedAtMs) {
        notes.push(`OpenCode reports no outcome for ${event.name}; the ledger shows it completed before the run returned`);
      }
    } else if (want && op.terminal !== want) violations.push(`OpenCode ${end.outcome} but ledger ${state} (${event.name})`);
  }
  for (const [k, ops] of remaining) {
    for (const op of ops) violations.push(`ledger op ${op.op} (${ledgerState(op, returnedAtMs)}) has no OpenCode start hook: ${k.slice(0, 120)}`);
  }
  if (counts.admitted !== (counts.hook_started ?? 0)) {
    notes.push(`OpenCode admitted ${counts.admitted} call(s) (toolCalls) but ${counts.hook_started ?? 0} reached the start hook`);
  }
  return { method: "OpenCode start/end hooks vs ledger, matched by (tool, canonical input)", violations, notes, counts };
}

export function stats(values: number[]) {
  const sorted = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  const n = sorted.length;
  if (n === 0) return null;
  const rank = (p: number) => sorted[Math.max(0, Math.ceil((p / 100) * n) - 1)];
  const round = (v: number) => +v.toFixed(2);
  return {
    n,
    p50: round(rank(50)),
    p95: round(rank(95)),
    min: round(sorted[0]),
    max: round(sorted[n - 1]),
    mean: round(sorted.reduce((sum, v) => sum + v, 0) / n),
  };
}
