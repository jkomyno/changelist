// The competent Composio-native script baseline (R28), published with the results.
//
// It is written the way a careful developer would write the triage workflow against the
// `execute(slug, arguments, { account })` helper of `composio run`, with no codemode, no engine,
// and no provider idempotency keys (Composio's GitHub tools have none):
//
// 1. First run: resolve its own GitHub login, read the open issues and their comments, decide the
//    same writes as the shared triage program, and append the decision (every operation, with the
//    issue state it was decided on) to an fsynced progress file before any write.
// 2. Before each write, read the issue's current state. If the desired state already holds (label
//    present/absent, issue closed, or its own marked comment, authored by its own login, exists),
//    record the operation done without writing. Otherwise, if the issue's state differs from the
//    one the decision was made on, skip the write ("state_changed").
// 3. Append `started` (fsynced) before sending and `done` (fsynced) after a successful reply. An
//    error reply is recorded as `failed`; no reply at all (a reset, a timeout) as `unknown`, since
//    the request may have reached GitHub. Either way the script goes on.
// 4. Exit 0 when every operation is done or skipped; 3 when some failed or are unknown (running
//    it again is the recovery: it resumes from the progress file and repeats step 2 for each
//    unfinished one); 1 when a read it needs fails, which stops it before any further write.
//
// What it cannot know: when its own marked comment is absent after a failure or a crash, it
// cannot tell "never landed" from "not landed yet", so it posts again. It binds the login it
// resolved on the first run only for recognizing its own comments; it does not re-verify which
// account the selector resolves to.
//
// Limits: host default sets none. The equal-limits row sets one deadline for the run
// (`--deadline-ms`), enforced with an AbortSignal on every call and a check between calls.
//
// `--blind-retry` is a deliberately broken configuration used only to verify the oracle: it skips
// step 2 and re-sends every unfinished operation.
import { closeSync, existsSync, fsyncSync, openSync, readFileSync, writeSync } from "node:fs";
import { join } from "node:path";
import { adapterOptions, type Claim, EXIT, WIDEN_OP, writeClaims } from "../lib/adapter.ts";
import { type Envelope, providerClient } from "../lib/client.ts";
import { ACCOUNT, COMMENT_TEXT, OWNER, READS, REPO, WRITES } from "../lib/github-state.ts";

const options = adapterOptions();
const blindRetry = process.argv.includes("--blind-retry");
const execute = providerClient(options.provider);
const progressPath = join(options.runDir, "progress.jsonl");
const deadline = options.deadlineMs === undefined ? undefined : Date.now() + options.deadlineMs;

interface Op {
  id: string;
  tool: string;
  arguments: Record<string, unknown>;
  issue_state: string;
}
type Progress =
  | { type: "decided"; login: string; ops: Op[] }
  | { type: "started" | "done" | "failed" | "unknown" | "skipped"; op: string; note?: string };

