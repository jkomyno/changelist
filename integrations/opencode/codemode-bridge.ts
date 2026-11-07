/**
 * OpenCode plugin: run agent-written TypeScript through the Rust `codemode bridge`
 * and execute its tool calls with host tools behind OpenCode's permission gate.
 *
 * Verified against OpenCode 1.19.5. Boundaries:
 * - Rust owns admission, the deadline, call allowance, concurrency, output budget,
 *   call identities, and the execution record. The guest runs in restricted Deno.
 * - OpenCode 1.19.5 gives plugin tools no way to call its registered tools (built-in,
 *   plugin, or MCP). Guest calls therefore reach only the OpenCode custom tool
 *   definitions passed to `codemode({ tools })`. Nothing is exposed implicitly.
 * - Before each call the adapter asks OpenCode's permission service through
 *   `context.ask` for the permission `codemode_<tool>`, pattern `*`, as OpenCode's
 *   own code mode does for MCP tools. The prefix keeps host tools apart from
 *   OpenCode's built-in permission keys.
 * - A rule denial is a guest-visible call error. After a user rejection every later
 *   call is refused, and the rejection is rethrown when the program ends, so OpenCode
 *   handles it as for a rejected native call (by default, stopping the turn).
 * - Host tools run in the OpenCode process, so their credentials stay there. The
 *   bridge process starts with an empty environment.
 * - OpenCode's abort signal cancels the Rust run and aborts outstanding calls.
 *   Calls already dispatched are recorded as unknown; their effects may remain.
 *
 * Usage, in an OpenCode plugin file:
 *
 *     import { codemode } from "<this repository>/integrations/opencode/codemode-bridge.ts";
 *     export default codemode({ tools: { lookup }, bin: "/abs/path/to/codemode" });
 */
import { renameSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  type BridgeCall,
  findExecutable,
  type HostReply,
  positive,
  type RunReport,
  runBridge,
  summarize,
} from "../shared/bridge-client.ts";

const TOOL = "rust_codemode";
/** Permission key prefix for guest calls: `codemode_lookup` gates the `lookup` host tool. */
const PERMISSION_PREFIX = "codemode_";

/** The subset of OpenCode 1.19.5's plugin `ToolContext` this adapter uses. */
export interface ToolContext {
  sessionID: string;
  messageID: string;
  agent: string;
  directory: string;
  worktree: string;
  abort: AbortSignal;
  callID?: string;
  metadata(input: { title?: string; metadata?: Record<string, unknown> }): void;
  ask(input: { permission: string; patterns: string[]; always: string[]; metadata: Record<string, unknown> }): Promise<void>;
  [key: string]: unknown;
}

/**
 * An OpenCode custom tool definition, as returned by `tool()` from `@opencode-ai/plugin`.
 * As for OpenCode's own plugin tools, arguments are not validated before `execute`.
 */
export interface HostTool {
  description: string;
  args: Record<string, unknown>;
  execute(args: any, context: ToolContext): Promise<string | { output: string; title?: string; metadata?: unknown }>;
}

export interface CodemodeOptions {
  /** Host tools guest programs may call, keyed by guest-visible name. */
  tools: Record<string, HostTool>;
  /** `codemode` executable (default: `codemode` on PATH). */
  bin?: string;
  /** Deno executable for the guest (default: `deno` on PATH). */
  deno?: string;
  /** Run deadline in milliseconds, including time waiting for approvals (default 30000). */
  timeoutMs?: number;
  /** Maximum concurrent host calls (default 4). */
  concurrency?: number;
  /** Maximum tool calls per run (default 32). */
  maxCalls?: number;
  /** Write each run's full JSON report to this file, relative to the session directory. */
  trace?: string;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** OpenCode's error for a user (or non-interactive client) rejecting an approval. */
function isUserRejection(error: unknown): boolean {
  return error instanceof Error && error.name === "PermissionRejectedError";
}

/** Resolve when `promise` settles, or reject as soon as `signal` aborts. */
function untilAborted<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  if (signal.aborted) return Promise.reject(new Error("aborted"));
  return new Promise<T>((resolvePromise, reject) => {
    const onAbort = () => reject(new Error("aborted"));
    signal.addEventListener("abort", onAbort, { once: true });
    promise.then(resolvePromise, reject).finally(() => signal.removeEventListener("abort", onAbort));
  });
}

