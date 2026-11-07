/**
 * Demo-only Pi extension: an independent permission gate and audit log.
 *
 * It sees every tool call, including nested calls made for rust_codemode
 * programs, because Pi routes `ctx.executeTool()` through `tool_call` handlers.
 * `bash` needs interactive approval; without a UI it is blocked (fail closed).
 * When CODEMODE_PI_AUDIT is set, each decision is appended to that JSONL file.
 */
import { appendFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function approvalGate(pi: ExtensionAPI) {
  const audit = (entry: Record<string, unknown>) => {
    const path = process.env.CODEMODE_PI_AUDIT;
    if (path) appendFileSync(path, JSON.stringify(entry) + "\n");
  };
  pi.events.on("codemode-demo:aborted", (tool) => audit({ event: "aborted", tool }));
  pi.on("tool_call", async (event, ctx) => {
    let decision = "allowed";
    if (event.toolName === "bash") {
      const approved = ctx.hasUI && (await ctx.ui.confirm("Allow bash?", String(event.input.command)));
      if (!approved) decision = "blocked";
    }
    audit({ event: "tool_call", tool: event.toolName, nested: event.parentToolCallId !== undefined, decision });
    if (decision === "blocked") return { block: true, reason: "bash requires approval, which is unavailable here" };
    return undefined;
  });
}