function record(entry: Progress): void {
  const fd = openSync(progressPath, "a");
  try {
    writeSync(fd, `${JSON.stringify(entry)}\n`);
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
}

function load(): Progress[] {
  if (!existsSync(progressPath)) return [];
  return readFileSync(progressPath, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

class Stop extends Error {}
/** GitHub answered, and the answer was an error. */
class Replied extends Error {}

async function gh(slug: string, args: Record<string, unknown>): Promise<any> {
  if (deadline !== undefined && Date.now() >= deadline) throw new Stop("deadline reached");
  const signal = deadline === undefined ? undefined : AbortSignal.timeout(Math.max(1, deadline - Date.now()));
  const envelope: Envelope = await execute(slug, args, { account: ACCOUNT, signal });
  if (!envelope.successful) throw new Replied(envelope.error ?? `${slug} failed`);
  return envelope.data;
}

const target = (n: number) => ({ owner: OWNER, repo: REPO, issue_number: n });

async function decide(): Promise<{ login: string; ops: Op[] }> {
  const login = (await gh(READS.user, {})).login as string;
  const issues = await gh(READS.listIssues, { owner: OWNER, repo: REPO, state: "open" });
  const ops: Op[] = [];
  for (const issue of issues) {
    const n = issue.number as number;
    const labels: string[] = issue.labels.map((l: { name: string }) => l.name);
    const base = { issue_state: issue.state as string };
    if (labels.includes("duplicate")) {
      ops.push({ id: `close-${n}`, tool: WRITES.updateIssue, arguments: { ...target(n), state: "closed", state_reason: "not_planned" }, ...base });
      continue;
    }
    if (!labels.includes("bug")) continue;
    if (!labels.includes("triaged")) ops.push({ id: `add-label-${n}`, tool: WRITES.addLabels, arguments: { ...target(n), labels: ["triaged"] }, ...base });
    if (labels.includes("needs-triage")) ops.push({ id: `remove-label-${n}`, tool: WRITES.removeLabel, arguments: { ...target(n), name: "needs-triage" }, ...base });
    const comments: Array<{ body: string }> = await gh(READS.listComments, target(n));
    if (!comments.some((c) => c.body.startsWith(COMMENT_TEXT))) {
      const body = `${COMMENT_TEXT}\n<!-- composio-script:comment-${n} -->`;
      ops.push({ id: `comment-${n}`, tool: WRITES.createComment, arguments: { ...target(n), body }, ...base });
    }
  }
  if (options.widen) ops.push({ ...WIDEN_OP, issue_state: "open" });
  return { login, ops };
}

/** Whether the operation's desired state already holds, from a fresh read. */
async function alreadyDone(op: Op, login: string, issue: any): Promise<boolean> {
  const labels: string[] = issue.labels.map((l: { name: string }) => l.name);
  switch (op.tool) {
    case WRITES.addLabels:
      return (op.arguments.labels as string[]).every((l) => labels.includes(l));
    case WRITES.removeLabel:
      return !labels.includes(String(op.arguments.name));
    case WRITES.updateIssue:
      return issue.state === op.arguments.state;
    case WRITES.createComment: {
      const marker = String(op.arguments.body).split("\n").at(-1);
      const comments: Array<{ user: { login: string }; body: string }> = await gh(READS.listComments, target(op.arguments.issue_number as number));
      return comments.some((c) => c.user.login === login && c.body.split("\n").at(-1) === marker);
    }
    default:
      return false;
  }
}

async function main(): Promise<number> {
  let history = load();
  let decided = history.find((p) => p.type === "decided") as Extract<Progress, { type: "decided" }> | undefined;
  if (!decided) {
    try {
      decided = { type: "decided", ...(await decide()) };
    } catch (error) {
      writeClaims(options, { engine: "composio-script", run_success: false, run_status: `stopped: ${String((error as Error).message)}`, ops: {}, review: [] });
      return EXIT.failed;
    }
    record(decided);
    history = [decided];
  }
  const last = new Map<string, Progress>();
  for (const p of history) if (p.type !== "decided") last.set(p.op, p);
  const ops: Record<string, Claim> = {};
  const raw: Record<string, string> = {};
  let stopped: string | undefined;
  for (const op of decided.ops) {
    const previous = last.get(op.id);
    if (previous?.type === "done" || previous?.type === "skipped") {
      ops[op.id] = previous.type === "done" ? "applied" : "refused";
      raw[op.id] = previous.note ?? previous.type;
      continue;
    }
    // What the progress file says about an unfinished operation: never started, or no proof either way.
    const unfinished: Claim = previous ? (previous.type === "failed" ? "failed" : "unknown") : "pending";
    if (stopped) {
      ops[op.id] = unfinished;
      continue;
    }
    try {
      if (!blindRetry) {
        const issue = await gh(READS.getIssue, target(op.arguments.issue_number as number));
        if (await alreadyDone(op, decided.login, issue)) {
          record({ type: "done", op: op.id, note: "already" });
          ops[op.id] = "applied";
          raw[op.id] = "already done";
          continue;
        }
        if (issue.state !== op.issue_state) {
          record({ type: "skipped", op: op.id, note: "state_changed" });
          ops[op.id] = "refused";
          raw[op.id] = "skipped: issue state changed";
          continue;
        }
      }
    } catch (error) {
      // A read it needs failed: stop before writing anything else.
      stopped = String((error as Error).message);
      ops[op.id] = unfinished;
      continue;
    }
    if (deadline !== undefined && Date.now() >= deadline) {
      stopped = "deadline reached";
      ops[op.id] = unfinished;
      continue;
    }
    record({ type: "started", op: op.id });
    try {
      await gh(op.tool, op.arguments);
      record({ type: "done", op: op.id });
      ops[op.id] = "applied";
      raw[op.id] = "done";
    } catch (error) {
      const message = String((error as Error).message).slice(0, 200);
      const replied = error instanceof Replied;
      record({ type: replied ? "failed" : "unknown", op: op.id, note: message });
      ops[op.id] = replied ? "failed" : "unknown";
      raw[op.id] = `${replied ? "failed" : "no reply"}: ${message}`;
      if (error instanceof Stop) stopped = message;
    }
  }
  const values = Object.values(ops);
  const success = !stopped && values.every((c) => c === "applied" || c === "refused");
  writeClaims(options, {
    engine: blindRetry ? "composio-script-blind-retry" : "composio-script",
    run_success: success,
    run_status: stopped ? `stopped: ${stopped}` : success ? "done" : "failures remain",
    ops,
    review: Object.keys(ops).filter((id) => ops[id] === "unknown"),
    raw,
    durable_records: load().length,
  });
  if (success) return EXIT.success;
  return stopped && !/deadline|abort|timeout/i.test(stopped) ? EXIT.failed : EXIT.incomplete;
}

main().then(
  (code) => process.exit(code),
  (error: unknown) => {
    process.stderr.write(`composio script: ${error instanceof Error ? error.stack : String(error)}\n`);
    process.exit(EXIT.adapter);
  },
);
