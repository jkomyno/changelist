/**
 * Pi: `@earendil-works/pi-codemode` 1.4.0 (`CodemodeSandbox`), loaded from the verified
 * registry tarball in bench/.cache/pi (see bench/setup.sh). Each execute() starts a fresh
 * node:worker_threads worker and QuickJS-wasm VM. Runs on Bun and Node >= 22.19.
 *
 * Shim (guest side, the only adaptation): Pi exposes tools as `tools.<jsName>(args)`,
 * where jsName replaces characters that are not valid in an identifier with `_`, and its
 * `tools` object throws on unknown members, so `tools.call` does not exist. The program is
 * wrapped on its first line, so Pi's line numbers match the program's:
 *
 *   const __piTools = tools, __piNames = {"tracker.list_issues":"tracker_list_issues",...};
 *   return await (async (tools) => {<program>
 *   })({ call: async (name, args) => __piTools[__piNames[name]](args) });
 *
 * `__piTools[jsName](args)` is Pi's own per-tool caller (JSON strings across the VM
 * boundary, host call, JSON back). Host side, each tool's `execute(args, { signal })` is
 * passed to the fixture tool as `ctx.signal`; Pi has no report channel.
 *
 * Limits: Pi enforces only a deadline (`timeoutMs`, armed before the worker starts; Pi's
 * default is 300 s). It has no call allowance, no concurrency cap (all submitted calls run
 * at once), and no tool-reply byte budget; maxCalls, concurrency, and maxOutputBytes are
 * therefore NOT applied. No QuickJS memory limit is set (Pi's engine default).
 *
 * Location: PI_CODEMODE_DIST overrides the dist/index.js path.
 */
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import type { EngineInfo, EngineInput, EngineResult, ErrorKind } from "./types.ts";

const DIST =
  process.env.PI_CODEMODE_DIST ??
  join(dirname(fileURLToPath(import.meta.url)), "../.cache/pi/node_modules/@earendil-works/pi-codemode/dist/index.js");

// biome-ignore lint: dynamic import of the pinned engine
const pi: any = await import(pathToFileURL(DIST).href);

export const info: EngineInfo = {
  name: "pi",
  version: "@earendil-works/pi-codemode 1.4.0 (quickjs-wasi 3.6.2)",
  limits: {
    timeoutMs: "enforced by Pi (deadline armed before the worker starts)",
    maxCalls: "not supported; not applied",
    concurrency: "not supported; every submitted call runs at once (unbounded)",
    maxOutputBytes: "not supported for tool replies; Pi caps text()/console output at 16 MiB characters only",
  },
  shim: "guest prelude rebinding tools.call(name, args) to Pi's tools.<jsName>(args), on the program's first line",
};

const KINDS: Record<string, ErrorKind> = { timeout: "timeout", script: "guest_error", aborted: "cancelled" };

export async function runEngine(input: EngineInput): Promise<EngineResult> {
  const setupStart = performance.now();
  // Compiles quickjs.wasm once per process (module-level cache inside loadQuickJSWasm).
  const wasm = await pi.loadQuickJSWasm();
  const names: Record<string, string> = {};
  for (const name of Object.keys(input.tools)) names[name] = pi.toCodemodeIdentifier(name);
  const sandbox = new pi.CodemodeSandbox({
    wasm,
    timeoutMs: input.limits.timeoutMs,
    tools: Object.entries(input.tools).map(([name, fn]) => ({
      name,
      jsName: names[name],
      execute: (args: unknown, ctx: { signal?: AbortSignal }) => fn(args, { signal: ctx?.signal }),
    })),
  });
  const prefix = `const __piTools = tools, __piNames = ${JSON.stringify(names)}; return await (async (tools) => {`;
  const suffix = "\n})({ call: async (name, args) => __piTools[__piNames[name]](args) });";
  const setupMs = performance.now() - setupStart;

  const runStart = performance.now();
  let result: any;
  let runMs: number;
  try {
    result = await sandbox.execute(prefix + input.code + suffix, { timeoutMs: input.limits.timeoutMs, signal: input.signal });
    runMs = performance.now() - runStart;
  } catch (error) {
    return {
      ok: false,
      error: { kind: "harness_error", message: error instanceof Error ? error.message : String(error) },
      setupMs,
      runMs: performance.now() - runStart,
    };
  } finally {
    // close() only aborts executions still running; this one has already resolved.
    await sandbox.close?.();
  }
  const engineTrace = { calls: result.calls, output: result.output, ...(result.ok ? {} : { error: result.error }) };
  if (result.ok) return { ok: true, output: result.value, engineTrace, setupMs, runMs };
  const kind = result.error?.kind as string;
  return {
    ok: false,
    error: {
      kind: KINDS[kind] ?? "engine_error",
      engineKind: kind,
      message: `${result.error?.name ? `${result.error.name}: ` : ""}${result.error?.message}`,
    },
    engineTrace,
    setupMs,
    runMs,
  };
}
