// Offline GitHub provider shared by every implementation, as its own process.
//
//   node bench/workflow/provider.ts --ledger <file> [--latency-ms 20]
//
// It prints `@@READY {"port":N}` once listening on 127.0.0.1, then serves:
//
//   POST /execute  {"slug","arguments","account"} -> {"successful","data","error"}
//       Composio-shaped envelope for the reads and the four writes in lib/github-state.ts.
//       There are no idempotency keys: real Composio GitHub tools have none.
//   POST /control  {"action":...}  used only by the benchmark driver (run.ts):
//       faults {faults:[...]}   replace the fault plan and reset its counters
//       account {status?, login?}  revoke the connected account or switch the login it resolves to
//       release                 apply every held effect now (a late effect)
//       unpause                 drop requests held by a pause fault (their callers are dead)
//       maintainer {slug, args} apply a write as another GitHub user ("maintainer")
//
// The provider owns the ledger: an append-only JSONL file, fsynced after every record, with
// `start` (request received; recorded after the simulated latency, with the receipt time in
// `received`), `effect_applied` (state changed), `held` (accepted, effect
// deferred), `terminal` (reply produced; `delivered` says whether the caller was still
// listening), and `control` (a driver action). State is the fixture plus every `effect_applied`
// record, so it survives a SIGKILL of any engine. A lost response has `start` and
// `effect_applied` and no `terminal`.
//
// Faults ({kind, slug?|class?: "read"|"write", nth}): nth counts matching requests since the
// last `faults` control. A fault fires once; the driver re-arms faults per attempt.
//   lost_response       apply the effect, never reply
//   hold                record `held`, never reply; the effect applies at `release`
//   pause_before        record nothing, hold this and every later request (the kill point
//                       "before the request reached GitHub")
//   pause_after         reply normally, then hold every later request unrecorded
//   ignore_cancel       wait delayMs, then apply and reply; a caller abort changes nothing
//   slow_reply          apply, then send the reply body in slices over delayMs
//   reset_after_effect  apply, then destroy the connection without a reply
//   mutate_after        reply normally, then apply `change` as the maintainer
// Each fault that strands a caller prints `@@ARMED {...}` so the driver can kill at that point.
import { closeSync, fsyncSync, openSync, writeSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";
import { say } from "./lib/adapter.ts";
import {
  ACCOUNT,
  applyWrite,
  freshState,
  GithubError,
  type Issue,
  LOGIN,
  type LedgerRecord,
  OWNER,
  READS,
  REPO,
  REPO_ID,
  REPO_LABELS,
  validateWrite,
  WRITE_SLUGS,
} from "./lib/github-state.ts";

interface Fault {
  kind: string;
  slug?: string;
  class?: "read" | "write";
  nth?: number;
  delayMs?: number;
  change?: { slug: string; args: Record<string, unknown> };
}

const { values: opt } = parseArgs({
  options: { ledger: { type: "string" }, "latency-ms": { type: "string", default: "20" } },
});
if (!opt.ledger) throw new Error("--ledger is required");
const latencyMs = Number(opt["latency-ms"]);

const fd = openSync(opt.ledger, "a");
let seq = 0;
const issues: Issue[] = freshState();
const account = { status: "active", login: LOGIN };
let faults: Fault[] = [];
let counts = new Map<Fault, number>();
let calls = 0;
let paused = false;
const pausedResponses: ServerResponse[] = [];
const held: Array<{ call_id: string; slug: string; args: Record<string, unknown>; actor: string; res: ServerResponse }> = [];

function append(record: Omit<LedgerRecord, "seq" | "ts">): void {
  const line = `${JSON.stringify({ seq: ++seq, ts: Date.now(), ...record })}\n`;
  writeSync(fd, line);
  fsyncSync(fd);
}

const armed = (detail: Record<string, unknown>) => say("ARMED", detail);

function faultFor(slug: string): Fault | undefined {
  const cls = WRITE_SLUGS.includes(slug) ? "write" : "read";
  let chosen: Fault | undefined;
  for (const fault of faults) {
    if (fault.slug ? fault.slug !== slug : fault.class !== cls) continue;
    const n = (counts.get(fault) ?? 0) + 1;
    counts.set(fault, n);
    if (n === (fault.nth ?? 1) && !chosen) chosen = fault;
  }
  return chosen;
}

/** Whether the caller is still waiting for this response. */
const listening = (res: ServerResponse) => !res.destroyed && !res.writableEnded && !res.socket?.destroyed;

function send(res: ServerResponse, call_id: string, body: unknown, status: string): void {
  const delivered = listening(res);
  if (delivered) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  }
  append({ event: "terminal", call_id, status, delivered });
}

