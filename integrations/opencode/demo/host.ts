/**
 * Demo-only OpenCode plugin: the codemode adapter with four host tools, a scripted
 * model so OpenCode runs offline, and an audit of OpenCode's permission events.
 * Configured through environment variables so the same file serves the demo
 * script and the integration tests.
 *
 * The model calls `rust_codemode` once with the file named by CODEMODE_OC_PROGRAM,
 * then replies with that tool's model-facing result.
 *
 * - `lookup` uses a host credential (CODEMODE_DEMO_TOKEN) that never reaches the guest.
 * - `publish` needs approval and `purge` is denied by the demo's OpenCode rules.
 * - `wait` is slow and reports when OpenCode aborts it.
 */
import { appendFileSync, readFileSync } from "node:fs";
import { codemode, type HostTool } from "../codemode-bridge.ts";

const audit = (entry: Record<string, unknown>) => {
  const path = process.env.CODEMODE_OC_AUDIT;
  if (path) appendFileSync(path, JSON.stringify(entry) + "\n");
};

const STOCK: Record<string, number> = { "A-1": 3, "B-2": 0 };

const lookup: HostTool = {
  description: "Look up stock for a SKU using the host's inventory credential.",
  args: { sku: { type: "string" } },
  async execute(args: { sku: string }) {
    if (!process.env.CODEMODE_DEMO_TOKEN) throw new Error("inventory credential is not configured");
    await new Promise((resolve) => setTimeout(resolve, args.sku === "A-1" ? 150 : 50));
    return JSON.stringify({ sku: args.sku, stock: STOCK[args.sku] ?? 0 });
  },
};

const publish: HostTool = {
  description: "Publish a message. Requires approval.",
  args: { message: { type: "string" } },
  async execute(args: { message: string }) {
    audit({ event: "executed", tool: "publish" });
    return `published ${args.message}`;
  },
};

const purge: HostTool = {
  description: "Purge the inventory. Denied by configuration.",
  args: {},
  async execute() {
    audit({ event: "executed", tool: "purge" });
    return "purged";
  },
};

const wait: HostTool = {
  description: "Wait for `ms` milliseconds unless aborted, then return `value`.",
  args: { ms: { type: "number" }, value: { type: "string" } },
  async execute(args: { ms: number; value?: string }, context) {
    audit({ event: "started", tool: "wait", session: context.sessionID });
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(resolve, args.ms);
      context.abort.addEventListener(
        "abort",
        () => {
          clearTimeout(timer);
          audit({ event: "aborted", tool: "wait" });
          reject(new Error("wait aborted"));
        },
        { once: true },
      );
    });
    return args.value ?? "waited";
  },
};

const bridge = codemode({
  tools: { lookup, publish, purge, wait },
  bin: process.env.CODEMODE_OC_BIN,
  deno: process.env.CODEMODE_OC_DENO,
  trace: process.env.CODEMODE_OC_TRACE,
  timeoutMs: 20_000,
});

/** One OpenAI-compatible streaming response: a tool call or final text. */
function completion(turn: { tool: string; args: unknown } | { text: string }): Response {
  const chunk = (delta: object, finish: string | null = null) =>
    `data: ${JSON.stringify({ id: "demo", object: "chat.completion.chunk", created: 0, model: "scripted", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  const body =
    "tool" in turn
      ? chunk({ role: "assistant", content: null }) +
        chunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: turn.tool, arguments: JSON.stringify(turn.args) } }] }) +
        chunk({}, "tool_calls")
      : chunk({ role: "assistant", content: turn.text }) + chunk({}, "stop");
  return new Response(body + "data: [DONE]\n\n", { headers: { "content-type": "text/event-stream" } });
}

/** A loopback model endpoint inside the OpenCode process; no credentials or network. */
function scriptedModel(code: string) {
  return Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request: Request) {
      const body = await request.json();
      // OpenCode also asks for a session title, without tools.
      if (!body.tools?.length) return completion({ text: "codemode demo" });
      const last = body.messages.at(-1);
      const reply = last?.role === "tool" ? "text" : "tool";
      audit({ event: "model", reply });
      if (reply === "tool") return completion({ tool: "rust_codemode", args: { code } });
      return completion({ text: `rust_codemode returned ${String(last.content)}` });
    },
  });
}

declare const Bun: { serve(options: object): { port: number; stop(force?: boolean): void } };

export default async function host(input: { directory: string }) {
  const program = process.env.CODEMODE_OC_PROGRAM;
  if (!program) throw new Error("CODEMODE_OC_PROGRAM must name a guest program");
  const model = scriptedModel(readFileSync(program, "utf8"));
  return {
    ...(await bridge(input)),
    async config(config: { provider?: Record<string, unknown> }) {
      config.provider = {
        ...config.provider,
        "codemode-demo": {
          name: "codemode demo",
          npm: "@ai-sdk/openai-compatible",
          options: { apiKey: "unused", baseURL: `http://127.0.0.1:${model.port}/v1` },
          models: { scripted: { name: "scripted", tool_call: true, limit: { context: 100000, output: 10000 } } },
        },
      };
    },
    // OpenCode's own record of approval requests and replies, independent of the adapter.
    async event({ event }: { event: { type: string; properties: Record<string, any> } }) {
      if (event.type === "permission.asked") {
        const { id, sessionID, permission } = event.properties;
        audit({ event: event.type, permission, id, session: sessionID });
      } else if (event.type === "permission.replied") {
        audit({ event: event.type, reply: event.properties.reply });
      }
    },
    async dispose() {
      model.stop(true);
    },
  };
}
