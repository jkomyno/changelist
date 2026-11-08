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
import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import type { AgentToolCallOutcome } from "@earendil-works/pi-agent-core";
import type { ExtensionAPI, ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { findExecutable, type HostReply, positive, type RunReport, runBridge, summarize } from "../shared/bridge-client.ts";

const TOOL = "rust_codemode";
/** Tools that orchestrate other tools are never reachable from guest code. */
const NEVER_AUTHORIZED = new Set([TOOL, "codemode", "tool_search"]);

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

async function runPi(
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
  const report = await runBridge({
    bin: findExecutable(flag("codemode-bin", "codemode"), ctx.cwd),
    deno: findExecutable(flag("codemode-deno", "deno"), ctx.cwd),
    cwd: ctx.cwd,
    code,
    tools: allowed,
    timeoutMs: positive("--codemode-timeout-ms", flag("codemode-timeout-ms", "30000")),
    concurrency: positive("--codemode-concurrency", flag("codemode-concurrency", "4")),
    maxCalls: positive("--codemode-max-calls", flag("codemode-max-calls", "32")),
    signal,
    // Pi applies argument validation and every extension's tool_call and tool_result handlers.
    // No executor_started fact: Pi's permission and approval handlers run inside
    // executeTool, so this adapter cannot see when the tool itself starts.
    execute: async (call, callSignal) =>
      toReply(await ctx.executeTool(call.tool, call.arguments, { signal: callSignal })),
  });
  return { report, allowed, unavailable };
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
      const { report, allowed, unavailable } = await runPi(pi, params.code, signal, ctx);
      const trace = pi.getFlag("codemode-trace");
      if (typeof trace === "string" && trace !== "") {
        writeFileSync(resolve(ctx.cwd, trace), JSON.stringify(report, null, 2) + "\n");
      }
      return {
        content: [{ type: "text", text: JSON.stringify(summarize(report)) }],
        details: { report, authorizedTools: allowed, unavailableTools: unavailable },
        isError: report.status !== "success",
      };
    },
  });
}
