/**
 * Demo-only Pi extension: a scripted model and a slow tool, so the integration
 * runs offline with no model credentials.
 *
 * The model makes one `rust_codemode` call whose source is the file named by
 * CODEMODE_PI_PROGRAM, then replies with that tool's model-facing result.
 */
import { readFileSync } from "node:fs";
import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

export default function offlineHost(pi: ExtensionAPI) {
  const program = process.env.CODEMODE_PI_PROGRAM;
  if (!program) throw new Error("CODEMODE_PI_PROGRAM must name a guest program");
  const code = readFileSync(program, "utf8");

  const model = fauxProvider({ provider: "codemode-demo", models: [{ id: "scripted" }] });
  model.setResponses([
    fauxAssistantMessage(fauxToolCall("rust_codemode", { code }), { stopReason: "toolUse" }),
    (context) => {
      const last = context.messages.at(-1);
      const text =
        last?.role === "toolResult"
          ? last.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n")
          : "no tool result";
      return fauxAssistantMessage(`rust_codemode returned ${text}`);
    },
  ]);
  pi.registerProvider(model.provider);

  pi.registerTool({
    name: "demo_wait",
    label: "Demo wait",
    description: "Wait for `ms` milliseconds unless aborted, then return `value`.",
    parameters: Type.Object({ ms: Type.Integer({ minimum: 0 }), value: Type.Optional(Type.String()) }),
    async execute(_id, params, signal) {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(resolve, params.ms);
        signal?.addEventListener(
          "abort",
          () => {
            clearTimeout(timer);
            pi.events.emit("codemode-demo:aborted", "demo_wait");
            reject(new Error("demo_wait aborted"));
          },
          { once: true },
        );
      });
      return { content: [{ type: "text", text: params.value ?? "waited" }], details: undefined };
    },
  });
}
