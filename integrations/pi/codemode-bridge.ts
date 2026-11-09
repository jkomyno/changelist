/**
 * Pi extension: run agent-written TypeScript through the Rust `codemode bridge`
 * and execute its tool calls as Pi nested tool calls.
 *
 * Verified against Pi 1.4.0. Boundaries:
 * - Rust owns admission, the deadline, call allowance, concurrency, output budget,
 *   call identities, and the execution record. The guest runs in a restricted V8 worker.
 * - Pi owns tool execution. Each call goes through `ctx.executeTool()`, so argument
 *   validation, `tool_call` permission and approval handlers, and `tool_result`
 *   handlers run exactly as for model-issued calls.
 * - Only tools named in `--codemode-tools` that are currently callable in this
 *   session are authorized. Nothing is exposed implicitly.
 * - The bridge process starts with an empty environment, so Pi's provider
 *   credentials and other environment variables never reach Rust or the guest.
 * - Aborting this tool sends `cancel` to Rust and aborts outstanding nested calls.
 *   Calls already dispatched are recorded as unknown; their effects may remain.
 *
 * `rust_codemode_plan` runs or resumes an approved operation plan the same way:
 * Rust owns the journal and dispatches each attempt through `ctx.executeTool()`, so
 * Pi's gates still decide every call; plan approval supplements them. The provider
 * declaration comes from `--codemode-plan-declaration`, never from the model.
 * Provider extensions register attempt keys, preconditions, and reconcilers on
 * the `codemode:plan-providers` event (see `PlanProviderRegistry`).
 */
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import type { AgentToolCallOutcome } from "@earendil-works/pi-agent-core";
import type { ExtensionAPI, ExtensionToolContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
  findExecutable,
  type HostReply,
  type PlanCall,
  type PlanOutcome,
  type PlanReply,
  type PlanScope,
  type ProviderDeclaration,
  positive,
  type ReconcileRequest,
  type Authorized,
  type Reconciled,
  type RunReport,
  runBridge,
  runPlanBridge,
  summarize,
} from "../shared/bridge-client.ts";

const TOOL = "rust_codemode";
const PLAN_TOOL = "rust_codemode_plan";
/** Tools that orchestrate other tools are never reachable from guest code or plans. */
const NEVER_AUTHORIZED = new Set([TOOL, PLAN_TOOL, "codemode", "tool_search"]);

/** Pi event on which provider extensions register plan support for their tools. */
export const PLAN_PROVIDERS_EVENT = "codemode:plan-providers";

/** What a provider extension registers for one of its Pi tools. */
export interface PlanToolProvider {
  /** Argument that receives the attempt key. Required for tools that reconcile by receipt. */
  keyArgument?: string;
  /** Argument that receives the operation's preconditions. Required for operations that have any. */
  preconditionsArgument?: string;
  /**
   * Argument that receives the approved scope. Required: the tool must refuse a scope
   * that is not the account and destination it acts as, so a changed host
   * configuration cannot act under an approval for another scope.
   */
  scopeArgument?: string;
  /** Whether the tool's account may act on `scope` now. Required for plan sessions. */
  authorize?(scope: PlanScope, signal: AbortSignal): Promise<Authorized>;
  /** Answer a probe. Required for tools that reconcile by receipt or state. */
  reconcile?(request: ReconcileRequest, signal: AbortSignal): Promise<Reconciled>;
}

/**
 * Emitted with `PLAN_PROVIDERS_EVENT` each time a plan session starts. Listeners
 * must call `register` synchronously, so extension load order does not matter.
 */
