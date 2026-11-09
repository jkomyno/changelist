/**
 * Offline checks of the Composio plan host's handlers against the stand-in
 * helper and its independent ledger. Prints one JSON object mapping each check
 * to "ok" or what went wrong; exits 1 if any failed.
 * Usage: deno run --allow-read --allow-write plan-check.ts <scratch directory>
 */
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import type { PlanCall, ReconcileRequest } from "../../shared/bridge-client.ts";
import { ADD_LABELS, type ComposioPlanConfig, composioPlanHost } from "../composio-plan.ts";
import { fakeLabels, type LabelState } from "./fake-labels.ts";

const ACCOUNT = "ca_fake_bot";
const config: ComposioPlanConfig = { account: ACCOUNT, destinations: ["acme/widgets"] };
const scope = { provider: "composio.github", account: ACCOUNT, destination: "acme/widgets" };
const many = Array.from({ length: 150 }, (_, index) => `area-${index}`);

let count = 0;
function setup(faults: Record<string, string> = {}) {
  const path = join(Deno.args[0], `labels-${count++}.json`);
  const state: LabelState = {
    accounts: [ACCOUNT, "ca_other"],
    repositories: {
      "acme/widgets": {
        labels: ["triage", "bug"],
        permissions: { push: true, triage: true },
        issues: { "1": [], "2": ["triage"], "3": [...many, "triage"], "4": [...many] },
      },
      "acme/private": { labels: ["triage"], permissions: { push: false, triage: false }, issues: { "1": [] } },
    },
    faults,
    calls: [],
  };
  writeFileSync(path, JSON.stringify(state));
  return { host: composioPlanHost(fakeLabels(path), config), ledger: () => JSON.parse(Deno.readTextFileSync(path)) as LabelState };
}

const call = (overrides: Partial<PlanCall> = {}, args: Record<string, unknown> = {}): PlanCall => ({
  id: 1,
  tool: ADD_LABELS,
  operation: "label-1",
  attempt: "key-1",
  preconditions: [],
  scope,
  ...overrides,
  arguments: { owner: "acme", repo: "widgets", issue_number: 1, labels: ["triage"], ...args },
});
const probe = (issue: number): ReconcileRequest => ({
  id: 2,
  operation: `label-${issue}`,
  tool: ADD_LABELS,
  keys: ["key-1"],
  scope,
  arguments: { owner: "acme", repo: "widgets", issue_number: issue, labels: ["triage"] },
});
const signal = new AbortController().signal;

const results: Record<string, string> = {};
const replies: unknown[] = [];
async function check(name: string, body: () => Promise<string | true>) {
  try {
    const verdict = await body();
    results[name] = verdict === true ? "ok" : verdict;
  } catch (error) {
    results[name] = `threw: ${error instanceof Error ? error.message : String(error)}`;
  }
}
const expect = (actual: unknown, expected: unknown): string | true => {
  replies.push(actual);
  return JSON.stringify(actual) === JSON.stringify(expected) ? true : `got ${JSON.stringify(actual)}`;
};

// Refusals before anything is sent: no helper call at all.
for (const [name, bad, effectCode] of [
  ["account mismatch", call({ scope: { ...scope, account: "ca_other" } }), "account_mismatch"],
  ["destination not allowlisted", call({ scope: { ...scope, destination: "acme/private" } }, { repo: "private" }), "destination_not_allowed"],
  ["arguments name another repository", call({}, { repo: "private" }), "destination_mismatch"],
  ["unsupported precondition", call({ preconditions: [{ issue_state: "open" }] }), "unsupported_precondition"],
  ["extra argument", call({}, { assignees: ["x"] }), "unsupported_arguments"],
  ["two labels", call({}, { labels: ["triage", "bug"] }), "unsupported_labels"],
  ["empty labels", call({}, { labels: [] }), "unsupported_labels"],
  ["non-positive issue", call({}, { issue_number: 0 }), "invalid_issue"],
] as const) {
  await check(`refused before any call: ${name}`, async () => {
    const { host, ledger } = setup();
    const reply = await host.execute(bad, signal);
    if (ledger().calls.length !== 0) return `made ${ledger().calls.length} calls`;
    return expect(reply.ok ? "ok" : [reply.error.code, reply.effect], [effectCode, "refused"]);
  });
}

