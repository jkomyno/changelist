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
 * Paths: CODEMODE_BIN (default target/release/codemode). Each run starts one bridge,
 * which starts one fresh embedded V8 worker for the program.
 */
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  type BridgeCall,
  type BridgeLimits,
  type HostReply,
  type RunReport,
  runBridge,
} from "../../integrations/shared/bridge-client.ts";
import type { EngineInfo, EngineInput, EngineResult, ErrorKind } from "./types.ts";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const BIN = resolve(process.env.CODEMODE_BIN ?? join(ROOT, "target/release/codemode"));

export const info: EngineInfo = {
  name: "codemode",
  version: "embedded V8 worker; bridge protocol 2 (this checkout)",
  limits: {
    timeoutMs: "enforced by Rust; one absolute deadline that includes worker startup",
    maxCalls: "enforced by Rust; the first call beyond the allowance is rejected and ends the run",
    concurrency: "enforced by Rust's rolling scheduler (8, or 4 for codemode-c4)",
    maxOutputBytes: "enforced by Rust; shared budget for tool replies and final output (default 262144, set to 1048576 here); separately, each reply to the guest is capped at 1 MiB",
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

export async function runEngine(input: EngineInput): Promise<EngineResult> {
  return runOnce(input, {
    bin: BIN,
    cwd: ROOT,
    timeoutMs: input.limits.timeoutMs,
    concurrency: input.limits.concurrency,
    maxCalls: input.limits.maxCalls,
    maxOutputBytes: input.limits.maxOutputBytes,
  });
}

async function runOnce(input: EngineInput, limits: BridgeLimits): Promise<EngineResult> {
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
