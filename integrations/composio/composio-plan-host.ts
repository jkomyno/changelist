/**
 * The Composio plan host's commands, for `plan-host.ts`: add one existing label
 * to one GitHub issue per operation, with Rust keeping the journal. The entry
 * supplies the `composio run` helper; this module has no top-level effects.
 */
import { execFile } from "node:child_process";
import { closeSync, mkdtempSync, openSync, readFileSync, readSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { promisify } from "node:util";
import { join, resolve } from "node:path";
import { findExecutable, type PlanCall, runPlanBridge } from "../shared/bridge-client.ts";
import {
  type AccountExecute,
  type ComposioPlanHost,
  type Connection,
  composioPlanHost,
  declaration,
  GET_LABEL,
  type ListConnections,
  observe,
  parseConfig,
  target,
} from "./composio-plan.ts";

const VALUE_OPTIONS = new Set(["config", "journal", "plan", "bin"]);

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
export function finish(stream: NodeJS.WriteStream, line: string, code: number): void {
  process.exitCode = code;
  stream.write(line + "\n", () => process.exit(code));
}

const json = (path: string) => JSON.parse(readFileSync(resolve(process.cwd(), path), "utf8"));

/** The helper that executes Composio tools, and the connection lister. */
export interface PlanHostProviders {
  execute: AccountExecute;
  connections: ListConnections;
}

/** Wraps the host's handlers, for example to observe them; the identity by default. */
export type DecorateHost = (host: ComposioPlanHost) => ComposioPlanHost;

/** `composio connections list`, GitHub entries only. */
export async function listConnections(): Promise<Connection[]> {
  // `composio run` sets BUN_BE_BUN, which makes a nested `composio` act as its embedded Bun.
  const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => name !== "BUN_BE_BUN"));
  const { stdout } = await promisify(execFile)("composio", ["connections", "list"], { maxBuffer: 1024 * 1024, env });
  const github = (JSON.parse(stdout) as { github?: unknown }).github;
  if (!Array.isArray(github)) throw new Error("unexpected connections list");
  return github as Connection[];
}

/** Read at most Rust's MAX_PLAN_BYTES plus one byte, including for growing files. */
function planBytes(path: string): Buffer {
  const bytes = Buffer.alloc(1024 * 1024 + 1);
  const file = openSync(path, "r");
  let length = 0;
  try {
    while (length < bytes.length) {
      const count = readSync(file, bytes, length, bytes.length - length, null);
      if (count === 0) break;
      length += count;
    }
  } finally {
    closeSync(file);
  }
  if (length === bytes.length) throw new Error("plan exceeds 1048576 bytes");
  return bytes.subarray(0, length);
}

type CheckedOperation = Pick<PlanCall, "tool" | "arguments" | "scope"> & { id: string; preconditions?: unknown[] };

/** Validate the exact bytes subsequently inspected, with Rust's complete plan decoder. */
async function checkedPlan(args: Map<string, string>): Promise<{ digest: string; operations: CheckedOperation[] }> {
  const path = args.get("plan");
  if (!path) throw new Error("--plan is required");
  const cwd = process.cwd();
  const bytes = planBytes(resolve(cwd, path));
  const directory = mkdtempSync(join(tmpdir(), "codemode-preflight-"));
  try {
    const snapshot = join(directory, "plan.json");
    writeFileSync(snapshot, bytes, { mode: 0o600 });
    const { stdout } = await promisify(execFile)(
      findExecutable(args.get("bin") ?? "codemode", cwd),
      ["plan", "check", snapshot, "--json"],
      { cwd, env: {}, maxBuffer: 2 * 1024 * 1024, timeout: 10_000 },
    );
    // SAFETY: the configured Rust binary validated these exact snapshot bytes, including every operation.
    const plan = JSON.parse(bytes.toString("utf8")) as { operations: CheckedOperation[] };
    // SAFETY: --json from the configured authoritative binary returns the checked digest.
    const checked = JSON.parse(stdout) as { digest: string };
    return { digest: checked.digest, operations: plan.operations };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

/** Read-only checks of every operation in a plan; prints projected facts only. */
async function preflight({ execute, connections }: { execute: AccountExecute; connections: ListConnections }, args: Map<string, string>): Promise<void> {
  const config = parseConfig(json(args.get("config") ?? ""));
  const plan = await checkedPlan(args);
  const host = composioPlanHost(execute, config, connections);
  const report = [];
  let ready = true;
  for (const operation of plan.operations) {
    const row: Record<string, unknown> = { operation: operation.id };
    try {
      const op = target(operation.tool, operation.arguments, operation.scope, operation.preconditions ?? [], config);
      const authorized = await host.authorize(operation.scope);
      row.authorized = authorized.ok ? true : authorized.error.code;
      const seen = await observe(execute, GET_LABEL, { owner: op.owner, repo: op.repo, name: op.label }, config.account);
      const label = "result" in seen && seen.result.successful;
      row.label_exists = label;
      const state = await host.reconcile(
        { id: 0, operation: operation.id, tool: operation.tool, arguments: operation.arguments, keys: [], scope: operation.scope },
        new AbortController().signal,
      );
      row.label_on_issue = "error" in state ? state.error.code : state.verdict === "satisfied";
      ready &&= authorized.ok && label && !("error" in state);
    } catch (error) {
      row.refused = error instanceof Error && "code" in error ? error.code : "invalid_operation";
      ready = false;
    }
    report.push(row);
  }
  finish(process.stdout, JSON.stringify({ preflight: ready ? "ready" : "not_ready", digest: plan.digest, checked_operations: plan.operations.length, operations: report }), ready ? 0 : 3);
}

/**
 * Run one plan host command (`declare`, `preflight`, `run`, or `resume`) for
 * `argv`, then exit. `providers` is required by every command except `declare`.
 */
export async function runPlanHost(argv: string[], providers: PlanHostProviders | undefined, decorate: DecorateHost = (host) => host): Promise<void> {
  const { command, values: args } = options(argv);
  if (command === "declare") {
    finish(process.stdout, JSON.stringify(declaration(parseConfig(json(args.get("config") ?? "")))), 0);
    return;
  }
  if (!providers) throw new Error("run under `composio run`, which supplies the execute helper");
  const { execute } = providers;
  if (command === "preflight") {
    await preflight(providers, args);
    return;
  }
  if (command !== "run" && command !== "resume") throw new Error("command must be declare, preflight, run, or resume");
  const config = parseConfig(json(args.get("config") ?? ""));
  const journal = args.get("journal");
  if (!journal) throw new Error("--journal is required");

  const cwd = process.cwd();
  const controller = new AbortController();
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => controller.abort());

  const host = decorate(composioPlanHost(execute, config, providers.connections));
  const result = await runPlanBridge({
    bin: findExecutable(args.get("bin") ?? "codemode", cwd),
    cwd,
    journal: resolve(cwd, journal),
    kind: command,
    signal: controller.signal,
    declaration: declaration(config),
    authorize: (scope) => host.authorize(scope),
    reconcile: (request, signal) => host.reconcile(request, signal),
    execute: (call, signal) => host.execute(call, signal),
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
