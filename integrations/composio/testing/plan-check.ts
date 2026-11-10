/**
 * Offline checks of the GitHub plan host's handlers against the stand-in helper
 * and its independent ledger. Prints one JSON object mapping each check to "ok"
 * or what went wrong; exits 1 if any failed.
 * Usage: deno run --allow-read --allow-write plan-check.ts <scratch directory>
 */
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { describeError, type PlanCall, type ReconcileRequest } from "../../shared/bridge-client.ts";
import {
  ADD_LABELS,
  CREATE_COMMENT,
  githubPlanHost,
  type Identity,
  identityFromDeclaration,
  declaration,
  parseTarget,
  REMOVE_LABEL,
  UPDATE_ISSUE,
} from "../github-plan.ts";
import { fakeConnections, fakeGithub, type GithubState, type Issue, LEAKED_ACCOUNT } from "./fake-github.ts";

const ACCOUNT = "ca_fake_bot";
const identity: Identity = { account: ACCOUNT, repo: "acme/widgets", login: "widget-bot" };
const scope = { provider: "composio.github", account: `${ACCOUNT}:widget-bot`, destination: "acme/widgets:1001" };
const MARK = "<!-- codemode:0123456789abcdef:comment-1 -->";
const many = Array.from({ length: 150 }, (_, index) => `area-${index}`);
const issue = (labels: string[] = [], extra: Partial<Issue> = {}): Issue => ({ title: "t", state: "open", labels, comments: [], ...extra });

let count = 0;
function setup(faults: Record<string, string> = {}, change: (state: GithubState) => void = () => {}) {
  const path = join(Deno.args[0], `github-${count++}.json`);
  const state: GithubState = {
    accounts: { [ACCOUNT]: "widget-bot", ca_other: "someone" },
    repositories: {
      "acme/widgets": {
        id: 1001,
        labels: ["triage", "bug"],
        permissions: { push: true, triage: true },
        issues: {
          "1": issue(),
          "2": issue(["triage"]),
          "3": issue([...many, "triage"]),
          "4": issue([...many]),
          "5": issue([], { state: "closed", state_reason: "completed" }),
          "6": issue([], { comments: [{ id: 1, user: "widget-bot", body: `found it\n\n${MARK}` }] }),
          "7": issue([], { comments: [{ id: 2, user: "mallory", body: `copied\n\n${MARK}` }] }),
          "8": issue([], { comments: Array.from({ length: 120 }, (_, id) => ({ id, user: "widget-bot", body: id === 110 ? MARK : "noise" })) }),
        },
      },
      "acme/private": { id: 2002, labels: ["triage"], permissions: { push: false, triage: false }, issues: { "1": issue() } },
    },
    faults,
    calls: [],
  };
  change(state);
  writeFileSync(path, JSON.stringify(state));
  let ended = 0;
  const host = githubPlanHost(fakeGithub(path), identity, fakeConnections(path), () => ended++);
  return { host, ended: () => ended, ledger: () => JSON.parse(Deno.readTextFileSync(path)) as GithubState };
}

const writes = (state: GithubState) => state.calls.filter((item) => [ADD_LABELS, REMOVE_LABEL, UPDATE_ISSUE, CREATE_COMMENT].includes(item.slug));
const args = (issue: number, extra: Record<string, unknown>) => ({ owner: "acme", repo: "widgets", issue_number: issue, ...extra });
const call = (tool: string, extra: Record<string, unknown>, overrides: Partial<PlanCall> = {}, issueNumber = 1): PlanCall => ({
  id: 1,
  tool,
  operation: "comment-1",
  attempt: "key-1",
  preconditions: [],
  scope,
  arguments: args(issueNumber, extra),
  ...overrides,
});
const add = (overrides: Partial<PlanCall> = {}, extra: Record<string, unknown> = {}) => call(ADD_LABELS, { labels: ["triage"], ...extra }, overrides);
const probe = (tool: string, issueNumber: number, extra: Record<string, unknown>): ReconcileRequest => ({
  id: 2,
  operation: "comment-1",
  tool,
  keys: ["key-1"],
  scope,
  arguments: args(issueNumber, extra),
});
const signal = new AbortController().signal;

