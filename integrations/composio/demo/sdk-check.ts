/**
 * Offline check of `sdkExecutor` against a stand-in for `@composio/core`'s client:
 * request shape, abort-signal forwarding, result mapping, and an unobserved request
 * error that cancels the run. Prints one JSON line.
 * Usage: deno run --allow-run --allow-read --allow-env sdk-check.ts <codemode> <deno>
 */
import { type ComposioSdk, runComposio, sdkExecutor } from "../composio-provider.ts";

const requests: unknown[] = [];
const sdk: ComposioSdk = {
  tools: {
    async execute(slug, body, options) {
      requests.push({ slug, body, signal: options?.signal instanceof AbortSignal });
      if (slug === "SLACK_TEST_AUTH") return { successful: false, data: {}, error: "token revoked" };
      if (slug === "GITHUB_GET_A_REPOSITORY") {
        // Raised before execution, so the failure is definite.
        throw Object.assign(new Error("owner is required"), { name: "ValidationError" });
      }
      return { successful: true, data: { login: "codemode-demo" }, error: null };
    },
  },
};

const { report } = await runComposio({
  bin: Deno.args[0],
  deno: Deno.args[1],
  cwd: Deno.cwd(),
  code: `const out = [];
for (const [slug, args] of [["GITHUB_GET_THE_AUTHENTICATED_USER", {}], ["SLACK_TEST_AUTH", {}], ["GITHUB_GET_A_REPOSITORY", {}], ["GITHUB_GET_THE_AUTHENTICATED_USER", [1]]]) {
  try { out.push(await tools.call(slug, args)); } catch (e) { out.push(e.code); }
}
return out;`,
  tools: ["GITHUB_GET_THE_AUTHENTICATED_USER", "SLACK_TEST_AUTH", "GITHUB_GET_A_REPOSITORY"],
  timeoutMs: 10_000,
  concurrency: 2,
  maxCalls: 8,
  executor: sdkExecutor(sdk, { userId: "user-1", version: "20261104_00" }),
});
const crashed = await runComposio({
  bin: Deno.args[0],
  deno: Deno.args[1],
  cwd: Deno.cwd(),
  code: `return await tools.call("GITHUB_GET_THE_AUTHENTICATED_USER", {});`,
  tools: ["GITHUB_GET_THE_AUTHENTICATED_USER"],
  timeoutMs: 10_000,
  concurrency: 1,
  maxCalls: 1,
  executor: sdkExecutor(
    { tools: { execute: () => Promise.reject(new Error("socket hang up")) } },
    { userId: "user-1" },
  ),
});
console.log(JSON.stringify({
  status: report.status,
  output: report.output,
  requests,
  unobserved: {
    status: crashed.report.status,
    outcomes: crashed.report.calls.map((call) => (call as { outcome: { status: string } }).outcome.status),
    diagnostic: crashed.hostDiagnostic,
  },
}));
