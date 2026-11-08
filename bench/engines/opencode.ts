/**
 * OpenCode: `@opencode-ai/codemode` 1.19.5 (packages/codemode at tag v1.19.5, commit
 * 810c0eba1a73), imported from the pinned sparse checkout in bench/.cache/opencode with its
 * pinned dependencies (see bench/setup.sh). An in-process tree-walking interpreter: TS is
 * stripped with transpileModule, acorn parses, and an owned interpreter runs a restricted
 * JS subset. Requires Bun (the package is .ts source with ".js" import specifiers).
 *
 * Shims (the only non-upstream code on the execution path):
 *  1. Program shape. CodeMode exposes tools as `tools.<ns>.<tool>(input)`. The program body
 *     is wrapped in a guest-side prelude that rebinds `tools` to an object whose `call`
 *     forwards to the real tree (diagnostic line numbers shift by +2):
 *       const __root = tools;
 *       const __program = async (tools) => {\n<program>\n};
 *       return await __program({ call: (name, args) => { const p = name.split("."); return __root[p[0]][p[1]](args); } });
 *     Calls still go through the engine's dispatch: call accounting, the fixed 8-permit
 *     semaphore, copyIn/copyOut data boundaries, and diagnostics are intact.
 *  2. Tools. Each fixture tool becomes `Tool.make({ input: { type: "object" } (JSON Schema,
 *     render-only), run })`; `run` wraps the promise in Effect.promise((signal) => fn(args,
 *     { signal })) and maps failures to toolError(message), as OpenCode's own code-mode.ts
 *     does for MCP child calls. `signal` is Effect's per-fiber interruption signal.
 *  3. Observation. onToolCallStart/onToolCallEnd hooks record {index, name, input, outcome}
 *     for the ledger comparison. They fire neither outcome on interruption.
 *
 * Limits: `timeoutMs` and `maxToolCalls` are passed as CodeMode limits (cooperative timeout
 * via Effect.timeoutOrElse). Concurrency is fixed at 8 by the engine (TOOL_CALL_CONCURRENCY)
 * and cannot be configured. CodeMode's `maxOutputBytes` truncates the final model-facing
 * output instead of bounding tool replies, so it is not comparable and is not passed.
 */
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import type { EngineInfo, EngineInput, EngineResult, ErrorKind } from "./types.ts";

const CACHE = join(dirname(fileURLToPath(import.meta.url)), "../.cache/opencode");
const ENGINE = join(CACHE, "repo/packages/codemode/src/index.ts");
// The same `effect` instance the engine resolves (bench/.cache/opencode/node_modules).
// biome-ignore lint: Bun-only resolver; this engine requires Bun
const EFFECT = (globalThis as any).Bun.resolveSync("effect", CACHE);

// biome-ignore lint: dynamic imports of the pinned engine
const { Cause, Effect }: any = await import(pathToFileURL(EFFECT).href);
// biome-ignore lint: dynamic imports of the pinned engine
const { CodeMode, Tool, toolError }: any = await import(pathToFileURL(ENGINE).href);

export const info: EngineInfo = {
  name: "opencode",
  version: "@opencode-ai/codemode 1.19.5 (v1.19.5, 810c0eba1a73; effect 4.0.0-beta.83, acorn 8.15.0, typescript 5.8.2)",
  limits: {
    timeoutMs: "enforced by CodeMode limits.timeoutMs (cooperative, Effect.timeoutOrElse from execute())",
    maxCalls: "enforced by CodeMode limits.maxToolCalls",
    concurrency: "fixed at 8 by the engine (TOOL_CALL_CONCURRENCY); the requested value is not applied",
    maxOutputBytes: "not applied: CodeMode's maxOutputBytes truncates the final output and does not bound tool replies",
  },
  shim: "guest prelude rebinding tools.call(name, args) to tools.<ns>.<tool>(args); tools wrapped with Tool.make + Effect.promise",
};

const KINDS: Record<string, ErrorKind> = {
  TimeoutExceeded: "timeout",
  ExecutionFailure: "guest_error",
  ToolFailure: "guest_error",
  UnknownTool: "guest_error",
  InvalidToolInput: "guest_error",
  ToolCallLimitExceeded: "call_limit",
  ParseError: "unsupported",
  UnsupportedSyntax: "unsupported",
};

const PRELUDE_HEAD = "const __root = tools;\nconst __program = async (tools) => {\n";
const PRELUDE_TAIL =
  '\n};\nreturn await __program({ call: (name, args) => { const p = name.split("."); return __root[p[0]][p[1]](args); } });';

export async function runEngine(input: EngineInput): Promise<EngineResult> {
  const t0 = performance.now();
  const events: Array<Record<string, unknown>> = [];
  const now = () => +(performance.now() - t0).toFixed(3);

  const tree: Record<string, Record<string, unknown>> = {};
  for (const [name, fn] of Object.entries(input.tools)) {
    const [ns, tool] = name.split(".");
    tree[ns] ??= {};
    tree[ns][tool] = Tool.make({
      description: `Benchmark tool ${name}`,
      input: { type: "object" },
      run: (args: unknown) =>
        Effect.promise((signal: AbortSignal) => fn(args, { signal })).pipe(
          // Same failure projection as packages/opencode/src/tool/code-mode.ts for MCP child calls.
          Effect.catchCause((cause: unknown) => {
            if (Cause.hasInterruptsOnly(cause)) return Effect.interrupt;
            const error = Cause.squash(cause);
            return Effect.fail(toolError(error instanceof Error ? error.message : String(error), error));
          }),
        ),
    });
  }

  const runtime = CodeMode.make({
    tools: tree,
    limits: { timeoutMs: input.limits.timeoutMs, maxToolCalls: input.limits.maxCalls },
    onToolCallStart: ({ index, name, input: args }: any) =>
      Effect.sync(() => void events.push({ ev: "start", t: now(), index, name, input: args })),
    onToolCallEnd: ({ index, name, input: args, durationMs, outcome, message }: any) =>
      Effect.sync(() => void events.push({ ev: "end", t: now(), index, name, input: args, durationMs, outcome, message })),
  });
  const source = PRELUDE_HEAD + input.code + PRELUDE_TAIL;
  const setupMs = performance.now() - t0;

  const runStart = performance.now();
  let result: any;
  try {
    result = await Effect.runPromise(runtime.execute(source), input.signal ? { signal: input.signal } : undefined);
  } catch (error) {
    return {
      ok: false,
      error: { kind: "harness_error", message: error instanceof Error ? error.message : String(error) },
      setupMs,
      runMs: performance.now() - runStart,
    };
  }
  const runMs = performance.now() - runStart;
  const engineTrace = {
    toolCalls: result.toolCalls,
    events,
    ...(result.logs ? { logs: result.logs } : {}),
    ...(result.truncated ? { truncated: true } : {}),
    ...(result.ok ? {} : { diagnostic: result.error }),
  };
  if (result.ok) return { ok: true, output: result.value, engineTrace, setupMs, runMs };
  return {
    ok: false,
    error: { kind: KINDS[result.error.kind] ?? "engine_error", engineKind: result.error.kind, message: result.error.message },
    engineTrace,
    setupMs,
    runMs,
  };
}
