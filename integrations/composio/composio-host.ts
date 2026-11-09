/**
 * The commands of the Composio GitHub host (`host.ts`): investigate a
 * repository with a program, declare the provider, preflight a plan, and run
 * or resume an approved plan. The entry supplies the `composio run` helper;
 * this module has no top-level effects.
 */
import { execFile } from "node:child_process";
import { randomBytes } from "node:crypto";
import { closeSync, mkdtempSync, openSync, readFileSync, readSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve as resolvePath } from "node:path";
import { promisify } from "node:util";
import { findExecutable, type PlanCall, positive, type RunReport, runPlanBridge, summarize } from "../shared/bridge-client.ts";
import { investigationExecutor, READ_TOOLS, runComposio, runHelperExecutor } from "./composio-provider.ts";
import {
  type AccountExecute,
  check,
  CREATE_COMMENT,
  declaration,
  destination,
  type GithubPlanHost,
  githubPlanHost,
  type Identity,
  identityFromDeclaration,
  type ListConnections,
  type Connection,
  marker,
  observe,
  GET_LABEL,
  PROVIDER,
  parseTarget,
  refusalCode,
  resolve,
  scopeAccount,
  type Target,
} from "./github-plan.ts";

export const USAGE = `Usage: codemode-composio <command> --account <ca_...> --repo <owner/repo> [options]

  investigate --program <file> --out <plan.json> [--trace <file>]
              [--timeout-ms 60000] [--concurrency 4] [--max-calls 32]
  declare
  preflight   --plan <plan.json>
  run         --declaration <file> --journal <file> [--deadline-ms <n>]
  resume      --declaration <file> --journal <file> [--deadline-ms <n>]

Every command also takes --bin <codemode> (codemode on PATH by default).`;

const OPTIONS: Record<string, string[]> = {
  investigate: ["program", "out", "trace", "timeout-ms", "concurrency", "max-calls"],
  declare: [],
  preflight: ["plan"],
  run: ["declaration", "journal", "deadline-ms"],
  resume: ["declaration", "journal", "deadline-ms"],
};

/** Strict parsing: an unknown command or option, or a missing value, never selects an action. */
function options(argv: string[]): { command: string; values: Map<string, string> } {
  const [command = "", ...rest] = argv;
  const allowed = OPTIONS[command];
  if (!allowed) throw new Error(`unknown command '${command}'\n${USAGE}`);
  const values = new Map<string, string>();
  for (let index = 0; index < rest.length; index += 2) {
    const name = rest[index].startsWith("--") ? rest[index].slice(2) : "";
    const value = rest[index + 1];
    if (![...allowed, "account", "repo", "bin"].includes(name)) throw new Error(`unexpected argument ${rest[index]} for ${command}`);
    if (value === undefined || value.startsWith("--")) throw new Error(`--${name} needs a value`);
    if (values.has(name)) throw new Error(`--${name} can only be supplied once`);
    values.set(name, value);
  }
  return { command, values };
}

function required(values: Map<string, string>, name: string): string {
  const value = values.get(name);
  if (value === undefined) throw new Error(`--${name} is required`);
  return value;
}

/** Write a line, then exit: an unabortable helper call must not keep the host alive. */
export function finish(stream: NodeJS.WriteStream, line: string, code: number): void {
  process.exitCode = code;
  stream.write(line + "\n", () => process.exit(code));
}

/** The helper that executes Composio tools, and the connection lister. */
export interface HostProviders {
  execute: AccountExecute;
  connections: ListConnections;
}

/** Wraps the plan host's handlers, for example to observe them; the identity by default. */
export type DecorateHost = (host: GithubPlanHost) => GithubPlanHost;

/** `composio connections list`, GitHub entries only. */
export async function listConnections(): Promise<Connection[]> {
  // `composio run` sets BUN_BE_BUN, which makes a nested `composio` act as its embedded Bun.
  const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => name !== "BUN_BE_BUN"));
  const { stdout } = await promisify(execFile)("composio", ["connections", "list"], { maxBuffer: 1024 * 1024, env });
  const github = (JSON.parse(stdout) as { github?: unknown }).github;
  if (!Array.isArray(github)) throw new Error("unexpected connections list");
  return github as Connection[];
}

