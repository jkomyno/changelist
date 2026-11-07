// Two read-only identity checks in parallel, a controlled invalid-argument failure,
// and a slug the host did not authorize. Returns shapes, not account details.
const [github, slack] = (await Promise.all([
  tools.call("GITHUB_GET_THE_AUTHENTICATED_USER", {}),
  tools.call("SLACK_TEST_AUTH", {}),
])) as [Record<string, unknown>, Record<string, unknown>];

const refused: string[] = [];
for (const [slug, args] of [
  ["GITHUB_GET_A_REPOSITORY", {}],
  ["TELEGRAM_GET_ME", {}],
] as const) {
  try {
    await tools.call(slug, args);
    refused.push("none");
  } catch (error) {
    refused.push((error as { code: string }).code);
  }
}

let environment = "readable";
try {
  Deno.env.get("COMPOSIO_API_KEY");
} catch {
  environment = "denied";
}

return {
  github: Object.keys(github).length > 0,
  slack: Object.keys(slack).length > 0,
  refused,
  environment,
};
