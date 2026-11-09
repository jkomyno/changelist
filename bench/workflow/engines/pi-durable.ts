// Pi Durable 1.5.0 adapter: one durable code mode run over SQLite storage.
//
// Pi Durable has no CLI and executes code only through a model turn, so the pi-ai faux provider
// plays the model with a stateless response: no tool result in the transcript -> one `codemode`
// tool call carrying the program; otherwise plain text. Stateless matters: after a SIGKILL the
// next process must answer the next generation too.
//
// Each GitHub tool is a durable tool with the default replay policy ("unsafe" for writes: an
// execution a crash interrupts is not rerun and settles as "interrupted and may have partially
// run"); reads are declared replay "safe". A restart runs the same command on the same run
// directory: the same submission requestId finds the admitted submission, and waiting on it
// resumes the code mode task, which replays the program against its journal.
//
// Limits: host default passes none (no deadline, no memory limit, restartLimit 5). The
// equal-limits row sets the deadline the only way Pi offers for durable runs, a first-line
// `// @options: {"timeout_ms":N}`, and `memoryLimitBytes` on the sandbox.
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { adapterOptions, EXIT, outcomeClaims, parseOutcomes, PKGS, program, WIDEN_OP, writeClaims } from "../lib/adapter.ts";
import { call, providerClient } from "../lib/client.ts";
import { ACCOUNT, operationId, READ_SLUGS, WRITE_SLUGS } from "../lib/github-state.ts";
import { pkgImport } from "../lib/pkg-import.ts";

const options = adapterOptions();
const root = join(PKGS, "pi-durable");
const dir = join(options.runDir, "pi");
mkdirSync(dir, { recursive: true });
const execute = providerClient(options.provider);
const say = (tag: string, value: unknown) => process.stdout.write(`@@${tag} ${JSON.stringify(value)}\n`);

const extra = options.widen ? JSON.stringify([WIDEN_OP]) : "[]";
const prelude = "async function gh(slug, args) { const r = await tools[slug](args); return typeof r === 'string' ? JSON.parse(r) : r; }";
let code = program(options.workload, prelude, `await decideAndApply((id) => "<!-- pi-durable:" + id + " -->", ${extra});`);
if (options.deadlineMs !== undefined) code = `// @options: {"timeout_ms": ${options.deadlineMs}}\n${code}`;

const { BACKGROUND_CONTEXT: context } = await pkgImport(root, "@earendil-works/chord/context");
const { createModels } = await pkgImport(root, "@earendil-works/pi-ai/models");
const { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } = await pkgImport(root, "@earendil-works/pi-ai/providers/faux");
const { createRegistry, defineExtension, defineTool, Harness, ToolResultEntry } = await pkgImport(root, "@earendil-works/pi-durable");
const { openNodeSqliteStorage } = await pkgImport(root, "@earendil-works/pi-durable/storage/sqlite/node");
const { createCodemodeExtension } = await pkgImport(root, "@earendil-works/pi-durable/codemode");
const { CodemodeSandbox } = await pkgImport(root, "@earendil-works/pi-codemode");

const anyArgs = { type: "object", additionalProperties: true };
const tools = [...READ_SLUGS, ...WRITE_SLUGS].map((slug) =>
  defineTool({
    name: slug,
    description: `GitHub ${slug}`,
    parameters: anyArgs,
    structuredOutputSchema: {},
    // Reads may rerun after a crash; writes keep Pi's default ("unsafe": never rerun).
    ...(READ_SLUGS.includes(slug) ? { replay: "safe" } : {}),
    execute: async (args: Record<string, unknown>, _api: unknown, ctx: { abortSignal?: AbortSignal }) => {
      const data = await call(execute, slug, args, ACCOUNT, ctx.abortSignal);
      return { output: [{ type: "text", text: JSON.stringify(data) }], structuredOutput: data };
    },
  }),
);

const registry = createRegistry();
registry.install(defineExtension({ name: "github", tools }));
const sandboxOptions: Record<string, number> = {};
if (options.heapMb !== undefined) sandboxOptions.memoryLimitBytes = options.heapMb * 1024 * 1024;
const sandbox = new CodemodeSandbox(sandboxOptions);
registry.install(createCodemodeExtension({ sandbox, models: false }));

