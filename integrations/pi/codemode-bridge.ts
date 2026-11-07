/**
 * Pi extension: run agent-written TypeScript through the Rust `codemode bridge`
 * and execute its tool calls as Pi nested tool calls.
 *
 * Verified against Pi 1.4.0. Boundaries:
 * - Rust owns admission, the deadline, call allowance, concurrency, output budget,
 *   call identities, and the execution record. The guest runs in restricted Deno.
 * - Pi owns tool execution. Each call goes through `ctx.executeTool()`, so argument
 *   validation, `tool_call` permission and approval handlers, and `tool_result`
 *   handlers run exactly as for model-issued calls.
 * - Only tools named in `--codemode-tools` that are currently callable in this
 *   session are authorized. Nothing is exposed implicitly.
 * - The bridge process starts with an empty environment, so Pi's provider
 *   credentials and other environment variables never reach Rust or the guest.
 * - Aborting this tool sends `cancel` to Rust and aborts outstanding nested calls.
 *   Calls already dispatched are recorded as unknown; their effects may remain.
 */
import { spawn } from "node:child_process";
import { accessSync, constants, writeFileSync } from "node:fs";
import { delimiter, isAbsolute, join, resolve } from "node:path";
import type { AgentToolCallOutcome } from "@earendil-works/pi-agent-core";
import type { ExtensionAPI, ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const TOOL = "rust_codemode";
/** Matches `BRIDGE_FRAME_BYTES` in the Rust crate. */
const HOST_FRAME_BYTES = 1024 * 1024;
/** Rust's per-guest-frame limit, which bounds source and each call's arguments. */
const GUEST_FRAME_BYTES = 64 * 1024;
const OUTPUT_BYTES = "262144";

/**
 * Bound on frames read from the bridge. The report holds the source, every call's
 * arguments, values and errors within the output budget, and per-call events.
 */
function reportLimit(maxCalls: number, outputBytes: number): number {
  return 2 * GUEST_FRAME_BYTES + maxCalls * (GUEST_FRAME_BYTES + 4096) + outputBytes + HOST_FRAME_BYTES;
}
const KILL_GRACE_MS = 2_000;
const STDERR_TAIL = 4_096;
/** Tools that orchestrate other tools are never reachable from guest code. */
const NEVER_AUTHORIZED = new Set([TOOL, "codemode", "tool_search"]);

type HostReply =
  | { ok: true; value: unknown }
  | { ok: false; error: { code: string; message: string } };

interface RunReport {
  status: string;
  output?: unknown;
  diagnostic?: string | null;
  calls: unknown[];
  [key: string]: unknown;
}

function findExecutable(name: string, cwd: string): string {
  if (isAbsolute(name)) return name;
  if (name.includes("/")) return resolve(cwd, name);
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, name);
    try {
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Keep searching.
    }
  }
  throw new Error(`${name} was not found on PATH; set it explicitly`);
}

function positive(name: string, value: string): string {
  if (!/^[1-9][0-9]*$/.test(value)) throw new Error(`--${name} must be a positive integer`);
  return value;
}

/** Convert Pi's nested-call outcome into a bridge reply. Rust records it verbatim. */
function toReply(outcome: AgentToolCallOutcome): HostReply {
  const { content, structuredContent } = outcome.result;
  const text = content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n");
  if (outcome.isError) {
    // Pi reports blocked calls, validation errors, unknown tools, and thrown errors this way.
    return { ok: false, error: { code: "tool_error", message: text || "tool failed" } };
  }
  if (structuredContent !== undefined) return { ok: true, value: structuredContent };
  if (content.every((block) => block.type === "text")) return { ok: true, value: text };
  return {
    ok: true,
    value: content.map((block) =>
      block.type === "text" ? block : { type: "image", mimeType: block.mimeType, omitted: true },
    ),
  };
}

function authorizedTools(configured: string, callable: readonly { name: string }[]) {
  const available = new Set(callable.map((tool) => tool.name));
  const requested = configured
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  const allowed = requested.filter((name) => available.has(name) && !NEVER_AUTHORIZED.has(name));
  const unavailable = requested.filter((name) => !allowed.includes(name));
  return { allowed: [...new Set(allowed)], unavailable };
}