const results: Record<string, string> = {};
const replies: unknown[] = [];
async function check(name: string, body: () => Promise<string | true>) {
  try {
    const verdict = await body();
    results[name] = verdict === true ? "ok" : verdict;
  } catch (error) {
    results[name] = `threw: ${describeError(error)}`;
  }
}
const expect = (actual: unknown, expected: unknown): string | true => {
  replies.push(actual);
  return JSON.stringify(actual) === JSON.stringify(expected) ? true : `got ${JSON.stringify(actual)}`;
};
const outcome = (reply: { ok: boolean; error?: { code: string }; effect?: string }) => (reply.ok ? "ok" : [reply.error?.code, reply.effect]);

// Refusals before anything is sent: no helper call at all.
for (const [name, bad, code] of [
  ["account mismatch", add({ scope: { ...scope, account: "ca_other:widget-bot" } }), "account_mismatch"],
  ["login mismatch", add({ scope: { ...scope, account: `${ACCOUNT}:someone` } }), "account_mismatch"],
  ["destination outside --repo", add({ scope: { ...scope, destination: "acme/private:2002" } }), "destination_not_allowed"],
  ["destination without a repository id", add({ scope: { ...scope, destination: "acme/widgets" } }), "destination_not_allowed"],
  ["arguments name another repository", add({}, { repo: "private" }), "destination_mismatch"],
  ["two labels", add({}, { labels: ["triage", "bug"] }), "unsupported_labels"],
  ["non-positive issue", add({}, { issue_number: 0 }), "invalid_issue"],
  ["replace-all labels", call("GITHUB_SET_LABELS_FOR_AN_ISSUE", { labels: ["triage"] }), "unsupported_tool"],
  ["a Composio meta tool", call("COMPOSIO_MULTI_EXECUTE_TOOL", {}), "unsupported_tool"],
  ["an issue update with labels", call(UPDATE_ISSUE, { state: "closed", labels: [] }), "unsupported_arguments"],
  ["an issue update with a title", call(UPDATE_ISSUE, { state: "closed", title: "x" }), "unsupported_arguments"],
  ["a reopen reason on close", call(UPDATE_ISSUE, { state: "closed", state_reason: "reopened" }), "unsupported_state"],
  ["a comment without its marker", call(CREATE_COMMENT, { body: "hello" }), "comment_unmarked"],
  ["a marker that is not the final line", call(CREATE_COMMENT, { body: `${MARK}\nmore` }), "comment_unmarked"],
  ["another operation's marker", call(CREATE_COMMENT, { body: `hi\n\n${MARK}` }, { operation: "comment-2" }), "comment_unmarked"],
  ["an unknown precondition", add({ preconditions: [{ labels: [] }] }), "unsupported_precondition"],
] as const) {
  await check(`refused before any call: ${name}`, async () => {
    const { host, ledger } = setup();
    const reply = await host.execute(bad, signal);
    if (ledger().calls.length !== 0) return `made ${ledger().calls.length} calls`;
    return expect(outcome(reply), [code, "refused"]);
  });
}

await check("a missing label is refused without a write", async () => {
  const { host, ledger } = setup();
  const reply = await host.execute(add({}, { labels: ["no-such-label"] }), signal);
  if (writes(ledger()).length !== 0) return "sent a write";
  return expect(outcome(reply), ["label_missing", "refused"]);
});

await check("an issue_state precondition that no longer holds refuses with nothing sent", async () => {
  const { host, ledger } = setup();
  const reply = await host.execute(call(UPDATE_ISSUE, { state: "closed", state_reason: "not_planned" }, { preconditions: [{ issue_state: "open" }] }, 5), signal);
  if (writes(ledger()).length !== 0) return "sent a write";
  return expect(outcome(reply), ["precondition_failed", "refused"]);
});