const faux = fauxProvider();
const models = createModels();
models.setProvider(faux.provider);
const respond = (ctx: { messages: Array<{ role: string; content: Array<{ type: string; text?: string }>; isError?: boolean }> }) => {
  const result = ctx.messages.find((m) => m.role === "toolResult");
  if (result) return fauxAssistantMessage([fauxText(`done; isError=${result.isError}`)]);
  return fauxAssistantMessage([fauxToolCall("codemode", { code }, { id: "cm-1" })], { stopReason: "toolUse" });
};
faux.setResponses(Array.from({ length: 16 }, () => respond));

const reports: unknown[] = [];
const storagePath = join(dir, "session.sqlite");
const storage = await openNodeSqliteStorage(storagePath);
const harness = await Harness.open(
  storage,
  {
    models,
    registry,
    settings: {},
    onReport: (report: { kind: string; reason?: string; restarts?: number; error?: unknown }) => {
      const r = { kind: report.kind, reason: report.reason, restarts: report.restarts, error: String((report.error as Error)?.message ?? report.error ?? "") };
      reports.push(r);
      say("REPORT", r);
    },
  },
  context,
);
const conversation = await harness.root(context, { agent: { model: { provider: "faux", modelId: "faux-1" } } });
const submission = await conversation.submit({ type: "input", content: "Run the triage workflow.", requestId: "workflow-1" }, context);
const settled = await submission.wait(context);
say("SETTLED", { status: settled.status, type: settled.type, reason: settled.reason });

let text = "";
let isError = false;
let calls: unknown;
const page = await conversation.entries({}, 50, undefined, context);
for (const entry of [...page.items].reverse()) {
  if (!ToolResultEntry.is(entry)) continue;
  const message = entry.model[0];
  isError = message.isError;
  calls = message.details?.calls ?? entry.details?.calls;
  text = message.content.map((c: { type: string; text?: string }) => (c.type === "text" ? c.text : "")).join("");
}
say("TOOL_RESULT", { isError, text: text.slice(0, 2000) });
await harness.close(context);
await sandbox.close?.();

/** Rows Pi's storage holds after the run: its durable records. */
function storageRows(): number | null {
  try {
    const db = new DatabaseSync(storagePath, { readOnly: true });
    try {
      const tables = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all() as Array<{ name: string }>;
      return tables.reduce((total, t) => total + (db.prepare(`SELECT count(*) AS n FROM "${t.name}"`).get() as { n: number }).n, 0);
    } finally {
      db.close();
    }
  } catch {
    return null;
  }
}

const outcomes = parseOutcomes(text);
const status = `${settled.status}${settled.reason ? `/${settled.reason}` : ""}${isError ? " codemode-error" : ""}: ${text.replace(/@@OUTCOMES.*/s, "").replace(/\s+/g, " ").slice(0, 300)}`;
const claims = outcomes ? outcomeClaims(outcomes) : { ops: {}, review: [], raw: {} as Record<string, string> };
// When the program died before printing its outcomes (a deadline), Pi still lists the nested calls
// it made with a status; keep each unfinished write's status verbatim, without mapping it to a claim.
if (Array.isArray(calls)) {
  for (const c of calls as Array<{ name: string; args?: string; status?: string }>) {
    if (c.status === "ok" || !WRITE_SLUGS.includes(c.name) || !c.args) continue;
    const id = operationId(c.name, JSON.parse(c.args));
    if (!(id in claims.ops)) claims.raw[id] = `nested call ${c.status}`;
  }
}
const success = !isError && outcomes !== undefined && Object.values(outcomes).every((v) => v === "ok");
writeClaims(options, {
  engine: "pi-durable",
  run_success: success,
  run_status: status,
  ...claims,
  durable_records: storageRows(),
  detail: { reports, calls: JSON.stringify(calls ?? null).slice(0, 4000) },
});
process.exit(success ? EXIT.success : EXIT.failed);
