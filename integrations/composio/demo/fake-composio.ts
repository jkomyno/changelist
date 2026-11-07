/**
 * Offline stand-in for the `execute` helper of `composio run` (Composio CLI 0.6.0).
 * It reproduces that helper's observed contract with synthetic values: a resolved
 * `{ successful: true, data, error: null }`, an Error carrying an unsuccessful
 * `result`, or an Error carrying an `exitCode` when the CLI produced no result.
 * No network, account, or credential is used.
 */
import type { ComposioResult } from "../composio-provider.ts";

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function failure(message: string, detail: Record<string, unknown>): Error {
  return Object.assign(new Error(message), detail);
}

export async function fakeExecute(slug: string, data: Record<string, unknown> = {}): Promise<ComposioResult> {
  switch (slug) {
    case "GITHUB_GET_THE_AUTHENTICATED_USER":
      await sleep(120);
      return { successful: true, data: { login: "codemode-demo", public_repos: 3 }, error: null };
    case "SLACK_TEST_AUTH":
      await sleep(40);
      return { successful: true, data: { ok: true, team: "codemode-demo" }, error: null };
    case "GITHUB_GET_A_REPOSITORY":
      await sleep(10);
      if (typeof data.owner !== "string" || typeof data.repo !== "string") {
        // The CLI validates input locally and prints an unsuccessful result.
        const result = { successful: false, error: "Invalid input: owner and repo are required", slug };
        throw failure(result.error, { result, slug });
      }
      return { successful: true, data: { full_name: `${data.owner}/${data.repo}` }, error: null };
    case "GMAIL_GET_PROFILE": {
      const result = { successful: false, error: "No active connection for gmail", slug: "ToolRouterV2_NoActiveConnection" };
      throw failure(result.error, { result, slug });
    }
    case "CODEMODE_DEMO_SLOW": {
      // Synthetic slow tool for cancellation checks; the helper cannot be aborted.
      // Tell a test that the call is in flight.
      process.stderr.write(`started ${slug}\n`);
      await sleep(10_000);
      return { successful: true, data: { finished: true }, error: null };
    }
    case "CODEMODE_DEMO_CRASH":
      // Synthetic CLI crash after dispatch: no result, so the outcome is unknown.
      throw failure(`composio execute ${slug} returned no JSON output`, { exitCode: 1 });
    default:
      throw failure(`composio execute ${slug} failed with exit code 1`, { exitCode: 1, stderr: "unknown tool" });
  }
}
