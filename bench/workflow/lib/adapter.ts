// What every engine adapter shares: its command line, its claims file, and its exit codes.
//
//   node engines/<name>.ts --provider <url> --run-dir <dir> --attempt <n>
//        --workload triage|cpu-loop|memory-growth --config host-default|equal-limits
//        [--deadline-ms N] [--heap-mb N] [--max-calls N] [--concurrency N] [--widen]
//
// `--deadline-ms`, `--heap-mb`, `--max-calls`, and `--concurrency` are given only in the
// equal-limits row; the host-default row passes none, so each adapter uses its engine's own
// defaults. Each adapter writes `<run-dir>/claims-<attempt>.json` when it finishes (a killed
// attempt writes nothing) and exits with one of EXIT.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

export const WORKFLOW_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const ROOT = resolve(WORKFLOW_DIR, "../..");
export const PKGS = join(WORKFLOW_DIR, ".cache", "pkgs");

/** Adapter exit codes the driver acts on. */
export const EXIT = {
  /** The engine reports the workflow complete. */
  success: 0,
  /** The engine finished and reports failures; it offers no further automatic step. */
  failed: 1,
  /** The adapter itself failed. */
  adapter: 2,
  /** The engine says work remains that running it again may finish. */
  incomplete: 3,
  /** The engine refused to run (authorization, approval). */
  refused: 4,
} as const;

/** One implementation's per-operation claim, normalized. */
export type Claim = "applied" | "satisfied" | "not_applied" | "refused" | "unknown" | "failed" | "pending";

export interface Claims {
  engine: string;
  attempt: number;
  /** Whether the engine reports the whole workflow as done. */
  run_success: boolean;
  /** The engine's own status words, verbatim. */
  run_status: string;
  ops: Record<string, Claim>;
  /** Operations the engine itself says need a person. */
  review: string[];
  /** The engine's own words for each operation, verbatim and short. */
  raw?: Record<string, string>;
  /** Durable records the engine wrote, where observable. */
  durable_records?: number | null;
  detail?: unknown;
}

export interface AdapterOptions {
  provider: string;
  runDir: string;
  attempt: number;
  workload: "triage" | "cpu-loop" | "memory-growth";
  config: "host-default" | "equal-limits";
  deadlineMs?: number;
  heapMb?: number;
  maxCalls?: number;
  concurrency?: number;
  widen: boolean;
}

export function adapterOptions(): AdapterOptions {
  const { values } = parseArgs({
    options: {
      provider: { type: "string" },
      "run-dir": { type: "string" },
      attempt: { type: "string", default: "0" },
      workload: { type: "string", default: "triage" },
      config: { type: "string", default: "host-default" },
      "deadline-ms": { type: "string" },
      "heap-mb": { type: "string" },
      "max-calls": { type: "string" },
      concurrency: { type: "string" },
      widen: { type: "boolean", default: false },
      // composio-script only: the deliberately broken configuration that checks the oracle.
      "blind-retry": { type: "boolean", default: false },
    },
  });
  if (!values.provider || !values["run-dir"]) throw new Error("--provider and --run-dir are required");
  const num = (value: string | undefined) => (value === undefined ? undefined : Number(value));
  return {
    provider: values.provider,
    runDir: resolve(values["run-dir"]),
    attempt: Number(values.attempt),
    workload: values.workload as AdapterOptions["workload"],
    config: values.config as AdapterOptions["config"],
    deadlineMs: num(values["deadline-ms"]),
    heapMb: num(values["heap-mb"]),
    maxCalls: num(values["max-calls"]),
    concurrency: num(values.concurrency),
    widen: values.widen,
  };
}

export function writeClaims(options: AdapterOptions, claims: Omit<Claims, "attempt">): void {
  writeFileSync(join(options.runDir, `claims-${options.attempt}.json`), `${JSON.stringify({ ...claims, attempt: options.attempt }, null, 2)}\n`);
}

/** The engine program: the shared triage core between an engine prelude and an engine tail. */
export function program(workload: AdapterOptions["workload"], prelude: string, tail: string): string {
  if (workload !== "triage") return readFileSync(join(WORKFLOW_DIR, "programs", `${workload}.js`), "utf8");
  return `${prelude}\n${readFileSync(join(WORKFLOW_DIR, "programs", "triage-core.js"), "utf8")}\n${tail}\n`;
}

/**
 * The widened change list used by the re-approval property: one extra write the reviewed
 * decision did not contain (closing the enhancement request #2).
 */
export const WIDEN_OP = {
  id: "close-2",
  tool: "GITHUB_UPDATE_AN_ISSUE",
  arguments: { owner: "bench", repo: "widgets", issue_number: 2, state: "closed", state_reason: "not_planned" },
};

/** Map a direct-write program's `@@OUTCOMES` line to claims. */
export function outcomeClaims(outcomes: Record<string, string>): { ops: Record<string, Claim>; review: string[]; raw: Record<string, string> } {
  const ops: Record<string, Claim> = {};
  const review: string[] = [];
  for (const [id, text] of Object.entries(outcomes)) {
    if (text === "ok") ops[id] = "applied";
    else if (/may have partially run|interrupted|outcome unknown/i.test(text)) {
      // An explicit statement that the outcome is uncertain: Pi's own, or the shared tool wrapper's.
      ops[id] = "unknown";
      review.push(id);
    } else ops[id] = "failed";
  }
  return { ops, review, raw: outcomes };
}

export function parseOutcomes(text: string): Record<string, string> | undefined {
  const match = /@@OUTCOMES (\{.*\})/.exec(text);
  return match ? JSON.parse(match[1]) : undefined;
}
