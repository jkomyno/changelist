/**
 * The Composio GitHub host: investigate a repository with a program, then
 * review, approve, run, and recover the change list it proposed. Run it under
 * the Composio CLI (verified with 0.6.0), which injects an authenticated
 * `execute` helper and keeps its credential in this process:
 *
 *     composio run --file integrations/composio/host.ts -- <command> \
 *       --account <ca_...> --repo <owner/repo> [options]
 *
 * An installed bundle runs it as `bin/codemode-composio <command> ...`.
 *
 * Commands:
 *   investigate --program <file> --out <plan.json> [--trace <file>]
 *               Run a read-only program; write its proposal as a plan and check it.
 *   declare     Print the provider declaration to approve plans with.
 *   preflight   --plan <plan.json>  Read-only: check each operation against GitHub now.
 *   run|resume  --declaration <file> --journal <file> [--deadline-ms <n>]
 *               Serve one session of an approved plan.
 *
 * Every command takes --account (a connected-account id, never an alias),
 * --repo (the only repository it reads or writes), and --bin <codemode>.
 * stdout receives one compact JSON line. Exit status: 0 success or completed,
 * 1 the program or session ended without success, 2 usage or startup error,
 * 3 work remains or preflight is not ready, 4 refused.
 *
 * `composio run` wraps everything after the imports in a function, so this file
 * has no exports and ends with a statement.
 */
import { describeError } from "../shared/bridge-client.ts";
import { finish, listConnections, runHost } from "./composio-host.ts";
import type { AccountExecute } from "./github-plan.ts";

const execute = (globalThis as { execute?: AccountExecute }).execute;
try {
  await runHost(process.argv.slice(2), execute ? { execute, connections: listConnections } : undefined);
} catch (error) {
  finish(process.stderr, `composio host: ${describeError(error)}`, 2);
}
