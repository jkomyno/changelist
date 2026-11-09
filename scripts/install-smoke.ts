// Invoked by install-check.sh with no source checkout or remote dependencies.
import { type ProviderDeclaration, runBridge, runPlanBridge } from "../integrations/shared/bridge-client.ts";
import { codemode } from "../integrations/opencode/codemode-bridge.ts";

const [installation] = Deno.args;
const bin = `${installation}/bin/codemode`;
const cwd = Deno.cwd();
function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}
async function command(executable: string, args: string[]): Promise<string> {
  const result = await new Deno.Command(executable, { args, stdout: "piped", stderr: "piped" }).output();
  assert(result.success, `${executable} ${args.join(" ")}: ${new TextDecoder().decode(result.stderr)}`);
  return new TextDecoder().decode(result.stdout);
}

// A real trusted host uses the delivered shared adapter to execute an offline tool.
const report = await runBridge({
  bin, cwd, timeoutMs: 10_000, concurrency: 1, maxCalls: 1,
  code: 'return await tools.call("installed.echo", {message: "outside-checkout"});',
  tools: ["installed.echo"],
  async execute(call) { return { ok: true, value: { tool: call.tool, arguments: call.arguments } }; },
});
assert(report.status === "success" && report.calls.length === 1, "installed shared adapter failed");
const output = report.output;
assert(typeof output === "object" && output !== null && "tool" in output && output.tool === "installed.echo"
  && "arguments" in output, "installed host returned an unexpected value");
const args = output.arguments;
assert(typeof args === "object" && args !== null && "message" in args && args.message === "outside-checkout",
  "installed host returned unexpected arguments");
await Deno.writeTextFile("trace.json", JSON.stringify(report));
await Deno.writeTextFile("trace-inspection.txt", await command(bin, ["inspect", "trace.json"]));

// Exercise the actual delivered OpenCode adapter with an offline permission context.
// The separate pinned-host CI lane tests OpenCode's real permission service.
const permissions: string[] = [];
const plugin = await codemode({
  bin, trace: "opencode-trace.json",
  tools: { echo: { description: "Offline installed tool", args: {}, async execute() { return "installed-opencode"; } } },
})({ directory: cwd });
const pluginResult = await plugin.tool.rust_codemode.execute({ code: 'return await tools.call("echo", {});' }, {
  sessionID: "install-smoke", messageID: "install-smoke", agent: "offline",
  directory: cwd, worktree: cwd, abort: new AbortController().signal,
  metadata() {},
  async ask(input: { permission: string }) { permissions.push(input.permission); },
});
assert(typeof pluginResult !== "string", "installed adapter omitted its result metadata");
assert(JSON.parse(pluginResult.output).output === "installed-opencode", "installed OpenCode adapter failed");
assert(permissions.length === 1 && permissions[0] === "codemode_echo", "adapter skipped its permission request");
await command(bin, ["inspect", "opencode-trace.json"]);

// Native plans run through the installed plan bridge, with this script as the host
// and provider, and must work without a guest engine.
const scope = { provider: "smoke.labels", account: "triage-bot", destination: "acme/widgets" };
const declaration: ProviderDeclaration = {
  provider: scope.provider, account: scope.account,
  contracts: { "labels.add": { repeat: "safe", reconcile: "state" } },
};
await Deno.writeTextFile("provider.json", JSON.stringify(declaration));
await Deno.writeTextFile("plan.json", JSON.stringify({
  version: 1, summary: "Installed offline label change", operations: [{
    id: "label-101", tool: "labels.add", scope, arguments: { issue: 101, label: "triage" },
  }],
}));
const checked = JSON.parse(await command(bin, ["plan", "check", "plan.json", "--provider", "provider.json", "--json"]));
assert(typeof checked.digest === "string", "plan check omitted digest");
await command(bin, ["plan", "approve", "plan.json", "--journal", "journal.db", "--provider", "provider.json", "--digest", checked.digest, "--approver", "install-smoke"]);
// The host's own record of applied effects: every attempt it executed, by attempt key.
const labels = new Set<string>();
const effects: string[] = [];
for (const kind of ["run", "resume"] as const) {
  const { outcome } = await runPlanBridge({
    bin, cwd, journal: `${cwd}/journal.db`, kind, declaration,
    async authorize(requested) {
      return requested.provider === scope.provider && requested.account === scope.account
        ? { ok: true }
        : { ok: false, error: { code: "unauthorized", message: "not this account" } };
    },
    async execute(call) {
      const key = JSON.stringify(call.arguments);
      labels.add(key);
      effects.push(call.attempt);
      return { ok: true, value: { applied: key } };
    },
    async reconcile(request) {
      return { verdict: labels.has(JSON.stringify(request.arguments)) ? "satisfied" : "not_applied", evidence: "labels" };
    },
  });
  assert(outcome.status === "completed", `plan ${kind} did not complete: ${JSON.stringify(outcome)}`);
  await Deno.writeTextFile(`${kind}.json`, JSON.stringify(outcome));
}
assert(effects.length === 1, `resume repeated or lost the effect: ${effects.length} attempts`);
await Deno.writeTextFile("plan-inspection.txt", await command(bin, ["plan", "inspect", "--journal", "journal.db"]));
await Deno.writeTextFile("native-trace.json", await command(bin, ["plan", "trace", "--journal", "journal.db", "--session", "1"]));
await command(bin, ["inspect", "native-trace.json"]);
// The Composio launcher runs the bundled host with the bundled binary through
// `composio run`, under a private TMPDIR it removes afterwards. A stub stands in
// for the Composio CLI and records what it was given.
await Deno.mkdir("stub-bin");
await Deno.writeTextFile("stub-bin/composio",
  `#!/bin/sh\n{ printf '%s\\n' "$TMPDIR"; ls -ld "$TMPDIR" | cut -c1-10; printf '%s\\n' "$@"; } > '${cwd}/launcher.txt'\n`,
  { mode: 0o755 });
const launched = await new Deno.Command(`${installation}/bin/codemode-composio`, {
  args: ["declare", "--account", "ca_smoke", "--repo", "acme/widgets"],
  env: { PATH: `${cwd}/stub-bin:${Deno.env.get("PATH") ?? ""}` },
}).output();
assert(launched.success, "the Composio launcher failed");
const [privateDir, mode, ...forwarded] = (await Deno.readTextFile("launcher.txt")).trimEnd().split("\n");
assert(mode === "drwx------", `the launcher's TMPDIR is not private: ${mode}`);
assert(JSON.stringify(forwarded) === JSON.stringify(["run", "--file", `${installation}/integrations/composio/host.ts`, "--",
  "declare", "--account", "ca_smoke", "--repo", "acme/widgets", "--bin", bin]), `the launcher ran: ${forwarded.join(" ")}`);
let removed = false;
try {
  await Deno.stat(privateDir);
} catch {
  removed = true;
}
assert(removed, "the launcher left its TMPDIR behind");
console.log(JSON.stringify({ status: "success", installed: installation, work: cwd }));
