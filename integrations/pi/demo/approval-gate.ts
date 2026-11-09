/**
 * Demo-only Pi extension: an independent permission gate and audit log.
 *
 * It sees every tool call, including nested calls made for rust_codemode
 * programs, because Pi routes `ctx.executeTool()` through `tool_call` handlers.
 * `bash` and `issues_add_comment` need interactive approval; without a UI they are
 * blocked (fail closed). When CODEMODE_PI_AUDIT is set, each decision is appended
 * to that JSONL file, with the call's `attempt_key` argument when it has one.
 */
import { appendFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const NEEDS_APPROVAL = new Set(["bash", "issues_add_comment"]);

export default function approvalGate(pi: ExtensionAPI) {
  const audit = (entry: Record<string, unknown>) => {
    const path = process.env.CODEMODE_PI_AUDIT;
    if (path) appendFileSync(path, JSON.stringify(entry) + "\n");
  };
  pi.events.on("codemode-demo:aborted", (tool) => audit({ event: "aborted", tool }));
  pi.on("tool_call", async (event, ctx) => {
    let decision = "allowed";
    if (NEEDS_APPROVAL.has(event.toolName)) {
      const detail = event.toolName === "bash" ? String(event.input.command) : JSON.stringify(event.input);
      const approved = ctx.hasUI && (await ctx.ui.confirm(`Allow ${event.toolName}?`, detail));
      if (!approved) decision = "blocked";
    }
    const attempt = typeof event.input.attempt_key === "string" ? { attempt: event.input.attempt_key } : {};
    audit({ event: "tool_call", tool: event.toolName, nested: event.parentToolCallId !== undefined, decision, ...attempt });
    if (decision === "blocked") {
      return { block: true, reason: `${event.toolName} requires approval, which is unavailable here` };
    }
    return undefined;
  });
}