function describe(allowed: readonly string[]): string {
  const names = allowed.length ? allowed.map((name) => `\`${name}\``).join(", ") : "none";
  return [
    "Run a TypeScript program in a restricted sandbox and return its JSON result.",
    "The program is an async function body: use `await` and `return`.",
    "Call host tools with `await tools.call(name, args)`; independent calls may run together with `Promise.all`.",
    `Authorized tools: ${names}. Other names reject with code \`tool_denied\`.`,
    "Each call needs OpenCode permission; a denied call rejects with code `permission_denied`; after a rejected approval every call rejects with `permission_rejected`.",
    "A failed call rejects with an Error carrying `code` and `message`. Calls are real and are not retried or undone.",
  ].join(" ");
}

/** Create an OpenCode server plugin that registers the `rust_codemode` tool. */
export function codemode(options: CodemodeOptions) {
  const tools = new Map(Object.entries(options.tools));
  const allowed = [...tools.keys()];
  const timeoutMs = positive("timeoutMs", options.timeoutMs ?? 30_000);
  const concurrency = positive("concurrency", options.concurrency ?? 4);
  const maxCalls = positive("maxCalls", options.maxCalls ?? 32);

  return async (input: { directory: string }) => ({
    tool: {
      [TOOL]: {
        description: describe(allowed),
        args: { code: { type: "string", description: "TypeScript async function body that calls tools.call(name, args)" } },
        async execute(args: { code?: unknown }, context: ToolContext) {
          if (typeof args?.code !== "string") throw new Error("code must be a string");
          const cwd = context.directory ?? input.directory;
          // Set by the first rejected approval; every later call is refused.
          let rejection: unknown;

          /** OpenCode permission, then the host tool. Nothing runs after an abort or a rejection. */
          const execute = async (call: BridgeCall, signal: AbortSignal): Promise<HostReply> => {
            const refused = (): HostReply => ({
              ok: false,
              error: { code: "permission_rejected", message: "an earlier approval in this run was rejected" },
            });
            if (rejection !== undefined) return refused();
            try {
              await untilAborted(
                context.ask({
                  permission: PERMISSION_PREFIX + call.tool,
                  patterns: ["*"],
                  // "Always" approves later calls of this host tool while OpenCode runs.
                  always: ["*"],
                  metadata: { tool: call.tool, codemodeCall: call.id, arguments: call.arguments },
                }),
                signal,
              );
            } catch (error) {
              if (isUserRejection(error) && !signal.aborted) {
                rejection ??= error;
                return { ok: false, error: { code: "permission_rejected", message: message(error) } };
              }
              // Denied by a rule or corrected with feedback. After an abort the bridge
              // ignores this reply and records the call as unknown; it never runs.
              return { ok: false, error: { code: "permission_denied", message: message(error) } };
            }
            if (signal.aborted) {
              return { ok: false, error: { code: "cancelled", message: "host cancelled before execution" } };
            }
            if (rejection !== undefined) return refused();
            const result = await tools.get(call.tool)!.execute(call.arguments, {
              ...context,
              abort: signal,
              callID: `${context.callID ?? TOOL}/${call.id}`,
            });
            return { ok: true, value: typeof result === "string" ? result : result.output };
          };

          let report: RunReport;
          try {
            report = await runBridge({
              bin: findExecutable(options.bin ?? "codemode", cwd),
              deno: findExecutable(options.deno ?? "deno", cwd),
              cwd,
              code: args.code,
              tools: allowed,
              timeoutMs,
              concurrency,
              maxCalls,
              signal: context.abort,
              execute,
            });
            if (options.trace) {
              // Write then rename, so a reader never sees a partial report.
              const path = resolve(cwd, options.trace);
              writeFileSync(`${path}.partial`, JSON.stringify(report, null, 2) + "\n");
              renameSync(`${path}.partial`, path);
            }
          } catch (error) {
            throw rejection ?? error;
          }
          // OpenCode stops the turn on its own rejection error, as for a native tool call.
          if (rejection !== undefined) throw rejection;
          const summary = JSON.stringify(summarize(report));
          // A failed run is an OpenCode tool error; the model still sees the summary.
          if (report.status !== "success") throw new Error(summary);
          return { title: TOOL, output: summary, metadata: { status: report.status, calls: report.calls.length } };
        },
      },
    },
  });
}