function describe(allowed: readonly string[]): string {
  const names = allowed.length ? allowed.map((name) => `\`${name}\``).join(", ") : "none";
  return [
    "Run a TypeScript program in a restricted sandbox and return its JSON result.",
    "The program is an async function body: use `await` and `return`.",
    "Call host tools with `await tools.call(name, args)`; independent calls may run together with `Promise.all`.",
    `Authorized tools: ${names}. Other names reject with code \`tool_denied\`.`,
    "A failed call rejects with an Error carrying `code` and `message`. Calls are real and are not retried or undone.",
  ].join(" ");
}

async function runBridge(
  pi: ExtensionAPI,
  code: string,
  signal: AbortSignal | undefined,
  ctx: ExtensionToolContext,
): Promise<{ report: RunReport; allowed: string[]; unavailable: string[] }> {
  const flag = (name: string, fallback: string) => {
    const value = pi.getFlag(name);
    return typeof value === "string" && value !== "" ? value : fallback;
  };
  const { allowed, unavailable } = authorizedTools(flag("codemode-tools", ""), ctx.tools);
  const timeoutMs = positive("codemode-timeout-ms", flag("codemode-timeout-ms", "30000"));
  const maxCalls = positive("codemode-max-calls", flag("codemode-max-calls", "32"));
  const frameLimit = reportLimit(Number(maxCalls), Number(OUTPUT_BYTES));
  const args = [
    "bridge",
    "--timeout-ms",
    timeoutMs,
    "--concurrency",
    positive("codemode-concurrency", flag("codemode-concurrency", "4")),
    "--max-calls",
    maxCalls,
    "--max-output-bytes",
    OUTPUT_BYTES,
    "--deno",
    findExecutable(flag("codemode-deno", "deno"), ctx.cwd),
  ];
  const child = spawn(findExecutable(flag("codemode-bin", "codemode"), ctx.cwd), args, {
    cwd: ctx.cwd,
    env: {},
    stdio: ["pipe", "pipe", "pipe"],
  });

  const authorized = new Set(allowed);
  const outstanding = new Map<number, AbortController>();
  let stderr = "";
  let finished = false;
  let cancelled = false;
  let killTimer: NodeJS.Timeout | undefined;

  const write = (frame: unknown) => {
    if (finished || !child.stdin.writable) return;
    child.stdin.write(JSON.stringify(frame) + "\n");
  };
  const abortOutstanding = () => {
    for (const controller of outstanding.values()) controller.abort();
    outstanding.clear();
  };
  // Escalate only if the bridge does not exit on its own; never keep Pi alive for it.
  const terminate = () => {
    killTimer ??= setTimeout(() => child.kill("SIGKILL"), KILL_GRACE_MS).unref();
  };
  child.once("close", () => clearTimeout(killTimer));
  const onAbort = () => {
    cancelled = true;
    write({ type: "cancel" });
    abortOutstanding();
    terminate();
  };
  child.stdin.on("error", () => {
    // The bridge exited; its report or exit status explains why.
  });
  child.stderr.on("data", (chunk: Buffer) => {
    stderr = (stderr + chunk.toString("utf8")).slice(-STDERR_TAIL);
  });

  const reply = (id: number, result: HostReply) => {
    let frame = JSON.stringify({ type: "result", id, ...result });
    if (Buffer.byteLength(frame) >= HOST_FRAME_BYTES) {
      // Observed but too large to send: Rust records result_too_large and stops the run.
      frame = JSON.stringify({ type: "result", id, ok: result.ok, too_large: true });
    }
    if (!finished && child.stdin.writable) child.stdin.write(frame + "\n");
  };

  const onCall = (frame: { id: number; tool: string; arguments: unknown }) => {
    if (!Number.isSafeInteger(frame.id) || outstanding.has(frame.id)) {
      throw new Error("bridge sent an invalid or duplicate call id");
    }
    if (cancelled) {
      // A call frame buffered before the abort must not start a Pi tool afterwards.
      reply(frame.id, { ok: false, error: { code: "cancelled", message: "host cancelled before execution" } });
      return;
    }
    if (!authorized.has(frame.tool)) {
      reply(frame.id, { ok: false, error: { code: "tool_denied", message: "tool is not authorized by the host" } });
      return;
    }
    const controller = new AbortController();
    outstanding.set(frame.id, controller);
    ctx.executeTool(frame.tool, frame.arguments, { signal: controller.signal }).then(
      (outcome) => {
        if (outstanding.delete(frame.id)) reply(frame.id, toReply(outcome));
      },
      (error: unknown) => {
        const message = error instanceof Error ? error.message : String(error);
        if (outstanding.delete(frame.id)) reply(frame.id, { ok: false, error: { code: "tool_error", message } });
      },
    );
  };

  write({ type: "start", version: 1, code, tools: allowed });
  if (signal?.aborted) onAbort();
  signal?.addEventListener("abort", onAbort, { once: true });

  try {
    const report = await new Promise<RunReport>((resolvePromise, reject) => {
      let buffered = Buffer.alloc(0);
      let bridgeError: string | undefined;
      const fail = (error: Error) => {
        abortOutstanding();
        child.kill("SIGKILL");
        reject(error);
      };
      child.stdout.on("data", (chunk: Buffer) => {
        buffered = Buffer.concat([buffered, chunk]);
        let newline: number;
        while ((newline = buffered.indexOf(10)) !== -1) {
          const line = buffered.subarray(0, newline).toString("utf8");
          buffered = buffered.subarray(newline + 1);
          try {
            const frame = JSON.parse(line);
            if (frame.type === "call") onCall(frame);
            else if (frame.type === "report") {
              finished = true;
              abortOutstanding();
              child.stdin.end();
              terminate();
              resolvePromise(frame.report);
            } else if (frame.type === "error") bridgeError = String(frame.message);
            else throw new Error(`unexpected bridge frame type ${frame.type}`);
          } catch (error) {
            fail(error instanceof Error ? error : new Error(String(error)));
            return;
          }
        }
        if (buffered.length > frameLimit) fail(new Error(`bridge frame exceeds ${frameLimit} bytes`));
      });
      child.on("error", (error) => fail(error));
      child.on("close", (code, killSignal) => {
        if (finished) return;
        abortOutstanding();
        const reason = bridgeError ?? (stderr.trim() || `exit ${code ?? killSignal}`);
        reject(new Error(`codemode bridge ended without a report: ${reason}`));
      });
    });
    return { report, allowed, unavailable };
  } finally {
    signal?.removeEventListener("abort", onAbort);
    finished = true;
  }
}

