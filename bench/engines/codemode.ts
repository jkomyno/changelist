/**
 * codemode (this repository): `target/release/codemode bridge`, driven through the
 * existing host client `runBridge` (integrations/shared/bridge-client.ts, unmodified).
 *
 * Shim: none on the guest side; programs already use `await tools.call(name, args)` and
 * a function body with `return`. Host side, each bridge call is forwarded to the tool
 * function: `execute(call, signal)` -> `tools[call.tool](call.arguments, { signal })`.
 * A resolved value becomes `{ ok: true, value }`; a rejection becomes
 * `{ ok: false, error: { code: error.code ?? "tool_error", message } }`. runBridge's
 * third `report` argument is forwarded as `ctx.report`, so the fixtures' permission and
 * executor facts reach the trace; other engines receive no `report`.
 *
 * Limits: all four are enforced by Rust (deadline including guest startup, call allowance,
 * rolling concurrency cap, shared reply/output byte budget). Independently of
 * maxOutputBytes, each reply to the guest is capped at 1 MiB and each call frame from
 * it at 64 KiB.
 *
 * Paths: CODEMODE_BIN (default target/release/codemode) and CODEMODE_DENO (default
 * `mise which deno`, resolved only for Deno runs). The optional embedded-v8 backend
 * uses the same bridge and limits without resolving or launching Deno.
 */
import { execFileSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  type BridgeCall,
  type BridgeLimits,
  bridgeKey,
  type HostReply,
  type RunReport,
  runBridge,
  type StartedBridge,
  startBridge,
} from "../../integrations/shared/bridge-client.ts";
import type { EngineInfo, EngineInput, EngineResult, ErrorKind } from "./types.ts";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const BIN = resolve(process.env.CODEMODE_BIN ?? join(ROOT, "target/release/codemode"));
let deno: string | undefined;
const denoPath = () => deno ??= process.env.CODEMODE_DENO ?? execFileSync("mise", ["which", "deno"], { cwd: ROOT, encoding: "utf8" }).trim();

export const info: EngineInfo = {
  name: "codemode",
  version: "Deno backend; bridge protocol 2 (this checkout)",
  limits: {
    timeoutMs: "enforced by Rust; one absolute deadline that includes bridge and Deno startup",
    maxCalls: "enforced by Rust; the first call beyond the allowance is rejected and ends the run",
    concurrency: "enforced by Rust's rolling scheduler (8, or 4 for codemode-c4)",
    maxOutputBytes: "enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the Deno guest is capped at 1 MiB",
  },
  shim: "none in the guest; host shim maps bridge calls to tool functions and errors to {code, message}",
};

const KINDS: Record<string, ErrorKind> = {
  deadline: "timeout",
  guest_error: "guest_error",
  call_limit: "call_limit",
  output_limit: "output_limit",
  cancelled: "cancelled",
};

/**
 * With `prewarm`, the adapter keeps one started bridge (and its selected guest) ready
 * for the next run with the same limits, started just after the previous run
 * returned. Each bridge serves one run. A run that finds no ready bridge, such as
 * the first in a process, starts one itself.
 */
export function createEngine(options: { prewarm: boolean; backend?: BridgeLimits["backend"] }): { info: EngineInfo; runEngine: (input: EngineInput) => Promise<EngineResult> } {
  const backend = options.backend ?? "deno";
  const embedded = backend === "embedded-v8";
  const guest = embedded ? "embedded V8 worker" : "Deno guest";
  const engineInfo: EngineInfo = embedded
    ? {
        ...info,
        name: "codemode-embedded",
        version: "embedded-v8 backend; bridge protocol 2 (this checkout)",
        limits: {
          ...info.limits,
          timeoutMs: "enforced by Rust; one absolute deadline that includes bridge and embedded worker startup",
          maxOutputBytes: "enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the embedded guest is capped at 1 MiB",
        },
      }
    : info;
  let ready: StartedBridge | undefined;
  const take = (limits: BridgeLimits): StartedBridge | undefined => {
    const found = ready;
    ready = undefined;
    if (found && !found.exited && found.key === bridgeKey(limits)) return found;
    found?.child.kill();
    return undefined;
  };
  const prepareNext = (limits: BridgeLimits) => {
    // After this run has returned, so starting the next bridge is not part of it.
    setTimeout(() => {
      ready?.child.kill();
      ready = startBridge(limits);
    }, 0);
  };
  return {
    info: options.prewarm
      ? {
          ...engineInfo,
          name: `${engineInfo.name}-prewarm`,
          shim: `${engineInfo.shim}; one bridge and ${guest} started ahead of each run, outside its measured time; one fresh guest per run`,
        }
      : engineInfo,
    runEngine: async (input) => {
      const limits: BridgeLimits = {
        bin: BIN,
        backend,
        ...(embedded ? {} : { deno: denoPath() }),
        cwd: ROOT,
        timeoutMs: input.limits.timeoutMs,
        concurrency: input.limits.concurrency,
        maxCalls: input.limits.maxCalls,
        maxOutputBytes: input.limits.maxOutputBytes,
      };
      const started = options.prewarm ? take(limits) : undefined;
      try {
        return await runOnce(input, limits, started);
      } finally {
        if (options.prewarm) prepareNext(limits);
      }
    },
  };
}

export const runEngine = createEngine({ prewarm: false }).runEngine;

async function runOnce(input: EngineInput, limits: BridgeLimits, started: StartedBridge | undefined): Promise<EngineResult> {
  const setupStart = performance.now();
  const execute = async (call: BridgeCall, signal: AbortSignal, report?: (kind: string) => void): Promise<HostReply> => {
    const tool = input.tools[call.tool];
    if (!tool) return { ok: false, error: { code: "tool_denied", message: `no tool ${call.tool}` } };
    try {
      const value = await tool(call.arguments, report ? { signal, report } : { signal });
      return { ok: true, value };
    } catch (error) {
      const code = (error as { code?: unknown })?.code;
      return {
        ok: false,
        error: {
          code: typeof code === "string" ? code : "tool_error",
          message: error instanceof Error ? error.message : String(error),
        },
      };
    }
  };
  const setupMs = performance.now() - setupStart;

  const runStart = performance.now();
  let report: RunReport;
  try {
    report = await runBridge({
      ...limits,
      code: input.code,
      tools: Object.keys(input.tools),
      signal: input.signal,
      execute,
      started,
    });
  } catch (error) {
    return {
      ok: false,
      error: { kind: "harness_error", message: error instanceof Error ? error.message : String(error) },
      setupMs,
      runMs: performance.now() - runStart,
    };
  }
  const runMs = performance.now() - runStart;
  if (report.status === "success") return { ok: true, output: report.output, engineTrace: report, setupMs, runMs };
  return {
    ok: false,
    error: {
      kind: KINDS[report.status] ?? "engine_error",
      engineKind: report.status,
      message: String(report.diagnostic ?? report.status),
    },
    engineTrace: report,
    setupMs,
    runMs,
  };
}