/** Resolve the login and repository id `target` acts on, or exit refused. */
async function identify(execute: AccountExecute, target: Target): Promise<Identity & { repoId: number }> {
  const found = await resolve(execute, target);
  if ("error" in found) {
    finish(process.stdout, JSON.stringify({ status: "refused", detail: found.error }), 4);
    return new Promise<never>(() => {});
  }
  return { ...target, login: found.login, repoId: found.repoId };
}

/** Write `text` to `path` atomically, readable by the owner only. */
function writePrivate(path: string, text: string): void {
  writeFileSync(`${path}.tmp`, text, { mode: 0o600 });
  renameSync(`${path}.tmp`, path);
}

/** Run `codemode plan check <path> --json` and return its digest. */
async function planCheck(bin: string, path: string): Promise<string> {
  try {
    const { stdout } = await promisify(execFile)(bin, ["plan", "check", path, "--json"], { env: {}, maxBuffer: 4 * 1024 * 1024, timeout: 10_000 });
    // SAFETY: --json from the configured authoritative binary returns the checked digest.
    return (JSON.parse(stdout) as { digest: string }).digest;
  } catch (error) {
    const stderr = (error as { stderr?: string }).stderr?.trim();
    throw new Error(`codemode plan check refused the plan: ${stderr || (error instanceof Error ? error.message : String(error))}`);
  }
}

/** The trace with every call value and the output replaced by their sizes, which Rust already records. */
function redact(report: RunReport): RunReport {
  const calls = report.calls.map((record) => {
    const call = record as { outcome: { status: string }; result_bytes?: number };
    return call.outcome.status === "succeeded" ? { ...call, outcome: { status: "succeeded", value: { redacted_bytes: call.result_bytes ?? null } } } : call;
  });
  const redacted: RunReport = { ...report, calls };
  if ("output" in report) redacted.output = { redacted_bytes: Buffer.byteLength(JSON.stringify(report.output) ?? "") };
  return redacted;
}

interface Proposed {
  id: string;
  tool: string;
  arguments: Record<string, unknown>;
  preconditions?: unknown[];
}

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);

/** The program's proposal: `{operations: [{id, tool, arguments, preconditions?}]}` and nothing else. */
function proposal(output: unknown): Proposed[] {
  const shape = "the program must return {operations: [{id, tool, arguments, preconditions?}]}";
  if (!isRecord(output) || Object.keys(output).join() !== "operations" || !Array.isArray(output.operations)) throw new Error(shape);
  return output.operations.map((item, index) => {
    const keys = isRecord(item) ? Object.keys(item) : [];
    const valid = isRecord(item) && typeof item.id === "string" && typeof item.tool === "string" && isRecord(item.arguments)
      && (item.preconditions === undefined || Array.isArray(item.preconditions))
      && keys.every((key) => ["id", "tool", "arguments", "preconditions"].includes(key));
    if (!valid) throw new Error(`operation ${index}: ${shape}`);
    return item as unknown as Proposed;
  });
}

/**
 * Turn a proposal into plan text: each operation gets the host's scope, and each
 * comment body gets the marker line for a fresh random plan id. Every operation
 * is checked against the provider contract first; the program is not part of it.
 */
export function planFromProposal(operations: Proposed[], identity: Identity & { repoId: number }, planId: string): string {
  const scope = { provider: PROVIDER, account: scopeAccount(identity), destination: destination(identity.repo, identity.repoId) };
  const planned = operations.map((operation) => {
    const args = { ...operation.arguments };
    if (operation.tool === CREATE_COMMENT && typeof args.body === "string") {
      const body = args.body.trimEnd();
      args.body = `${body}${body ? "\n\n" : ""}${marker(planId, operation.id)}`;
    }
    try {
      check(operation.id, operation.tool, args, scope, operation.preconditions ?? [], identity);
    } catch (error) {
      throw new Error(`operation ${operation.id} is outside the provider contract (${refusalCode(error)}): ${(error as Error).message}`);
    }
    return { id: operation.id, tool: operation.tool, scope, arguments: args, ...(operation.preconditions?.length ? { preconditions: operation.preconditions } : {}) };
  });
  return JSON.stringify({ version: 1, operations: planned }, null, 2) + "\n";
}

