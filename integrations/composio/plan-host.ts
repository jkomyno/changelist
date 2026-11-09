/**
 * Composio host for recoverable plan sessions: add one existing label to one
 * GitHub issue per operation, with Rust keeping the journal.
 *
 *     composio run --file integrations/composio/plan-host.ts -- <command> --config <file> ...
 *
 * Commands:
 *   declare   --config <file>                   Print the provider declaration to approve plans with.
 *   preflight --config <file> --plan <file>     Read-only: authorize each scope, check each operation,
 *                                               confirm each label exists, and read each issue's labels.
 *   run|resume --config <file> --journal <file> Serve one session of an approved plan.
 *
 * Options: --bin <codemode> (target/debug/codemode), --deno <deno> (deno),
 * --offline <state.json> (use the offline stand-in instead of the CLI helper),
 * --drop-reply <operation> (fault injection for recovery demonstrations: after
 * the helper reports a successful mutation for that operation, never reply, so
 * the session cannot record it; off unless given).
 *
 * The config is host-owned: `{"account": "<connected account id>", "destinations": ["owner/repo"]}`.
 * stdout receives one compact JSON line. Exit status: 0 completed, 3 work remains,
 * 4 refused, 2 usage or startup error, 1 the session ended without an outcome.
 *
 * `composio run` wraps everything after the imports in a function, so this file
 * has no exports and ends with a statement.
 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { findExecutable, type PlanReply, runPlanBridge } from "../shared/bridge-client.ts";
import { type AccountExecute, composioPlanHost, declaration, GET_LABEL, parseConfig, target } from "./composio-plan.ts";

const VALUE_OPTIONS = new Set(["config", "journal", "plan", "bin", "deno", "offline", "drop-reply"]);

function options(argv: string[]): { command: string; values: Map<string, string> } {
  const [command, ...rest] = argv;
  const values = new Map<string, string>();
  for (let index = 0; index < rest.length; index += 2) {
    const name = rest[index].startsWith("--") ? rest[index].slice(2) : "";
    const value = rest[index + 1];
    if (!VALUE_OPTIONS.has(name)) throw new Error(`unexpected argument ${rest[index]}`);
    if (value === undefined || value.startsWith("--")) throw new Error(`--${name} needs a value`);
    if (values.has(name)) throw new Error(`--${name} can only be supplied once`);
    values.set(name, value);
  }
  return { command: command ?? "", values };
}

/** Write a line, then exit: an unabortable helper call must not keep the host alive. */
function finish(stream: NodeJS.WriteStream, line: string, code: number): void {
  process.exitCode = code;
  stream.write(line + "\n", () => process.exit(code));
}

const json = (path: string) => JSON.parse(readFileSync(resolve(process.cwd(), path), "utf8"));

async function helper(args: Map<string, string>): Promise<AccountExecute> {
  const offline = args.get("offline");
  if (offline) return (await import("./demo/fake-labels.ts")).fakeLabels(resolve(process.cwd(), offline));
  const execute = (globalThis as { execute?: AccountExecute }).execute;
  if (!execute) throw new Error("run under `composio run`, or pass --offline <state.json>");
  return execute;
}

/** Read-only checks of every operation in a plan; prints projected facts only. */
async function preflight(execute: AccountExecute, args: Map<string, string>): Promise<number> {
  const config = parseConfig(json(args.get("config") ?? ""));
  const plan = json(args.get("plan") ?? "") as { operations?: Array<Record<string, any>> };
  const host = composioPlanHost(execute, config);
  const report = [];
  let ready = true;
  for (const operation of plan.operations ?? []) {
    const row: Record<string, unknown> = { operation: operation.id };
    try {
      const op = target(operation.tool, operation.arguments, operation.scope, operation.preconditions ?? [], config);
      const authorized = await host.authorize(operation.scope);
      row.authorized = authorized.ok ? true : authorized.error.code;
      const label = await execute(GET_LABEL, { owner: op.owner, repo: op.repo, name: op.label }, { account: config.account }).then(
        (result) => result.successful === true,
        () => false,
      );
      row.label_exists = label;
      const state = await host.reconcile(
        { id: 0, operation: operation.id, tool: operation.tool, arguments: operation.arguments, keys: [], scope: operation.scope },
        new AbortController().signal,
      );
      row.label_on_issue = "error" in state ? state.error.code : state.verdict === "satisfied";
      ready &&= authorized.ok && label && !("error" in state);
    } catch (error) {
      row.refused = (error as { code?: string }).code ?? "invalid_operation";
      ready = false;
    }
    report.push(row);
  }
  finish(process.stdout, JSON.stringify({ preflight: ready ? "ready" : "not_ready", operations: report }), ready ? 0 : 3);
  return ready ? 0 : 3;
}

async function main(): Promise<void> {
  const { command, values: args } = options(process.argv.slice(2));
  if (command === "declare") {
    finish(process.stdout, JSON.stringify(declaration(parseConfig(json(args.get("config") ?? "")))), 0);
    return;
  }
  const execute = await helper(args);
  if (command === "preflight") {
    await preflight(execute, args);
    return;
  }
  if (command !== "run" && command !== "resume") throw new Error("command must be declare, preflight, run, or resume");
  const config = parseConfig(json(args.get("config") ?? ""));
  const journal = args.get("journal");
  if (!journal) throw new Error("--journal is required");

  const drop = args.get("drop-reply");
  const cwd = process.cwd();
  const controller = new AbortController();
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => controller.abort());

  const host = composioPlanHost(execute, config);
  const result = await runPlanBridge({
    bin: findExecutable(args.get("bin") ?? "target/debug/codemode", cwd),
    deno: findExecutable(args.get("deno") ?? "deno", cwd),
    cwd,
    journal: resolve(cwd, journal),
    kind: command,
    signal: controller.signal,
    declaration: declaration(config),
    authorize: (scope) => host.authorize(scope),
    reconcile: (request, signal) => host.reconcile(request, signal),
    execute: async (call, signal) => {
      const reply = await host.execute(call, signal);
      if (drop === call.operation && reply.ok) {
        // Fault injection, only with --drop-reply: the success was observed here and is lost on purpose.
        process.stderr.write(`composio plan host: dropped the reply for ${call.operation} after an observed success\n`);
        return new Promise<PlanReply>(() => {});
      }
      return reply;
    },
  }).catch((error: unknown) => {
    finish(process.stderr, `composio plan host: ${error instanceof Error ? error.message : String(error)}`, 1);
    return new Promise<never>(() => {});
  });
  const { outcome, diagnostic } = result;
  const summary: Record<string, unknown> = { session: outcome.session, status: outcome.status };
  if (outcome.detail) summary.detail = outcome.detail;
  if (diagnostic) summary.diagnostic = diagnostic;
  summary.operations = (outcome.operations ?? []).map((status) => [status.operation, status.conclusion, status.next]);
  const code = outcome.status === "completed" ? 0 : outcome.status === "refused" ? 4 : 3;
  finish(process.stdout, JSON.stringify(summary), code);
}

try {
  await main();
} catch (error) {
  finish(process.stderr, `composio plan host: ${error instanceof Error ? error.message : String(error)}`, 2);
}