export interface PlanProviderRegistry {
  register(tool: string, provider: PlanToolProvider): void;
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

function readDeclaration(path: string): ProviderDeclaration {
  const value = JSON.parse(readFileSync(path, "utf8"));
  const valid =
    typeof value?.provider === "string" &&
    typeof value?.account === "string" &&
    typeof value?.contracts === "object" &&
    value.contracts !== null;
  if (!valid) throw new Error(`${path} is not a provider declaration`);
  return value;
}

/** Collect the plan providers that extensions register for this session. */
function planProviders(pi: ExtensionAPI): Map<string, PlanToolProvider> {
  const providers = new Map<string, PlanToolProvider>();
  const registry: PlanProviderRegistry = {
    register(tool, provider) {
      providers.set(tool, provider);
    },
  };
  pi.events.emit(PLAN_PROVIDERS_EVENT, registry);
  return providers;
}

/** Refused before Pi saw the attempt: a person must fix the configuration. */
const refused = (code: string, message: string): PlanReply => ({ ok: false, error: { code, message }, effect: "refused" });

async function runPiPlan(
  pi: ExtensionAPI,
  params: { journal: string; action: "run" | "resume" },
  signal: AbortSignal | undefined,
  ctx: ExtensionToolContext,
) {
  const flag = (name: string, fallback: string) => {
    const value = pi.getFlag(name);
    return typeof value === "string" && value !== "" ? value : fallback;
  };
  const declarationPath = flag("codemode-plan-declaration", "");
  if (!declarationPath) throw new Error("--codemode-plan-declaration must name the provider declaration file");
  const declaration = readDeclaration(resolve(ctx.cwd, declarationPath));
  const providers = planProviders(pi);
  const callable = new Set(ctx.tools.map((tool) => tool.name));

  const execute = async (call: PlanCall, callSignal: AbortSignal): Promise<PlanReply> => {
    const contract = declaration.contracts[call.tool];
    if (!contract || NEVER_AUTHORIZED.has(call.tool) || !callable.has(call.tool)) {
      // Never sent to Pi; a session where the tool is callable may send it.
      const message = `${call.tool} is not declared or not callable in this Pi session`;
      return { ok: false, error: { code: "tool_unavailable", message }, effect: "not_applied" };
    }
    const provider = providers.get(call.tool);
    const args: Record<string, unknown> = { ...call.arguments };
    if (provider?.keyArgument) {
      if (provider.keyArgument in args) {
        return refused("argument_conflict", `the plan's arguments already set ${provider.keyArgument}`);
      }
      args[provider.keyArgument] = call.attempt;
    } else if (contract.reconcile === "receipt") {
      // Without the key the provider keeps no receipt, and a probe would wrongly find nothing.
      return refused("no_key_argument", `no provider registered an attempt-key argument for ${call.tool}`);
    }
    if (!provider?.scopeArgument || provider.scopeArgument in args) {
      return refused("scope_unverifiable", `no provider registered a scope argument for ${call.tool}`);
    }
    args[provider.scopeArgument] = call.scope;
    if (call.preconditions.length > 0) {
      if (!provider?.preconditionsArgument || provider.preconditionsArgument in args) {
        return refused("preconditions_unsupported", `no provider registered a preconditions argument for ${call.tool}`);
      }
      args[provider.preconditionsArgument] = call.preconditions;
    }
    // Pi applies argument validation and every extension's tool_call and tool_result handlers.
    const reply = toReply(await ctx.executeTool(call.tool, args, { signal: callSignal }));
    // Pi reports gate blocks, validation errors, and thrown errors alike: the effect is unknown.
    return reply.ok ? reply : { ...reply, effect: "unknown" };
  };

  const result = await runPlanBridge({
    bin: findExecutable(flag("codemode-bin", "codemode"), ctx.cwd),
    cwd: ctx.cwd,
    journal: resolve(ctx.cwd, params.journal),
    kind: params.action,
    declaration,
    signal,
    // The approval was checked by Rust. Each declared tool's provider confirms the scope
    // against its live account; Pi's gates still decide each call at dispatch time.
    authorize: async (scope: PlanScope): Promise<Authorized> => {
      for (const tool of Object.keys(declaration.contracts)) {
        const authorize = providers.get(tool)?.authorize;
        if (!authorize) {
          return { ok: false, error: { code: "unauthorized", message: `no provider can confirm ${tool} may act as this scope` } };
        }
        const answer = await authorize(scope, signal ?? new AbortController().signal);
        if (!answer.ok) return answer;
      }
      return { ok: true };
    },
    execute,
    reconcile: async (request, probeSignal) => {
      const reconcile = providers.get(request.tool)?.reconcile;
      if (!reconcile) {
        return { error: { code: "no_reconciler", message: `no provider registered a reconciler for ${request.tool}` } };
      }
      return reconcile(request, probeSignal);
    },
  });
  return result;
}

/** Model-facing summary: session, status, and how many operations reached each conclusion. */
function summarizePlan(outcome: PlanOutcome, diagnostic: string | undefined): Record<string, unknown> {
  const counts: Record<string, number> = {};
  for (const operation of outcome.operations ?? []) counts[operation.conclusion] = (counts[operation.conclusion] ?? 0) + 1;
  const review = (outcome.operations ?? []).filter((operation) => operation.next === "review").map((operation) => operation.operation);
  return {
    session: outcome.session,
    status: outcome.status,
    counts,
    ...(review.length ? { review } : {}),
    ...(outcome.detail ? { detail: outcome.detail } : {}),
    ...(diagnostic ? { diagnostic } : {}),
  };
}

export default function codemodeBridge(pi: ExtensionAPI) {
  pi.registerFlag("codemode-bin", { type: "string", description: "codemode executable (default: codemode on PATH)" });
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

  pi.registerFlag("codemode-plan-declaration", {
    type: "string",
    description: "Provider declaration file for rust_codemode_plan; approve plans with the same file",
  });
  pi.registerTool({
    name: PLAN_TOOL,
    label: "Rust codemode plan",
    description: [
      "Run or resume an approved operation plan, recorded in a journal, through this session's tools.",
      "`run` executes a plan once; `resume` establishes uncertain outcomes first and continues safely recoverable work.",
      "Every operation still passes this session's permission checks. Operations whose outcome is unknown and",
      "cannot be established are left for a person to review; they are never repeated automatically.",
    ].join(" "),
    parameters: Type.Object({
      journal: Type.String({ description: "Journal file of an approved plan (codemode plan approve)" }),
      action: Type.Union([Type.Literal("run"), Type.Literal("resume")]),
    }),
    // Model-only: guest programs, plans, and other orchestrators cannot reach this tool.
    exposure: "model-only",
    reachesTools: () => {
      const path = pi.getFlag("codemode-plan-declaration");
      try {
        return typeof path === "string" && path !== "" ? Object.keys(readDeclaration(path).contracts) : [];
      } catch {
        return [];
      }
    },
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const { outcome, diagnostic } = await runPiPlan(pi, params, signal, ctx);
      return {
        content: [{ type: "text", text: JSON.stringify(summarizePlan(outcome, diagnostic)) }],
        details: { outcome, diagnostic },
        isError: outcome.status === "refused" || outcome.status === "interrupted",
      };
    },
  });
}