for (const [name, tool, extra, issueNumber, value, after] of [
  ["add a label", ADD_LABELS, { labels: ["triage"] }, 1, { label_added: true }, (state: GithubState) => state.repositories["acme/widgets"].issues["1"].labels.join() === "triage"],
  ["remove a label", REMOVE_LABEL, { name: "triage" }, 2, { label_removed: true }, (state: GithubState) => state.repositories["acme/widgets"].issues["2"].labels.length === 0],
  ["close an issue", UPDATE_ISSUE, { state: "closed", state_reason: "completed" }, 1, { state: "closed" }, (state: GithubState) => state.repositories["acme/widgets"].issues["1"].state === "closed"],
  ["comment", CREATE_COMMENT, { body: `hi\n\n${MARK}` }, 1, { comment_id: 1122 }, (state: GithubState) => state.repositories["acme/widgets"].issues["1"].comments[0]?.user === "widget-bot"],
] as const) {
  await check(`${name}: one write, as the approved account, with the approved arguments`, async () => {
    const { host, ledger } = setup();
    const reply = await host.execute(call(tool, extra, { preconditions: [{ issue_state: "open" }] }, issueNumber), signal);
    const state = ledger();
    if (new Set(state.calls.map((item) => item.account)).size !== 1 || state.calls[0].account !== ACCOUNT) return "a call used another account";
    if (writes(state).length !== 1 || !after(state)) return "not applied exactly once";
    return expect(reply, { ok: true, value });
  });
}

for (const [fault, code] of [["fail_after_apply", "github_status_500"], ["unobserved_after_apply", "composio_unobserved"]] as const) {
  await check(`a failure after dispatch is unknown (${fault})`, async () => {
    const { host, ledger } = setup({ [`${CREATE_COMMENT} acme/widgets#1`]: fault });
    const reply = await host.execute(call(CREATE_COMMENT, { body: MARK }), signal);
    if (writes(ledger()).length !== 1) return "not exactly one write: no retry";
    return expect(outcome(reply), [code, "unknown"]);
  });
}

await check("a dry-run envelope is not applied and ends the session", async () => {
  const { host, ended } = setup({ [`${ADD_LABELS} acme/widgets#1`]: "dry_run" });
  const reply = await host.execute(add(), signal);
  await new Promise((resolve) => setTimeout(resolve, 10));
  if (ended() !== 1) return "the session was not ended";
  return expect(outcome(reply), ["composio_dry_run", "not_applied"]);
});

await check("authorization re-resolves the login and repository, then checks permission", async () => {
  const { host } = setup();
  return expect((await host.authorize(scope)).ok, true);
});

for (const [name, change, code] of [
  ["inactive", (state: GithubState) => void (state.connections = [{ id: ACCOUNT, status: "EXPIRED" }]), "account_inactive"],
  ["missing", (state: GithubState) => void (state.connections = [{ id: "ca_other", status: "ACTIVE" }]), "account_inactive"],
  ["another connection's alias", (state: GithubState) => void (state.connections = [{ id: ACCOUNT, status: "ACTIVE" }, { id: "ca_other", status: "ACTIVE", alias: ACCOUNT }]), "account_ambiguous"],
  ["switched to another GitHub login", (state: GithubState) => void (state.accounts[ACCOUNT] = "someone-else"), "account_changed"],
  ["on a repository whose id changed", (state: GithubState) => void (state.repositories["acme/widgets"].id = 9999), "repository_changed"],
  ["without triage permission", (state: GithubState) => void (state.repositories["acme/widgets"].permissions = { push: false, triage: false }), "not_permitted"],
] as const) {
  await check(`authorization refuses an account ${name}`, async () => {
    const { host, ledger } = setup({}, change);
    const reply = await host.authorize(scope);
    if (writes(ledger()).length !== 0) return "sent a write";
    return expect(reply.ok ? "ok" : reply.error.code, code);
  });
}

