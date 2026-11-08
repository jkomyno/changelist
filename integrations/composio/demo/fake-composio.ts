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

/**
 * The two identity reads finish only after both have started, so a successful
 * workflow proves they overlapped regardless of timing. A read that waits 5 s for
 * its partner fails, which is what sequential execution would produce.
 */
const IDENTITY_READS = ["GITHUB_GET_THE_AUTHENTICATED_USER", "SLACK_TEST_AUTH"];
const arrived = new Set<string>();
let bothArrived: () => void = () => {};
const rendezvous = new Promise<void>((resolve) => (bothArrived = resolve));

async function meetPartner(slug: string): Promise<void> {
  arrived.add(slug);
  if (IDENTITY_READS.every((read) => arrived.has(read))) bothArrived();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      const result = { successful: false, error: `${slug} ran without its partner identity read`, slug };
      reject(failure(result.error, { result, slug }));
    }, 5_000);
  });
  try {
    await Promise.race([rendezvous, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

export async function fakeExecute(slug: string, data: Record<string, unknown> = {}): Promise<ComposioResult> {
  switch (slug) {
    case "GITHUB_GET_THE_AUTHENTICATED_USER":
      await meetPartner(slug);
      return { successful: true, data: { login: "codemode-demo", public_repos: 3 }, error: null };
    case "SLACK_TEST_AUTH":
      await meetPartner(slug);
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
