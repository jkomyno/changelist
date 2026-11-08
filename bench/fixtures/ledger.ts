/**
 * Independent ground truth for one run: which tool operations started and how they
 * ended, written by the fixture's tool wrapper, never by an engine.
 *
 * Events: "start" when the wrapper is invoked, "effect_applied" when a simulated side
 * effect lands in the in-memory store, and exactly one terminal event per op:
 * "complete" (resolved), "throw" (rejected by the tool or host policy), or "abort"
 * (rejected because the engine aborted the call's AbortSignal). An op may stay open
 * forever (a lost response whose signal never fires).
 *
 * `t_ms` is performance.now() relative to the driver's start of this run. `bytes` is
 * the UTF-8 JSON size of the result for "complete"; it is computed in `finalize()`,
 * after the run, so serializing results never adds to measured latency.
 */
import { canonicalJson, jsonBytes } from "./util.ts";

export type LedgerEvent = "start" | "complete" | "throw" | "abort" | "effect_applied";

export interface LedgerEntry {
  op: number;
  tool: string;
  args: string;
  event: LedgerEvent;
  t_ms: number;
  bytes?: number;
  /** Error code for "throw" (e.g. permission_denied). */
  code?: string;
}

export interface OpState {
  op: number;
  tool: string;
  args: string;
  argsBytes: number;
  start: number;
  effectApplied?: number;
  terminal?: Exclude<LedgerEvent, "start" | "effect_applied">;
  terminalAt?: number;
  resultBytes?: number;
}

export class Ledger {
  /** Driver start of the run; the driver resets it right before calling the engine. */
  t0: number;
  readonly entries: LedgerEntry[] = [];
  readonly ops: OpState[] = [];
  private readonly results = new Map<number, unknown>();
  private finalized = false;

  constructor(t0 = performance.now()) {
    this.t0 = t0;
  }

  now(): number {
    return +(performance.now() - this.t0).toFixed(3);
  }

  start(tool: string, args: unknown): number {
    const op = this.ops.length + 1;
    const canonical = canonicalJson(args ?? null);
    const t = this.now();
    this.ops.push({ op, tool, args: canonical, argsBytes: new TextEncoder().encode(canonical).byteLength, start: t });
    this.entries.push({ op, tool, args: canonical, event: "start", t_ms: t });
    return op;
  }

  effectApplied(op: number): void {
    const state = this.ops[op - 1];
    const t = this.now();
    state.effectApplied = t;
    this.entries.push({ op, tool: state.tool, args: state.args, event: "effect_applied", t_ms: t });
  }

  complete(op: number, result: unknown): void {
    this.end(op, "complete");
    // After finalize() (a late completion), size the result immediately.
    if (this.finalized) {
      const entry = this.entries[this.entries.length - 1];
      if (entry.op === op && entry.event === "complete") entry.bytes = this.ops[op - 1].resultBytes = jsonBytes(result);
    } else this.results.set(op, result);
  }

  fail(op: number, event: "throw" | "abort", code?: string): void {
    this.end(op, event, code);
  }

  private end(op: number, event: "complete" | "throw" | "abort", code?: string): void {
    const state = this.ops[op - 1];
    if (state.terminal) return;
    const t = this.now();
    state.terminal = event;
    state.terminalAt = t;
    const entry: LedgerEntry = { op, tool: state.tool, args: state.args, event, t_ms: t };
    if (code) entry.code = code;
    this.entries.push(entry);
  }

  /** Ops without a terminal event. */
  open(): OpState[] {
    return this.ops.filter((state) => !state.terminal);
  }

  /** Compute result sizes once the run (and its settle window) is over. */
  finalize(): void {
    if (this.finalized) return;
    this.finalized = true;
    for (const entry of this.entries) {
      if (entry.event !== "complete") continue;
      const bytes = jsonBytes(this.results.get(entry.op));
      entry.bytes = bytes;
      this.ops[entry.op - 1].resultBytes = bytes;
    }
    this.results.clear();
  }
}