export default function codemodeBridge(pi: ExtensionAPI) {
  pi.registerFlag("codemode-bin", { type: "string", description: "codemode executable (default: codemode on PATH)" });
  pi.registerFlag("codemode-deno", { type: "string", description: "Deno executable for the guest (default: deno on PATH)" });
  pi.registerFlag("codemode-tools", {
    type: "string",
    description: "Comma-separated Pi tools that rust_codemode programs may call (default: none)",
  });
  pi.registerFlag("codemode-timeout-ms", { type: "string", description: "Run deadline, including approvals (default: 30000)" });
  pi.registerFlag("codemode-concurrency", { type: "string", description: "Maximum concurrent tool calls (default: 4)" });
  pi.registerFlag("codemode-max-calls", { type: "string", description: "Maximum tool calls per run (default: 32)" });
  pi.registerFlag("codemode-trace", { type: "string", description: "Write each run's full JSON trace to this file" });

  pi.registerTool({
    name: TOOL,
    label: "Rust codemode",
    description: describe([]),
    parameters: Type.Object({
      code: Type.String({ description: "TypeScript async function body that calls tools.call(name, args)" }),
    }),
    // Model-only: guest programs and other orchestrators cannot reach this tool.
    exposure: "model-only",
    reachesTools: () => {
      const configured = pi.getFlag("codemode-tools");
      return typeof configured === "string" ? configured.split(",").map((name) => name.trim()).filter(Boolean) : [];
    },
    prepareLoadout(loadout) {
      const configured = pi.getFlag("codemode-tools");
      const { allowed } = authorizedTools(typeof configured === "string" ? configured : "", loadout.callable);
      return { descriptions: { [TOOL]: describe(allowed) } };
    },
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const { report, allowed, unavailable } = await runBridge(pi, params.code, signal, ctx);
      const trace = pi.getFlag("codemode-trace");
      if (typeof trace === "string" && trace !== "") {
        writeFileSync(resolve(ctx.cwd, trace), JSON.stringify(report, null, 2) + "\n");
      }
      const summary: Record<string, unknown> = { status: report.status, calls: report.calls.length };
      if ("output" in report) summary.output = report.output;
      if (report.diagnostic) summary.diagnostic = report.diagnostic;
      return {
        content: [{ type: "text", text: JSON.stringify(summary) }],
        details: { report, authorizedTools: allowed, unavailableTools: unavailable },
        isError: report.status !== "success",
      };
    },
  });
}