async function investigate(providers: HostProviders, target: Target, args: Map<string, string>): Promise<void> {
  const cwd = process.cwd();
  const bin = findExecutable(args.get("bin") ?? "codemode", cwd);
  // The program learns its repository from this prefix, on its first line so line numbers hold.
  const [owner, repo] = target.repo.split("/");
  const code = `const repository = Object.freeze(${JSON.stringify({ owner, repo })}); ` + readFileSync(resolvePath(cwd, required(args, "program")), "utf8");
  const out = resolvePath(cwd, required(args, "out"));
  const limits = {
    timeoutMs: positive("--timeout-ms", args.get("timeout-ms") ?? 60_000),
    concurrency: positive("--concurrency", args.get("concurrency") ?? 4),
    maxCalls: positive("--max-calls", args.get("max-calls") ?? 32),
  };
  const identity = await identify(providers.execute, target);

  const controller = new AbortController();
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => controller.abort());
  const { report, hostDiagnostic } = await runComposio({
    bin,
    cwd,
    code,
    tools: READ_TOOLS,
    ...limits,
    signal: controller.signal,
    executor: investigationExecutor(runHelperExecutor(providers.execute, target.account), target.repo),
  });
  const summary = summarize(report);
  delete summary.output;
  if (hostDiagnostic) summary.host_diagnostic = hostDiagnostic;
  const trace = args.get("trace");
  if (trace) {
    const path = resolvePath(cwd, trace);
    writePrivate(path, JSON.stringify(redact(report), null, 2) + "\n");
    summary.trace = path;
  }
  if (report.status !== "success") {
    finish(process.stdout, JSON.stringify(summary), 1);
    return;
  }
  const operations = proposal(report.output);
  summary.operations = operations.length;
  if (operations.length === 0) {
    // Nothing to change: no plan is written.
    finish(process.stdout, JSON.stringify(summary), 0);
    return;
  }
  writePrivate(out, planFromProposal(operations, identity, randomBytes(12).toString("hex")));
  try {
    summary.digest = await planCheck(bin, out);
  } catch (error) {
    rmSync(out, { force: true });
    throw error;
  }
  summary.plan = out;
  finish(process.stdout, JSON.stringify(summary), 0);
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
async function checkedPlan(bin: string, path: string): Promise<{ digest: string; operations: CheckedOperation[] }> {
  const bytes = planBytes(resolvePath(process.cwd(), path));
  const directory = mkdtempSync(join(tmpdir(), "codemode-preflight-"));
  try {
    const snapshot = join(directory, "plan.json");
    writeFileSync(snapshot, bytes, { mode: 0o600 });
    const digest = await planCheck(bin, snapshot);
    // SAFETY: the configured Rust binary validated these exact snapshot bytes, including every operation.
    const plan = JSON.parse(bytes.toString("utf8")) as { operations: CheckedOperation[] };
    return { digest, operations: plan.operations };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

/** Read-only checks of every operation in a plan; prints projected facts only. */
async function preflight(providers: HostProviders, target: Target, args: Map<string, string>): Promise<void> {
  const bin = findExecutable(args.get("bin") ?? "codemode", process.cwd());
  const plan = await checkedPlan(bin, required(args, "plan"));
  const identity = await identify(providers.execute, target);
  const host = githubPlanHost(providers.execute, identity, providers.connections, () => {});
  const [owner, repo] = target.repo.split("/");
  const authorized = new Map<string, true | string>();
  const report = [];
  let ready = true;
  for (const operation of plan.operations) {
    const row: Record<string, unknown> = { operation: operation.id };
    try {
      const { write, issueState } = check(operation.id, operation.tool, operation.arguments, operation.scope, operation.preconditions ?? [], identity);
      const key = JSON.stringify(operation.scope);
      if (!authorized.has(key)) {
        const verdict = await host.authorize(operation.scope);
        authorized.set(key, verdict.ok ? true : verdict.error.code);
      }
      row.authorized = authorized.get(key);
      ready &&= row.authorized === true;
      if (write.kind === "add" || write.kind === "remove") {
        const label = await observe(providers.execute, GET_LABEL, { owner, repo, name: write.label }, target.account);
        row.label_exists = "result" in label && label.result.successful && label.result.dryRun !== true;
        ready &&= row.label_exists === true;
      }
      if (issueState) {
        const issue = await observe(providers.execute, "GITHUB_GET_AN_ISSUE", { owner, repo, issue_number: write.issue }, target.account);
        const state = "result" in issue && issue.result.successful && isRecord(issue.result.data) ? issue.result.data.state : undefined;
        row.precondition_holds = state === issueState;
        ready &&= row.precondition_holds === true;
      }
      const now = await host.reconcile(
        { id: 0, operation: operation.id, tool: operation.tool, arguments: operation.arguments, keys: [], scope: operation.scope },
        new AbortController().signal,
      );
      if ("error" in now) {
        row.already_done = now.error.code === "marker_absent" ? false : now.error.code;
        ready &&= now.error.code === "marker_absent";
      } else {
        row.already_done = now.verdict === "satisfied" || now.verdict === "applied";
      }
    } catch (error) {
      row.refused = refusalCode(error);
      ready = false;
    }
    report.push(row);
  }
  const line = { preflight: ready ? "ready" : "not_ready", digest: plan.digest, checked_operations: plan.operations.length, operations: report };
  finish(process.stdout, JSON.stringify(line), ready ? 0 : 3);
}

async function session(kind: "run" | "resume", providers: HostProviders, target: Target, args: Map<string, string>, decorate: DecorateHost): Promise<void> {
  const cwd = process.cwd();
  const identity = identityFromDeclaration(JSON.parse(readFileSync(resolvePath(cwd, required(args, "declaration")), "utf8")), target);
  const journal = resolvePath(cwd, required(args, "journal"));
  const deadline = args.get("deadline-ms");
  const controller = new AbortController();
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => controller.abort());

  const host = decorate(githubPlanHost(providers.execute, identity, providers.connections, () => controller.abort()));
  const result = await runPlanBridge({
    bin: findExecutable(args.get("bin") ?? "codemode", cwd),
    cwd,
    journal,
    kind,
    ...(deadline === undefined ? {} : { deadlineMs: positive("--deadline-ms", deadline) }),
    signal: controller.signal,
    declaration: declaration(identity),
    authorize: (scope) => host.authorize(scope),
    reconcile: (request, signal) => host.reconcile(request, signal),
    execute: (call, signal) => host.execute(call, signal),
  }).catch((error: unknown) => {
    finish(process.stderr, `composio host: ${error instanceof Error ? error.message : String(error)}`, 1);
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

/**
 * Run one host command for `argv`, then exit. Every command needs `providers`
 * (under `composio run`, its helper and `composio connections list`).
 */
export async function runHost(argv: string[], providers: HostProviders | undefined, decorate: DecorateHost = (host) => host): Promise<void> {
  const { command, values } = options(argv);
  const target = parseTarget(values.get("account"), values.get("repo"));
  if (!providers) throw new Error("run under `composio run`, which supplies the execute helper");
  switch (command) {
    case "investigate":
      return investigate(providers, target, values);
    case "declare": {
      const identity = await identify(providers.execute, target);
      finish(process.stdout, JSON.stringify(declaration(identity)), 0);
      return;
    }
    case "preflight":
      return preflight(providers, target, values);
    default:
      return session(command as "run" | "resume", providers, target, values, decorate);
  }
}