const ok = (data: unknown) => ({ successful: true, data, error: null });
const failure = (status: number, message: string) => ({ successful: false, data: { status_code: status }, error: `${status} ${message}` });

function issueView(item: Issue) {
  return { number: item.number, title: item.title, state: item.state, state_reason: item.state_reason, labels: item.labels.map((name) => ({ name })) };
}

function read(slug: string, args: Record<string, unknown>): unknown {
  switch (slug) {
    case READS.user:
      return { login: account.login };
    case READS.repository:
      if (args.owner !== OWNER || args.repo !== REPO) throw new GithubError(404, "repository not found");
      return { id: REPO_ID, full_name: `${OWNER}/${REPO}`, permissions: { triage: true, push: true }, labels: REPO_LABELS };
    case READS.listIssues: {
      if (args.owner !== OWNER || args.repo !== REPO) throw new GithubError(404, "repository not found");
      const state = args.state ?? "open";
      return issues.filter((item) => state === "all" || item.state === state).map(issueView);
    }
    case READS.getIssue: {
      const item = issues.find((i) => i.number === args.issue_number);
      if (!item || args.owner !== OWNER || args.repo !== REPO) throw new GithubError(404, "issue not found");
      return issueView(item);
    }
    case READS.listComments: {
      const item = issues.find((i) => i.number === args.issue_number);
      if (!item || args.owner !== OWNER || args.repo !== REPO) throw new GithubError(404, "issue not found");
      return item.comments.map((c) => ({ id: c.id, user: { login: c.author }, body: c.body }));
    }
    default:
      throw new GithubError(404, `unknown tool ${slug}`);
  }
}

function applyAndRecord(call_id: string, slug: string, args: Record<string, unknown>, actor: string, extra: Partial<LedgerRecord> = {}): void {
  applyWrite(issues, slug, args, actor);
  append({ event: "effect_applied", call_id, slug, args, actor, ...extra });
}

