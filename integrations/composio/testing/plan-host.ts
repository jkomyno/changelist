/**
 * Test-only entry: the production plan host (`../composio-plan-host.ts`) with
 * two additions for recovery checks. Accepts the same commands and options as
 * `plan-host.ts`, plus:
 *
 *   --offline <state.json>   Use the offline stand-in (`fake-labels.ts`) and its
 *                            ledger instead of the `composio run` helper.
 *   --drop-reply <operation> Fault injection: after the helper reports a successful
 *                            mutation for that operation, never reply, so the
 *                            session cannot record it.
 *
 *     deno run --allow-run --allow-read --allow-write --allow-env \
 *       integrations/composio/testing/plan-host.ts run --offline state.json ...
 *
 * Without --offline it must run under `composio run`. Not installed.
 */
import { resolve } from "node:path";
import type { PlanReply } from "../../shared/bridge-client.ts";
import { type DecorateHost, finish, listConnections, type PlanHostProviders, runPlanHost } from "../composio-plan-host.ts";
import type { AccountExecute } from "../composio-plan.ts";
import { fakeConnections, fakeLabels } from "./fake-labels.ts";

/** Remove this entry's own options from `argv`; the production host refuses unknown ones. */
function extract(argv: string[], names: string[]): { rest: string[]; values: Map<string, string> } {
  const rest: string[] = [];
  const values = new Map<string, string>();
  for (let index = 0; index < argv.length; index++) {
    const name = argv[index].startsWith("--") ? argv[index].slice(2) : "";
    if (!names.includes(name)) {
      rest.push(argv[index]);
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) throw new Error(`--${name} needs a value`);
    if (values.has(name)) throw new Error(`--${name} can only be supplied once`);
    values.set(name, value);
    index++;
  }
  return { rest, values };
}

async function main(): Promise<void> {
  const { rest, values } = extract(process.argv.slice(2), ["offline", "drop-reply"]);
  const offline = values.get("offline");
  const execute = (globalThis as { execute?: AccountExecute }).execute;
  let providers: PlanHostProviders | undefined;
  if (offline) {
    const path = resolve(process.cwd(), offline);
    providers = { execute: fakeLabels(path), connections: fakeConnections(path) };
  } else if (execute) {
    providers = { execute, connections: listConnections };
  }
  const drop = values.get("drop-reply");
  const decorate: DecorateHost = (host) => ({
    ...host,
    async execute(call, signal) {
      const reply = await host.execute(call, signal);
      if (drop === call.operation && reply.ok) {
        // The success was observed here and is lost on purpose.
        process.stderr.write(`composio plan host: dropped the reply for ${call.operation} after an observed success\n`);
        return new Promise<PlanReply>(() => {});
      }
      return reply;
    },
  });
  await runPlanHost(rest, providers, decorate);
}

try {
  await main();
} catch (error) {
  finish(process.stderr, `composio plan host: ${error instanceof Error ? error.message : String(error)}`, 2);
}
