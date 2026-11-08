/**
 * Host-side tools shared by every engine. Each tool is an async function of JSON
 * arguments returning JSON. A per-run instance is built by `makeTools`, which layers:
 *
 *   ledger (start / terminal events)  ->  permission policy (denial scenario only)
 *     ->  report("executor_started")  ->  latency profile  ->  tool body
 *
 * Every side effect is simulated in a per-run in-memory store. Latencies are
 * deterministic functions of the tool and its arguments. Waits honour the call's
 * AbortSignal: when an engine aborts a call, the tool rejects and the ledger records
 * "abort".
 */
import { type DatasetA, type DatasetB, datasetA, datasetB } from "./datasets.ts";
import { Ledger } from "./ledger.ts";
import { fnv1a } from "./util.ts";

export interface ToolCtx {
  signal?: AbortSignal;
  report?: (kind: string) => void;
}
export type ToolFn = (args: any, ctx: ToolCtx) => Promise<unknown>;
export type Profile = "zero" | "equal" | "mixed";
export type AddLabelVariant = "normal" | "lost_response";

export const TOOL_NAMES = {
  A: ["tracker.list_issues", "tracker.get_issue", "directory.get_user", "tracker.list_issues_large", "tracker.add_label"],
  B: ["vcs.list_pulls", "ci.get_checks", "directory.get_user"],
} as const;

export const PAGE_LIMIT = 40;
export const LARGE_PAGE_LIMIT = 200;
export const PULLS_PAGE_LIMIT = 50;
export const APPROVAL_MS = 15;

const GET_ISSUE_MS = [10, 20, 40, 80, 160];
const GET_CHECKS_MS = [10, 30, 90];

/** Deterministic latency for one call under a profile. */
export function latencyMs(profile: Profile, tool: string, args: any): number {
  if (profile === "zero") return 0;
  if (profile === "equal") return 30;
  switch (tool) {
    case "tracker.list_issues":
    case "tracker.list_issues_large":
      return 60;
    case "tracker.get_issue":
      return GET_ISSUE_MS[fnv1a(String(args?.id)) % GET_ISSUE_MS.length];
    case "directory.get_user":
      return 25;
    case "tracker.add_label":
      return 40;
    case "vcs.list_pulls":
      return 50;
    case "ci.get_checks":
      return GET_CHECKS_MS[fnv1a(String(args?.pr)) % GET_CHECKS_MS.length];
    default:
      return 0;
  }
}

const ABORTED = Symbol("bench.aborted");

class ToolError extends Error {
  code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

function abortError(): Error {
  const error = new Error("tool call aborted by the engine") as Error & { [ABORTED]?: true; code?: string };
  error.name = "AbortError";
  error.code = "aborted";
  error[ABORTED] = true;
  return error;
}

/** Abortable sleep; `ms <= 0` resolves without a timer but still honours an aborted signal. */
function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) return Promise.reject(abortError());
  if (ms <= 0) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      clearTimeout(timer);
      reject(abortError());
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/** Never settles unless `signal` aborts: a response lost after the effect was applied. */
function lostResponse(signal?: AbortSignal): Promise<never> {
  return new Promise((_, reject) => {
    if (!signal) return;
    if (signal.aborted) return reject(abortError());
    signal.addEventListener("abort", () => reject(abortError()), { once: true });
  });
}

function cursorFor(kind: string, offset: number): string {
  return btoa(`${kind}:${offset}`);
}

function offsetFrom(kind: string, cursor: unknown): number {
  if (cursor === undefined || cursor === null) return 0;
  let decoded = "";
  try {
    decoded = atob(String(cursor));
  } catch {
    // fall through
  }
  const [prefix, offset] = decoded.split(":");
  if (prefix !== kind || !/^[0-9]+$/.test(offset ?? "")) throw new ToolError("invalid_cursor", "invalid cursor");
  return Number(offset);
}

function pageLimit(limit: unknown, max: number): number {
  if (limit === undefined || limit === null) return max;
  if (!Number.isInteger(limit) || (limit as number) < 1) throw new ToolError("invalid_argument", "limit must be a positive integer");
  return Math.min(limit as number, max);
}

function paginate<T>(items: readonly T[], kind: string, args: any, max: number) {
  const offset = offsetFrom(kind, args?.cursor);
  const limit = pageLimit(args?.limit, max);
  const end = Math.min(offset + limit, items.length);
  return { items: items.slice(offset, end), next_cursor: end < items.length ? cursorFor(kind, end) : null };
}

export interface ToolOptions {
  dataset: "A" | "B";
  profile: Profile;
  /** Denial scenario: approval wait plus confidential-issue denial on tracker.get_issue. */
  policy?: boolean;
  addLabel?: AddLabelVariant;
  ledger: Ledger;
  /** Restrict to these names (the scenario's authorized tools). */
  only?: readonly string[];
}