await check("a missing label is refused without a mutation", async () => {
  const { host, ledger } = setup();
  const reply = await host.execute(call({}, { labels: ["no-such-label"] }), signal);
  const mutations = ledger().calls.filter((item) => item.slug === ADD_LABELS).length;
  if (mutations !== 0) return `sent ${mutations} mutations`;
  return expect(reply.ok ? "ok" : [reply.error.code, reply.effect], ["label_missing", "refused"]);
});

await check("success adds the label once, as the approved account", async () => {
  const { host, ledger } = setup();
  const reply = await host.execute(call(), signal);
  const state = ledger();
  const accounts = [...new Set(state.calls.map((item) => item.account))];
  if (JSON.stringify(accounts) !== JSON.stringify([ACCOUNT])) return `accounts ${JSON.stringify(accounts)}`;
  if (JSON.stringify(state.repositories["acme/widgets"].issues["1"]) !== '["triage"]') return "label not applied";
  return expect(reply, { ok: true, value: { label_added: true } });
});

for (const [fault, code] of [["fail_after_apply", "github_status_500"], ["unobserved_after_apply", "composio_unobserved"]] as const) {
  await check(`a mutation failure after dispatch is unknown (${fault})`, async () => {
    const { host, ledger } = setup({ [`${ADD_LABELS} acme/widgets#1`]: fault });
    const reply = await host.execute(call(), signal);
    if (ledger().calls.filter((item) => item.slug === ADD_LABELS).length !== 1) return "not exactly one mutation: no retry";
    return expect(reply.ok ? "ok" : [reply.error.code, reply.effect], [code, "unknown"]);
  });
}

await check("authorization checks the scope, then the account's permission now", async () => {
  const { host, ledger } = setup();
  const ok = await host.authorize(scope);
  const wrong = await host.authorize({ ...scope, account: "ca_other" });
  const denied = await composioPlanHost(fakeLabels(join(Deno.args[0], "labels-0.json")), { account: ACCOUNT, destinations: ["acme/private"] })
    .authorize({ ...scope, destination: "acme/private" });
  if (ledger().calls.some((item) => item.account !== ACCOUNT)) return "a call used another account";
  return expect([ok.ok, wrong.ok ? "ok" : wrong.error.code, denied.ok ? "ok" : denied.error.code], [true, "account_mismatch", "not_permitted"]);
});

await check("present on the issue: satisfied", async () => {
  const { host } = setup();
  return expect(await host.reconcile(probe(2), signal), { verdict: "satisfied", evidence: "the label is on the issue; who added it is unknown" });
});

await check("absent: unknown, never not_applied", async () => {
  const { host } = setup();
  return expect(await host.reconcile(probe(1), signal), { verdict: "unknown", evidence: "the label is absent; an earlier request may still apply" });
});

await check("pagination: found on page 2", async () => {
  const { host, ledger } = setup();
  const reply = await host.reconcile(probe(3), signal);
  const reads = ledger().calls.length;
  return reads === 2 ? expect(reply, { verdict: "satisfied", evidence: "the label is on the issue; who added it is unknown" }) : `${reads} reads`;
});

await check("pagination: absent after every page", async () => {
  const { host } = setup();
  return expect(await host.reconcile(probe(4), signal), { verdict: "unknown", evidence: "the label is absent; an earlier request may still apply" });
});

for (const [fault, code] of [["fail", "state_read_failed"], ["malformed", "state_malformed"], ["unobserved", "state_read_failed"]] as const) {
  await check(`a ${fault} read is a probe error`, async () => {
    const { host } = setup({ "GITHUB_LIST_LABELS_FOR_AN_ISSUE acme/widgets#2": fault });
    const reply = await host.reconcile(probe(2), signal);
    return expect("error" in reply ? reply.error.code : reply, code);
  });
}

await check("no reply carries Composio or GitHub text", async () =>
  JSON.stringify(replies).includes("MARK-SECRET") ? "a reply carried MARK-SECRET" : true
);

console.log(JSON.stringify(results));
if (Object.values(results).some((verdict) => verdict !== "ok")) Deno.exit(1);
