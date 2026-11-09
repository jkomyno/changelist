// codemode adapter: the documented workflow, with a bench plan host standing in for the
// Composio GitHub host.
//
// First attempt: investigate with the triage program through `codemode bridge` (reads only),
// write its proposal as a plan (a marker line appended to each comment), `plan check`, approve
// the printed digest, then `plan run --bridge`. Later attempts `plan resume --bridge` the same
// journal. The plan host implements the KTD9 contracts against the offline provider:
//   add labels, remove a label, set issue state: repeat unsafe, reconcile by state
//     (desired state holds -> satisfied; otherwise unknown, never not_applied);
//   create comment: repeat unsafe, reconcile by receipt (the marker, authored by the login the
//     account resolved to at approval -> applied; absent -> probe error; another author -> unknown);
//   every operation carries an issue_state precondition from investigation, checked before each
//     attempt (a mismatch refuses the attempt, sending nothing);
//   authorization re-resolves the account's login and the repository id and refuses on drift.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import {
  type PlanCall,
  type PlanReply,
  type PlanScope,
  type ProviderDeclaration,
  type Reconciled,
  type ReconcileRequest,
  runBridge,
  runPlanBridge,
} from "../../../integrations/shared/bridge-client.ts";
import { adapterOptions, type Claim, EXIT, program, ROOT, WIDEN_OP, writeClaims } from "../lib/adapter.ts";
import { type Execute, providerClient } from "../lib/client.ts";
import { ACCOUNT, OWNER, READ_SLUGS, READS, REPO, REPO_ID, WRITES } from "../lib/github-state.ts";

const options = adapterOptions();
const BIN = process.env.CODEMODE_BIN ?? join(ROOT, "target", "release", "codemode");
const PROVIDER = "bench.github";
const DESTINATION = `${OWNER}/${REPO}`;
const dir = join(options.runDir, "codemode");
mkdirSync(dir, { recursive: true });
const journal = join(dir, "journal.db");
const execute: Execute = providerClient(options.provider);
const say = (tag: string, value: unknown) => process.stdout.write(`@@${tag} ${JSON.stringify(value)}\n`);

const declaration: ProviderDeclaration = {
  provider: PROVIDER,
  account: ACCOUNT,
  contracts: {
    [WRITES.addLabels]: { repeat: "unsafe", reconcile: "state" },
    [WRITES.removeLabel]: { repeat: "unsafe", reconcile: "state" },
    [WRITES.updateIssue]: { repeat: "unsafe", reconcile: "state" },
    [WRITES.createComment]: { repeat: "unsafe", reconcile: "receipt" },
  },
  retention: "summary/1",
};

function codemode(args: string[]): { status: number; stdout: string; stderr: string } {
  const result = spawnSync(BIN, args, { encoding: "utf8", env: {} });
  return { status: result.status ?? -1, stdout: result.stdout, stderr: result.stderr };
}

