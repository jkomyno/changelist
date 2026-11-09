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
 * Options: --bin <codemode> (codemode on PATH).
 *
 * The config is host-owned: `{"account": "<connected account id>", "destinations": ["owner/repo"]}`.
 * stdout receives one compact JSON line. Exit status: 0 completed, 3 work remains,
 * 4 refused, 2 usage or startup error, 1 the session ended without an outcome.
 *
 * `composio run` wraps everything after the imports in a function, so this file
 * has no exports and ends with a statement.
 */
import { finish, listConnections, runPlanHost } from "./composio-plan-host.ts";
import type { AccountExecute } from "./composio-plan.ts";

const execute = (globalThis as { execute?: AccountExecute }).execute;
try {
  await runPlanHost(process.argv.slice(2), execute ? { execute, connections: listConnections } : undefined);
} catch (error) {
  finish(process.stderr, `composio plan host: ${error instanceof Error ? error.message : String(error)}`, 2);
}
