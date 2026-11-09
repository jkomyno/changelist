// Invoked by install-check.sh with no source checkout or remote dependencies.
import { runBridge } from "../integrations/shared/bridge-client.ts";
import { codemode } from "../integrations/opencode/codemode-bridge.ts";

const [installation, backend, deno] = Deno.args;
if (backend !== "deno" && backend !== "embedded-v8") throw new Error("unknown backend");
const bin = `${installation}/bin/codemode`;
const fixture = `${installation}/bin/codemode-fixture`;
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
  bin, backend, deno, cwd, timeoutMs: 10_000, concurrency: 1, maxCalls: 1,
  code: 'return await tools.call("installed.echo", {message: "outside-checkout"});',
  tools: ["installed.echo"],
  async execute(call) { return { ok: true, value: { tool: call.tool, arguments: call.arguments } }; },
});
assert(report.status === "success" && report.calls.length === 1, "installed shared adapter failed");
assert(JSON.stringify(report.output) === JSON.stringify({
  tool: "installed.echo", arguments: { message: "outside-checkout" },
}), "installed host returned an unexpected value");
await Deno.writeTextFile("trace.json", JSON.stringify(report));
await Deno.writeTextFile("trace-inspection.txt", await command(bin, ["inspect", "trace.json"]));

// Exercise the actual delivered OpenCode adapter with an offline permission context.
// The separate pinned-host CI lane tests OpenCode's real permission service.
const permissions: string[] = [];
const plugin = await codemode({
  bin, backend, deno, trace: "opencode-trace.json",
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

// Native plans use the installed fixture and must work without a guest engine.
await command(fixture, ["init", "--db", "fixture.db", "--repo", "acme/widgets", "--account", "triage-bot", "--issues", "101-101"]);
await Deno.writeTextFile("provider.json", await command(fixture, ["declare", "--db", "fixture.db", "--account", "triage-bot"]));
await Deno.writeTextFile("plan.json", JSON.stringify({
  version: 1, summary: "Installed offline label change", operations: [{
    id: "label-101", tool: "issues.add_label",
    scope: { provider: "fixture.issues", account: "triage-bot", destination: "acme/widgets" },
    arguments: { issue: 101, label: "triage" },
  }],
}));
const checked = JSON.parse(await command(bin, ["plan", "check", "plan.json", "--provider", "provider.json", "--json"]));
assert(typeof checked.digest === "string", "plan check omitted digest");
await command(bin, ["plan", "approve", "plan.json", "--journal", "journal.db", "--provider", "provider.json", "--digest", checked.digest, "--approver", "install-smoke"]);
for (const action of ["run", "resume"]) {
  await Deno.writeTextFile(`${action}.json`, await command(bin, ["plan", action, "--journal", "journal.db", "--fixture", "fixture.db", "--account", "triage-bot", "--deno", "/nonexistent-install-smoke-engine"]));
}
await Deno.writeTextFile("plan-inspection.txt", await command(bin, ["plan", "inspect", "--journal", "journal.db"]));
const ledger = JSON.parse(await command(fixture, ["ledger", "--db", "fixture.db", "--json"]));
assert(Array.isArray(ledger.rows) && ledger.rows.length === 1 && ledger.rows[0].effect === true, "resume repeated or lost the fixture effect");
await Deno.writeTextFile("ledger.json", JSON.stringify(ledger));
await Deno.writeTextFile("native-trace.json", await command(bin, ["plan", "trace", "--journal", "journal.db", "--session", "1"]));
await command(bin, ["inspect", "native-trace.json"]);
console.log(JSON.stringify({ status: "success", backend, installed: installation, work: cwd }));