async function execute(body: { slug: string; arguments: Record<string, unknown>; account: string }, res: ServerResponse): Promise<void> {
  const { slug, arguments: args = {} } = body;
  // When the request reached the provider; `start` is recorded after the simulated latency.
  const received = Date.now();
  if (paused) {
    pausedResponses.push(res);
    return;
  }
  await sleep(latencyMs);
  if (paused) {
    pausedResponses.push(res);
    return;
  }
  const fault = faultFor(slug);
  if (fault?.kind === "pause_before") {
    paused = true;
    pausedResponses.push(res);
    armed({ kind: fault.kind, slug });
    return;
  }
  const call_id = `c${++calls}`;
  const actor = account.login;
  append({ event: "start", call_id, slug, args, account: body.account, actor, received });
  if (body.account !== ACCOUNT || account.status !== "active") {
    return send(res, call_id, failure(403, "the connected account is not active"), "denied");
  }
  const isWrite = WRITE_SLUGS.includes(slug);
  try {
    if (!isWrite) {
      const data = read(slug, args);
      send(res, call_id, ok(data), "ok");
    } else {
      validateWrite(issues, slug, args);
      switch (fault?.kind) {
        case "hold":
          append({ event: "held", call_id, slug, args, actor });
          held.push({ call_id, slug, args, actor, res });
          armed({ kind: fault.kind, slug, call_id });
          return;
        case "ignore_cancel":
          await sleep(fault.delayMs ?? 3000);
          applyAndRecord(call_id, slug, args, actor, { late: true, cause: "ignored_cancel" });
          return send(res, call_id, ok({ applied: true }), "ok");
        case "lost_response":
          applyAndRecord(call_id, slug, args, actor);
          armed({ kind: fault.kind, slug, call_id });
          return;
        case "reset_after_effect":
          applyAndRecord(call_id, slug, args, actor);
          append({ event: "terminal", call_id, status: "reset", delivered: false });
          res.socket?.destroy();
          armed({ kind: fault.kind, slug, call_id });
          return;
        case "slow_reply": {
          applyAndRecord(call_id, slug, args, actor);
          const text = JSON.stringify(ok({ applied: true }));
          res.writeHead(200, { "content-type": "application/json" });
          const slices = 20;
          const step = Math.ceil(text.length / slices);
          for (let i = 0; i < text.length; i += step) {
            if (!listening(res)) break;
            res.write(text.slice(i, i + step));
            await sleep((fault.delayMs ?? 3000) / slices);
          }
          const delivered = listening(res);
          if (delivered) res.end();
          append({ event: "terminal", call_id, status: "ok", delivered });
          return;
        }
        default:
          applyAndRecord(call_id, slug, args, actor);
          send(res, call_id, ok({ applied: true }), "ok");
      }
    }
  } catch (error) {
    if (!(error instanceof GithubError)) throw error;
    return send(res, call_id, failure(error.status, error.message), "error");
  }
  if (fault?.kind === "pause_after") {
    paused = true;
    armed({ kind: fault.kind, slug, call_id });
  } else if (fault?.kind === "mutate_after" && fault.change) {
    applyAndRecord(`m${++calls}`, fault.change.slug, fault.change.args, "maintainer");
  }
}

function control(body: Record<string, unknown>): unknown {
  append({ event: "control", action: String(body.action), detail: body });
  switch (body.action) {
    case "faults":
      faults = (body.faults as Fault[]) ?? [];
      counts = new Map();
      return { faults: faults.length };
    case "account":
      if (typeof body.status === "string") account.status = body.status;
      if (typeof body.login === "string") account.login = body.login;
      return account;
    case "release": {
      const released = held.splice(0);
      for (const h of released) {
        try {
          applyAndRecord(h.call_id, h.slug, h.args, h.actor, { late: true, cause: "hold_released" });
          send(h.res, h.call_id, ok({ applied: true }), "ok");
        } catch (error) {
          // State moved on while the request was held (for example the label is already gone).
          if (!(error instanceof GithubError)) throw error;
          send(h.res, h.call_id, failure(error.status, error.message), "error");
        }
      }
      return { released: released.length };
    }
    case "unpause": {
      paused = false;
      for (const res of pausedResponses.splice(0)) res.socket?.destroy();
      return { paused };
    }
    case "maintainer": {
      const change = body as unknown as { slug: string; args: Record<string, unknown> };
      applyAndRecord(`m${++calls}`, change.slug, change.args, "maintainer");
      return { applied: true };
    }
    default:
      throw new Error(`unknown control action ${String(body.action)}`);
  }
}

async function readBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

const server = createServer((req, res) => {
  readBody(req)
    .then((body) => {
      if (req.url === "/execute") return execute(body as never, res);
      if (req.url === "/control") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify(control(body)));
        return;
      }
      res.writeHead(404).end();
    })
    .catch((error: unknown) => {
      process.stderr.write(`provider: ${error instanceof Error ? error.stack : String(error)}\n`);
      if (listening(res)) res.writeHead(500).end();
    });
});
server.keepAliveTimeout = 60_000;
server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  process.stdout.write(`@@READY ${JSON.stringify({ port: typeof address === "object" && address ? address.port : 0 })}\n`);
});
const stop = () => {
  closeSync(fd);
  process.exit(0);
};
process.on("SIGTERM", stop);
process.on("SIGINT", stop);