export interface PolicyDecision {
  op: number;
  id: unknown;
  decision: "granted" | "denied";
  t_ms: number;
}

export interface ToolSet {
  tools: Record<string, ToolFn>;
  /** Simulated add_label effects: issue id -> labels added in this run. */
  store: Map<number, string[]>;
  policyLog: PolicyDecision[];
}

type Body = (args: any, signal: AbortSignal | undefined, op: number) => Promise<unknown>;

export function makeTools(options: ToolOptions): ToolSet {
  const { ledger, profile } = options;
  const store = new Map<number, string[]>();
  const policyLog: PolicyDecision[] = [];
  const bodies: Record<string, Body> = {};

  if (options.dataset === "A") {
    const a: DatasetA = datasetA();
    const users = new Map(a.users.map((user) => [user.id, user]));
    const details = new Map(a.details.map((detail) => [detail.id, detail]));
    const project = (args: any) => {
      if (args?.project !== a.project) throw new ToolError("not_found", `unknown project ${String(args?.project)}`);
    };
    bodies["tracker.list_issues"] = async (args) => {
      project(args);
      return paginate(a.summaries, "issues", args, PAGE_LIMIT);
    };
    bodies["tracker.list_issues_large"] = async (args) => {
      project(args);
      return paginate(a.large, "large", args, LARGE_PAGE_LIMIT);
    };
    bodies["tracker.get_issue"] = async (args) => {
      const detail = details.get(args?.id);
      if (!detail) throw new ToolError("not_found", `unknown issue ${String(args?.id)}`);
      return detail;
    };
    bodies["directory.get_user"] = async (args) => {
      const user = users.get(args?.id);
      if (!user) throw new ToolError("not_found", `unknown user ${String(args?.id)}`);
      return user;
    };
    bodies["tracker.add_label"] = async (args, signal, op) => {
      if (!details.has(args?.id) || typeof args?.label !== "string") throw new ToolError("invalid_argument", "id and label are required");
      store.set(args.id, [...(store.get(args.id) ?? []), args.label]);
      ledger.effectApplied(op);
      if (options.addLabel === "lost_response") return lostResponse(signal);
      return { ok: true };
    };
  } else {
    const b: DatasetB = datasetB();
    const users = new Map(datasetA().users.map((user) => [user.id, user]));
    bodies["vcs.list_pulls"] = async (args) => {
      if (args?.repo !== b.repo) throw new ToolError("not_found", `unknown repo ${String(args?.repo)}`);
      return paginate(b.pulls, "pulls", args, PULLS_PAGE_LIMIT);
    };
    bodies["ci.get_checks"] = async (args) => {
      const checks = b.checks[String(args?.pr)];
      if (!checks || !Number.isInteger(args?.pr)) throw new ToolError("not_found", `unknown pull request ${String(args?.pr)}`);
      return { pr: args.pr, checks };
    };
    bodies["directory.get_user"] = async (args) => {
      const user = users.get(args?.id);
      if (!user) throw new ToolError("not_found", `unknown user ${String(args?.id)}`);
      return user;
    };
  }

  const confidential = options.dataset === "A" ? new Set(datasetA().confidential) : new Set<number>();
  const tools: Record<string, ToolFn> = {};
  for (const [name, body] of Object.entries(bodies)) {
    if (options.only && !options.only.includes(name)) continue;
    const gated = options.policy === true && name === "tracker.get_issue";
    tools[name] = async (args, ctx = {}) => {
      const op = ledger.start(name, args);
      try {
        if (gated) {
          ctx.report?.("permission_requested");
          await sleep(APPROVAL_MS, ctx.signal);
          if (confidential.has(args?.id)) {
            policyLog.push({ op, id: args?.id, decision: "denied", t_ms: ledger.now() });
            ctx.report?.("permission_denied");
            throw new ToolError("permission_denied", "permission denied by host policy");
          }
          policyLog.push({ op, id: args?.id, decision: "granted", t_ms: ledger.now() });
          ctx.report?.("permission_granted");
        }
        ctx.report?.("executor_started");
        await sleep(latencyMs(profile, name, args), ctx.signal);
        const result = await body(args, ctx.signal, op);
        ledger.complete(op, result);
        return result;
      } catch (error) {
        const aborted = (error as { [ABORTED]?: boolean })?.[ABORTED] === true;
        ledger.fail(op, aborted ? "abort" : "throw", (error as { code?: string })?.code ?? "error");
        throw error;
      }
    };
  }
  return { tools, store, policyLog };
}
