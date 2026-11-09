/**
 * Demo-only Pi extension: the offline issue-tracker fixture as Pi tools, and their
 * plan support for `rust_codemode_plan`.
 *
 * Each tool runs `codemode-fixture call` (CODEMODE_FIXTURE_BIN) against the fixture
 * database (CODEMODE_FIXTURE_DB) as one account (CODEMODE_FIXTURE_ACCOUNT). The
 * fixture keeps a receipt per attempt key, so the tools accept an `attempt_key`
 * argument and register it, with a reconciler that runs `codemode-fixture reconcile`.
 * Pi tool names use underscores; the fixture's own names use dots.
 */
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { Authorized, PlanScope, ReconcileRequest, Reconciled } from "../../shared/bridge-client.ts";
import type { PlanProviderRegistry } from "../codemode-bridge.ts";

const TOOLS = {
  issues_add_label: {
    fixture: "issues.add_label",
    description: "Add a label to an issue.",
    fields: { issue: Type.Integer({ minimum: 1 }), label: Type.String() },
  },
  issues_remove_label: {
    fixture: "issues.remove_label",
    description: "Remove a label from an issue.",
    fields: { issue: Type.Integer({ minimum: 1 }), label: Type.String() },
  },
  issues_add_comment: {
    fixture: "issues.add_comment",
    description: "Post a comment on an issue.",
    fields: { issue: Type.Integer({ minimum: 1 }), body: Type.String() },
  },
} as const;

function settings() {
  const read = (name: string) => {
    const value = process.env[name];
    if (!value) throw new Error(`${name} must be set for the fixture tools`);
    return value;
  };
  return { bin: read("CODEMODE_FIXTURE_BIN"), db: read("CODEMODE_FIXTURE_DB"), account: read("CODEMODE_FIXTURE_ACCOUNT") };
}

/** Run the fixture CLI with an empty environment and parse its one JSON line. */
function fixture(args: string[], signal?: AbortSignal): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    // No timeout: a lost response stays pending until Pi aborts the call.
    const child = spawn(settings().bin, args, { env: {}, stdio: ["ignore", "pipe", "pipe"], signal });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString("utf8")));
    child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString("utf8")));
    child.on("error", reject);
    child.on("close", (code) => {
      try {
        resolve(JSON.parse(stdout));
      } catch {
        reject(new Error(`codemode-fixture exited ${code}: ${stderr.trim() || stdout.trim()}`));
      }
    });
  });
}

export default function fixtureTools(pi: ExtensionAPI) {
  for (const [name, tool] of Object.entries(TOOLS)) {
    pi.registerTool({
      name,
      label: name,
      description: `${tool.description} Acts on the offline fixture tracker.`,
      parameters: Type.Object({
        ...tool.fields,
        attempt_key: Type.Optional(Type.String({ description: "Attempt key for the provider's receipt" })),
        preconditions: Type.Optional(Type.Array(Type.Unknown())),
        scope: Type.Optional(Type.Unknown()),
      }),
      async execute(_id, params, signal) {
        const { attempt_key, preconditions, scope, ...fields } = params as Record<string, unknown>;
        const { db, account } = settings();
        const args = ["call", "--db", db, "--account", account, "--tool", tool.fixture];
        args.push("--key", typeof attempt_key === "string" ? attempt_key : `pi-${randomUUID()}`);
        args.push("--arguments", JSON.stringify(fields));
        if (Array.isArray(preconditions)) args.push("--preconditions", JSON.stringify(preconditions));
        // The fixture refuses a scope that is not this account and repository.
        if (scope !== undefined) args.push("--scope", JSON.stringify(scope));
        const result = await fixture(args, signal);
        if (result.ok !== true) {
          const error = result.error as { code?: string; message?: string } | undefined;
          throw new Error(`${error?.code ?? "fixture_error"}: ${error?.message ?? "call failed"} (effect ${String(result.effect)})`);
        }
        return { content: [{ type: "text", text: JSON.stringify(result.value) }], details: result.value };
      },
    });
  }

  pi.events.on("codemode:plan-providers", (registry) => {
    for (const [name, tool] of Object.entries(TOOLS)) {
      (registry as PlanProviderRegistry).register(name, {
        keyArgument: "attempt_key",
        preconditionsArgument: "preconditions",
        scopeArgument: "scope",
        async authorize(scope: PlanScope, signal: AbortSignal): Promise<Authorized> {
          const { db, account } = settings();
          const result = await fixture(["authorize", "--db", db, "--account", account, "--scope", JSON.stringify(scope)], signal);
          if (result.ok === true) return { ok: true };
          return { ok: false, error: (result.error as { code: string; message: string }) ?? { code: "unauthorized", message: "refused" } };
        },
        async reconcile(request: ReconcileRequest, signal: AbortSignal): Promise<Reconciled> {
          const { db, account } = settings();
          const args = ["reconcile", "--db", db, "--account", account, "--tool", tool.fixture];
          args.push("--arguments", JSON.stringify(request.arguments), "--keys", request.keys.join(","));
          // The fixture refuses to read state or fence keys outside the approved scope.
          args.push("--scope", JSON.stringify(request.scope));
          const result = await fixture(args, signal);
          if (typeof result.verdict === "string") return { verdict: result.verdict as never, evidence: String(result.evidence) };
          return { error: (result.error as { code: string; message: string }) ?? { code: "reconcile_failed", message: "no verdict" } };
        },
      });
    }
  });
}