/** Run a read and unwrap it; a failure throws. */
async function read(slug: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<any> {
  const envelope = await execute(slug, args, { account: ACCOUNT, signal });
  if (!envelope.successful) throw new Error(envelope.error ?? `${slug} failed`);
  return envelope.data;
}

// ---- investigation: the triage program proposes, it never writes --------------------------------

async function investigate(): Promise<{ ops: Array<{ id: string; tool: string; arguments: Record<string, unknown> }>; status: string }> {
  const code = program(options.workload, "async function gh(slug, args) { return await tools.call(slug, args); }", "return await decide();");
  const report = await runBridge({
    bin: BIN,
    cwd: dir,
    code,
    tools: READ_SLUGS,
    // The bridge's own defaults (`codemode bridge --help`) unless the equal-limits row sets them.
    timeoutMs: options.deadlineMs ?? 5000,
    concurrency: options.concurrency ?? 4,
    maxCalls: options.maxCalls ?? 32,
    heapMb: options.heapMb,
    async execute(call, signal) {
      try {
        return { ok: true, value: await read(call.tool, call.arguments as Record<string, unknown>, signal) };
      } catch (error) {
        return { ok: false, error: { code: "provider_error", message: error instanceof Error ? error.message : String(error) } };
      }
    },
  });
  say("INVESTIGATION", { status: report.status, calls: report.calls.length, diagnostic: report.diagnostic ?? null });
  return { ops: Array.isArray(report.output) ? (report.output as never) : [], status: report.status };
}

/** The proposal-to-plan step (KTD11): a random plan id lives only in each comment's marker line. */
function writePlan(ops: Array<{ id: string; tool: string; arguments: Record<string, unknown> }>): void {
  const planId = Math.random().toString(36).slice(2, 10);
  const scope = { provider: PROVIDER, account: ACCOUNT, destination: DESTINATION };
  const plan = {
    version: 1,
    summary: "Triage open bug reports and close duplicates in bench/widgets",
    operations: ops.map((op) => ({
      id: op.id,
      tool: op.tool,
      scope,
      arguments:
        op.tool === WRITES.createComment ? { ...op.arguments, body: `${op.arguments.body}\n<!-- codemode:${planId}:${op.id} -->` } : op.arguments,
      preconditions: [{ issue_state: "open" }],
    })),
  };
  writeFileSync(join(dir, "plan.json"), `${JSON.stringify(plan, null, 2)}\n`);
  writeFileSync(join(dir, "provider.json"), `${JSON.stringify(declaration)}\n`);
}

function checkAndApprove(): { digest: string } {
  const check = codemode(["plan", "check", join(dir, "plan.json"), "--provider", join(dir, "provider.json"), "--json"]);
  if (check.status !== 0) throw new Error(`plan check failed: ${check.stderr}`);
  const digest = JSON.parse(check.stdout).digest as string;
  const approve = codemode([
    "plan", "approve", join(dir, "plan.json"), "--journal", journal, "--provider", join(dir, "provider.json"),
    "--digest", digest, "--approver", "bench", ...(options.concurrency ? ["--concurrency", String(options.concurrency)] : []),
  ]);
  if (approve.status !== 0) throw new Error(`plan approve failed: ${approve.stderr}`);
  // The host's own binding, fixed at approval like KTD9's declaration: the login and repository id.
  writeFileSync(join(dir, "host.json"), `${JSON.stringify({ login: hostLogin, repository_id: REPO_ID, digest })}\n`);
  return { digest };
}

/**
 * The re-approval property: a change list widened after approval. The edited file is refused
 * with the reviewed digest, and the journal keeps executing exactly what was approved.
 */
function widenAfterApproval(digest: string): unknown {
  const plan = JSON.parse(readFileSync(join(dir, "plan.json"), "utf8"));
  plan.operations.push({ ...WIDEN_OP, scope: plan.operations[0].scope, preconditions: [{ issue_state: "open" }] });
  writeFileSync(join(dir, "plan.json"), `${JSON.stringify(plan, null, 2)}\n`);
  const reuse = codemode([
    "plan", "approve", join(dir, "plan.json"), "--journal", join(dir, "widened.db"), "--provider", join(dir, "provider.json"),
    "--digest", digest, "--approver", "bench",
  ]);
  return { approve_widened_with_reviewed_digest: { exit: reuse.status, stderr: reuse.stderr.trim().slice(0, 300) } };
}

// ---- the bench plan host --------------------------------------------------------------------------

let hostLogin = "";

function issueOf(args: Record<string, unknown>): number {
  if (args.owner !== OWNER || args.repo !== REPO) throw new Error("destination_mismatch");
  return args.issue_number as number;
}

async function authorize(scope: PlanScope) {
  if (scope.provider !== PROVIDER || scope.account !== ACCOUNT || scope.destination !== DESTINATION) {
    return { ok: false as const, error: { code: "scope_mismatch", message: "scope is not the approved account and repository" } };
  }
  const bound = JSON.parse(readFileSync(join(dir, "host.json"), "utf8"));
  try {
    const user = await read(READS.user, {});
    if (user.login !== bound.login) return { ok: false as const, error: { code: "account_drift", message: "the account resolves to another login" } };
    const repo = await read(READS.repository, { owner: OWNER, repo: REPO });
    if (repo.id !== bound.repository_id || repo.permissions?.triage !== true) {
      return { ok: false as const, error: { code: "repository_drift", message: "repository id or permission changed" } };
    }
  } catch {
    return { ok: false as const, error: { code: "authorization_unverified", message: "the account could not be verified now" } };
  }
  return { ok: true as const };
}

async function executeAttempt(call: PlanCall, signal: AbortSignal): Promise<PlanReply> {
  if (!(call.tool in declaration.contracts)) {
    return { ok: false, error: { code: "unsupported_tool", message: "tool has no contract" }, effect: "refused" };
  }
  let issue: number;
  try {
    issue = issueOf(call.arguments);
  } catch {
    return { ok: false, error: { code: "destination_mismatch", message: "arguments name another repository" }, effect: "refused" };
  }
  for (const pre of call.preconditions as Array<Record<string, unknown>>) {
    let current: any;
    try {
      current = await read(READS.getIssue, { owner: OWNER, repo: REPO, issue_number: issue }, signal);
    } catch {
      // The write was never sent.
      return { ok: false, error: { code: "precondition_unverified", message: "issue state could not be read" }, effect: "not_applied" };
    }
    if (current.state !== pre.issue_state) {
      return { ok: false, error: { code: "precondition_failed", message: `issue is ${current.state}` }, effect: "refused" };
    }
  }
  // From here the write is handed to the provider: any failure is unknown.
  const envelope = await execute(call.tool, call.arguments, { account: ACCOUNT, signal });
  if (envelope.successful) return { ok: true, value: { applied: true } };
  return { ok: false, error: { code: "provider_error", message: (envelope.error ?? "failed").slice(0, 200) }, effect: "unknown" };
}

async function reconcile(request: ReconcileRequest, signal: AbortSignal): Promise<Reconciled> {
  const args = request.arguments;
  const issue = issueOf(args);
  const target = { owner: OWNER, repo: REPO, issue_number: issue };
  if (request.tool === WRITES.createComment) {
    const bound = JSON.parse(readFileSync(join(dir, "host.json"), "utf8"));
    const body = String(args.body);
    const marker = body.split("\n").at(-1)!;
    const comments: Array<{ user: { login: string }; body: string }> = await read(READS.listComments, target, signal);
    const marked = comments.filter((c) => c.body.split("\n").at(-1) === marker);
    if (marked.some((c) => c.user.login === bound.login)) return { verdict: "applied", evidence: "marker found, authored by the approved login" };
    if (marked.length > 0) return { verdict: "unknown", evidence: "marker found, authored by another login" };
    // Absence is not proof: the request may still land.
    return { error: { code: "receipt_absent", message: "marker not found yet" } };
  }
  const current = await read(READS.getIssue, target, signal);
  const labels: string[] = current.labels.map((l: { name: string }) => l.name);
  const holds =
    request.tool === WRITES.addLabels
      ? (args.labels as string[]).every((l) => labels.includes(l))
      : request.tool === WRITES.removeLabel
        ? !labels.includes(String(args.name))
        : current.state === args.state;
  return holds ? { verdict: "satisfied", evidence: "desired state holds" } : { verdict: "unknown", evidence: "desired state does not hold yet" };
}

/** `plan run|resume` through a wrapper only to pass `--deadline-ms`, which runPlanBridge does not set. */
function planBin(): string {
  if (options.deadlineMs === undefined) return BIN;
  const wrapper = join(dir, "codemode-deadline.sh");
  writeFileSync(wrapper, `#!/bin/sh\nexec '${BIN}' "$@" --deadline-ms ${options.deadlineMs}\n`, { mode: 0o755 });
  return wrapper;
}

/** Durable facts committed to the journal so far (its ordinal clock), read without a writer lock. */
function journalRecords(): number | null {
  try {
    const db = new DatabaseSync(journal, { readOnly: true });
    try {
      return (db.prepare("SELECT ordinal FROM clock").get() as { ordinal: number }).ordinal;
    } finally {
      db.close();
    }
  } catch {
    return null;
  }
}

// ---- main ------------------------------------------------------------------------------------------

async function main(): Promise<number> {
  if (options.workload !== "triage") {
    const { status } = await investigate().catch((error: Error) => ({ status: `adapter_error: ${error.message}` }));
    writeClaims(options, { engine: "codemode", run_success: false, run_status: status, ops: {}, review: [] });
    return EXIT.failed;
  }
  let kind: "run" | "resume" = "resume";
  let widened: unknown;
  if (!existsSync(join(dir, "host.json"))) {
    // Nothing approved yet (or a crash before approval): investigate from scratch.
    hostLogin = (await read(READS.user, {})).login;
    const { ops, status } = await investigate();
    if (status !== "success") {
      writeClaims(options, { engine: "codemode", run_success: false, run_status: `investigation ${status}`, ops: {}, review: [] });
      return EXIT.failed;
    }
    writePlan(ops);
    const { digest } = checkAndApprove();
    if (options.widen) widened = widenAfterApproval(digest);
    kind = "run";
  } else if (!/"kind":\s*"run"/.test(codemode(["plan", "inspect", "--journal", journal, "--json"]).stdout)) {
    kind = "run";
  }
  const { outcome, diagnostic } = await runPlanBridge({
    bin: planBin(),
    cwd: dir,
    journal,
    kind,
    declaration,
    authorize,
    execute: executeAttempt,
    reconcile,
  });
  // The journal, read back like an operator would (`plan inspect --json`), is the authority on what
  // may happen next across sessions; the session's outcome frame only describes this session.
  const inspected = codemode(["plan", "inspect", "--journal", journal, "--json"]);
  const values: Array<{ operation: string; conclusion: string; next: string }> =
    inspected.status === 0 ? JSON.parse(inspected.stdout).operations : (outcome.operations ?? []);
  const ops: Record<string, Claim> = {};
  const raw: Record<string, string> = {};
  const review: string[] = [];
  for (const op of values) {
    ops[op.operation] = op.conclusion as Claim;
    raw[op.operation] = `${op.conclusion}/${op.next}`;
    if (op.next === "review") review.push(op.operation);
  }
  const success = outcome.status !== "refused" && values.length > 0 && values.every((op) => op.conclusion === "applied" || op.conclusion === "satisfied");
  writeClaims(options, {
    engine: "codemode",
    run_success: success,
    run_status: `${kind}: ${outcome.status}${diagnostic ? ` (${diagnostic})` : ""}`,
    ops,
    review,
    raw,
    durable_records: journalRecords(),
    detail: widened,
  });
  if (outcome.status === "refused") return EXIT.refused;
  if (success) return EXIT.success;
  return values.some((op) => op.next === "dispatch" || op.next === "reconcile") ? EXIT.incomplete : EXIT.failed;
}

main().then(
  (code) => process.exit(code),
  (error: unknown) => {
    process.stderr.write(`codemode adapter: ${error instanceof Error ? error.stack : String(error)}\n`);
    process.exit(EXIT.adapter);
  },
);