const satisfied = (evidence: string) => ({ verdict: "satisfied", evidence });
const unknown = (evidence: string) => ({ verdict: "unknown", evidence });
for (const [name, request, expected] of [
  ["added label present: satisfied", probe(ADD_LABELS, 2, { labels: ["triage"] }), satisfied("the label is on the issue; who changed it is unknown")],
  ["added label absent: unknown", probe(ADD_LABELS, 1, { labels: ["triage"] }), unknown("the label is absent from the issue; an earlier request may still apply")],
  ["added label on page 2: satisfied", probe(ADD_LABELS, 3, { labels: ["triage"] }), satisfied("the label is on the issue; who changed it is unknown")],
  ["added label absent after every page: unknown", probe(ADD_LABELS, 4, { labels: ["triage"] }), unknown("the label is absent from the issue; an earlier request may still apply")],
  ["removed label absent: satisfied, not attributed", probe(REMOVE_LABEL, 1, { name: "triage" }), satisfied("the label is absent from the issue; who changed it is unknown")],
  ["removed label present: unknown", probe(REMOVE_LABEL, 2, { name: "triage" }), unknown("the label is still on the issue; an earlier request may still apply")],
  ["closed issue: satisfied", probe(UPDATE_ISSUE, 5, { state: "closed", state_reason: "completed" }), satisfied("the issue is closed; who changed it is unknown")],
  ["issue still open: unknown", probe(UPDATE_ISSUE, 1, { state: "closed" }), unknown("the issue's state differs; an earlier request may still apply")],
  ["closed with another reason: unknown", probe(UPDATE_ISSUE, 5, { state: "closed", state_reason: "not_planned" }), unknown("the issue's state differs; an earlier request may still apply")],
  ["our marked comment: applied", probe(CREATE_COMMENT, 6, { body: `x\n\n${MARK}` }), { verdict: "applied", evidence: "1 comment by the approved login carries this operation's marker" }],
  ["our marked comment on page 2: applied", probe(CREATE_COMMENT, 8, { body: MARK }), { verdict: "applied", evidence: "1 comment by the approved login carries this operation's marker" }],
  ["a marked comment by another user settles nothing", probe(CREATE_COMMENT, 7, { body: MARK }), unknown("a comment by another GitHub user carries this operation's marker; it settles nothing")],
  ["no marked comment: a probe error, never not_applied", probe(CREATE_COMMENT, 1, { body: MARK }), { error: { code: "marker_absent", message: "no comment carries this operation's marker yet; the request may still land" } }],
] as const) {
  await check(`reconcile: ${name}`, async () => {
    const { host } = setup();
    return expect(await host.reconcile(request, signal), expected);
  });
}

for (const [fault, code] of [["fail", "state_read_failed"], ["malformed", "state_malformed"], ["unobserved", "state_read_failed"]] as const) {
  await check(`reconcile: a ${fault} read is a probe error`, async () => {
    const { host } = setup({ "GITHUB_LIST_LABELS_FOR_AN_ISSUE acme/widgets#2": fault });
    const reply = await host.reconcile(probe(ADD_LABELS, 2, { labels: ["triage"] }), signal);
    return expect("error" in reply ? reply.error.code : reply, code);
  });
}

await check("--account accepts only a stable connected-account id", async () => {
  const outcomes = ["ca_fake_bot", "default", "work-alias", "ca_"].map((account) => {
    try {
      parseTarget(account, "acme/widgets");
      return "ok";
    } catch {
      return "refused";
    }
  });
  return expect(outcomes, ["ok", "refused", "refused", "refused"]);
});

await check("an approved declaration must be this host's for --account", async () => {
  const target = { account: ACCOUNT, repo: "acme/widgets" };
  const good = identityFromDeclaration(JSON.parse(JSON.stringify(declaration(identity))), target);
  const outcomes = [
    { ...declaration(identity), account: "ca_other:widget-bot" },
    { ...declaration(identity), retention: "metadata/1" },
    { ...declaration(identity), contracts: {} },
  ].map((value) => {
    try {
      identityFromDeclaration(value, target);
      return "ok";
    } catch {
      return "refused";
    }
  });
  return expect([good.login, ...outcomes], ["widget-bot", "refused", "refused", "refused"]);
});

await check("no reply carries Composio or GitHub text or a ca_ id from an error", async () => {
  const text = JSON.stringify(replies);
  return text.includes("MARK-SECRET") || text.includes(LEAKED_ACCOUNT) ? "a reply carried provider text" : true;
});

console.log(JSON.stringify(results));
if (Object.values(results).some((verdict) => verdict !== "ok")) Deno.exit(1);
