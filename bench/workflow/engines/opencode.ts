// OpenCode Code Mode 2.2.6 adapter: one plain-JavaScript program through the library API
// (`CodeMode.make({ tools, limits }).execute(code)`), in this process and heap, released engine
// ("legacy" interpreter; the VM is flag-gated off in releases).
//
// OpenCode keeps nothing durable: a restart reruns the program from scratch. The triage program
// is convergent (it derives every write from the state it reads), which is what makes a rerun
// reasonable at all; that is the program's property, not the engine's.
//
// Limits: host default passes none (OpenCode sets no deadline, call, or output limit by default
// and has no memory limit). The equal-limits row sets `timeoutMs` and `maxToolCalls`; the memory
// bound is the driver's `node --max-old-space-size` on this process, because the interpreter
// shares the host's heap.
import { adapterOptions, EXIT, outcomeClaims, parseOutcomes, PKGS, program, say, WIDEN_OP, writeClaims } from "../lib/adapter.ts";
import { call, providerClient } from "../lib/client.ts";
import { ACCOUNT, READ_SLUGS, WRITE_SLUGS } from "../lib/github-state.ts";
import { pkgImport } from "../lib/pkg-import.ts";
import { join } from "node:path";

const options = adapterOptions();
const root = join(PKGS, "opencode");
const execute = providerClient(options.provider);

const { CodeMode, Tool } = await pkgImport(root, "@opencode/codemode");
const { Effect } = await pkgImport(root, "effect");

const tools = Object.fromEntries(
  [...READ_SLUGS, ...WRITE_SLUGS].map((slug) => [
    slug,
    Tool.make({
      description: `GitHub ${slug}`,
      input: { type: "object", additionalProperties: true },
      output: {},
      // Effect.tryPromise passes an AbortSignal that fires when the fiber is interrupted (timeout, end of program).
      execute: (input: Record<string, unknown>) =>
        Effect.tryPromise({
          try: (signal: AbortSignal) => call(execute, slug, input, ACCOUNT, signal),
          catch: (error: unknown) => (error instanceof Error ? error : new Error(String(error))),
        }),
    }),
  ]),
);

const limits: Record<string, number> = {};
if (options.deadlineMs !== undefined) limits.timeoutMs = options.deadlineMs;
if (options.maxCalls !== undefined) limits.maxToolCalls = options.maxCalls;
const extra = options.widen ? JSON.stringify([WIDEN_OP]) : "[]";
const code = program(
  options.workload,
  "async function gh(slug, args) { return await tools[slug](args); }",
  `await decideAndApply((id) => "<!-- opencode:" + id + " -->", ${extra});`,
);

const runtime = CodeMode.make({ tools, limits });
const result = await Effect.runPromise(runtime.execute(code));
const logs = (result.logs ?? []).map((entry: unknown) => (typeof entry === "string" ? entry : JSON.stringify(entry))).join("\n");
say("RESULT", { ok: result.ok, error: result.error, logs: logs.slice(0, 2000) });

const outcomes = parseOutcomes(logs);
const claims = outcomes ? outcomeClaims(outcomes) : { ops: {}, review: [], raw: {} };
const success = result.ok === true && outcomes !== undefined && Object.values(outcomes).every((v) => v === "ok");
writeClaims(options, {
  engine: "opencode",
  run_success: success,
  run_status: result.ok ? "ok" : `error ${result.error?.kind ?? ""}: ${String(result.error?.message ?? "").slice(0, 200)}`,
  ...claims,
  durable_records: 0,
  detail: { toolCalls: JSON.stringify(result.toolCalls ?? null).slice(0, 4000) },
});
process.exit(success ? EXIT.success : EXIT.failed);
