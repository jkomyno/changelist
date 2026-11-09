/**
 * Demo-only Pi extension: a scripted model for the plan flow, so it runs offline
 * with no model credentials.
 *
 * The model makes one `rust_codemode_plan` call with the journal named by
 * CODEMODE_PI_PLAN_JOURNAL and the action in CODEMODE_PI_PLAN_ACTION (`run` or
 * `resume`), then replies with that tool's model-facing result.
 */
import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function planHost(pi: ExtensionAPI) {
  const journal = process.env.CODEMODE_PI_PLAN_JOURNAL;
  const action = process.env.CODEMODE_PI_PLAN_ACTION;
  if (!journal || (action !== "run" && action !== "resume")) {
    throw new Error("CODEMODE_PI_PLAN_JOURNAL and CODEMODE_PI_PLAN_ACTION (run or resume) must be set");
  }

  const model = fauxProvider({ provider: "codemode-plan-demo", models: [{ id: "scripted" }] });
  model.setResponses([
    fauxAssistantMessage(fauxToolCall("rust_codemode_plan", { journal, action }), { stopReason: "toolUse" }),
    (context) => {
      const last = context.messages.at(-1);
      const text =
        last?.role === "toolResult"
          ? last.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n")
          : "no tool result";
      return fauxAssistantMessage(`rust_codemode_plan returned ${text}`);
    },
  ]);
  pi.registerProvider(model.provider);
}
