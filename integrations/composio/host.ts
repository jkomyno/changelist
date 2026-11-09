/**
 * Composio host for `codemode bridge`: run one guest program whose tool calls
 * execute as Composio tools in this trusted process.
 *
 * Live, under the Composio CLI (verified with 0.6.0), which injects an
 * authenticated `execute` helper into this script and keeps its credential here:
 *
 *     composio run --file integrations/composio/host.ts -- \
 *       --program integrations/composio/demo/workflow.ts \
 *       --tools GITHUB_GET_THE_AUTHENTICATED_USER,SLACK_TEST_AUTH,GITHUB_GET_A_REPOSITORY
 *
 * Offline, with any Node-compatible runtime and recorded fake results:
 *
 *     deno run --allow-run --allow-read --allow-write --allow-env \
 *       integrations/composio/host.ts --offline --program ... --tools ...
 *
 * Options: --program <file> (required), --tools <slug,...> (required),
 * --trace <file>, --bin <codemode> (codemode on PATH), --deno <deno>, --backend deno|embedded-v8, --timeout-ms <n> (30000),
 * --concurrency <n> (4), --max-calls <n> (16), --offline.
 * stdout receives one compact JSON summary; the exit status is 0 only on success.
 * The host exits after the report, abandoning any call Rust recorded as unknown.
 * If an execution ends without an observed result, the host cancels the run so
 * Rust records that call as unknown, and the summary adds `host_diagnostic`.
 *
 * `composio run` wraps everything after the imports in a function, so this file
 * has no exports and ends with a statement.
 */
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { findExecutable, positive, parseBackend, summarize } from "../shared/bridge-client.ts";
import { type RunHelperExecute, runComposio, runHelperExecutor } from "./composio-provider.ts";

const VALUE_OPTIONS = new Set(["program", "tools", "trace", "bin", "deno", "backend", "timeout-ms", "concurrency", "max-calls"]);

/** Strict parsing: an unknown option or a missing value never selects an executor. */
function options(argv: string[]): Map<string, string> {
  const parsed = new Map<string, string>();
  for (let index = 0; index < argv.length; index++) {
    const name = argv[index].startsWith("--") ? argv[index].slice(2) : "";
    if (name === "offline") {
      parsed.set(name, "true");
      continue;
    }
    const value = argv[index + 1];
    if (!VALUE_OPTIONS.has(name)) throw new Error(`unexpected argument ${argv[index]}`);
    if (value === undefined || value.startsWith("--")) throw new Error(`--${name} needs a value`);
    if (parsed.has(name)) throw new Error(`--${name} can only be supplied once`);
    parsed.set(name, value);
    index++;
  }
  return parsed;
}

/** Write a line, then exit: an unabortable call in flight must not keep the host alive. */
function finish(stream: NodeJS.WriteStream, line: string, code: number): void {
  process.exitCode = code;
  stream.write(line + "\n", () => process.exit(code));
}

async function main(): Promise<void> {
  const args = options(process.argv.slice(2));
  const cwd = process.cwd();
  const program = args.get("program");
  const tools = (args.get("tools") ?? "").split(",").filter(Boolean);
  if (!program || tools.length === 0) throw new Error("--program and --tools are required");

  const execute = args.has("offline")
    ? (await import("./demo/fake-composio.ts")).fakeExecute
    : (globalThis as { execute?: RunHelperExecute }).execute;
  if (!execute) throw new Error("run under `composio run`, or pass --offline");

  const backend = parseBackend(args.get("backend") ?? "deno");
  const controller = new AbortController();
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => controller.abort());

  const { report, hostDiagnostic } = await runComposio({
    bin: findExecutable(args.get("bin") ?? "codemode", cwd),
    backend,
    deno: backend === "deno" ? findExecutable(args.get("deno") ?? "deno", cwd) : undefined,
    cwd,
    code: readFileSync(resolve(cwd, program), "utf8"),
    tools,
    timeoutMs: positive("--timeout-ms", args.get("timeout-ms") ?? 30_000),
    concurrency: positive("--concurrency", args.get("concurrency") ?? 4),
    maxCalls: positive("--max-calls", args.get("max-calls") ?? 16),
    signal: controller.signal,
    executor: runHelperExecutor(execute),
  });

  const summary = summarize(report);
  if (hostDiagnostic) summary.host_diagnostic = hostDiagnostic;
  const trace = args.get("trace");
  if (trace) {
    const path = resolve(cwd, trace);
    writeFileSync(`${path}.tmp`, JSON.stringify(report, null, 2) + "\n");
    renameSync(`${path}.tmp`, path);
    summary.trace = path;
  }
  finish(process.stdout, JSON.stringify(summary), report.status === "success" ? 0 : 1);
}

try {
  await main();
} catch (error) {
  finish(process.stderr, `composio host: ${error instanceof Error ? error.message : String(error)}`, 2);
}
