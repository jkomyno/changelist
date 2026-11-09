/**
 * A short demonstration on the benchmark fixtures, offline and without accounts:
 *
 * 1. the triage workflow under mixed latency, and why it took as long as it did;
 * 2. a permission denial, and which calls ran;
 * 3. a simulated effect whose response is lost, and what needs reconciliation.
 *
 * Each step saves the execution record and prints `codemode inspect` on it.
 *
 *   mise run demo            # or: bun bench/demo.ts [--dir DIR]
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Profile } from "./fixtures/tools.ts";
import { loadEngine, program, ROOT, runOnce, scenario } from "./harness.ts";

const index = process.argv.indexOf("--dir");
const dir = index === -1 ? mkdtempSync(join(tmpdir(), "codemode-demo-")) : process.argv[index + 1];
mkdirSync(dir, { recursive: true });
const bin = process.env.CODEMODE_BIN ?? join(ROOT, "target/release/codemode");

async function step(title: string, engine: string, name: string, profile: Profile, inspect: string[]) {
  const mod = await loadEngine(engine);
  const s = scenario(name);
  const record = await runOnce(engine, mod, s, profile, program(s));
  const trace = join(dir, `${name}-${engine}.json`);
  writeFileSync(trace, JSON.stringify(record.raw.engineTrace, null, 2) + "\n");
  const ledger = record.ledger;
  console.log(`\n## ${title}\n`);
  console.log(
    `${engine}, ${name} with ${profile} latency: ${record.ok ? "ok" : record.errorKind}, ` +
      `${record.correct ? "correct" : "INCORRECT"}, task ${record.taskMs.toFixed(1)} ms`,
  );
  if (record.ok) console.log(`output: ${JSON.stringify(record.raw.output)}`);
  console.log(
    `independent ledger: ${ledger.started} started, ${ledger.completed} completed, ` +
      `${ledger.effectApplied} effects applied, ${ledger.effectAppliedWithoutComplete} without a completed response`,
  );
  console.log(`$ codemode inspect ${trace} ${inspect.join(" ")}`);
  console.log(execFileSync(bin, ["inspect", trace, ...inspect], { encoding: "utf8" }).trimEnd());
}

await step("1. A composed workflow", "codemode", "triage", "mixed", ["--why-slow", "--limit", "3"]);
await step("2. A permission denial", "codemode", "denied", "equal", ["--calls", "--limit", "4"]);
await step("3. A response lost after its effect", "codemode", "lost-response", "mixed", ["--reconcile"]);
console.log(`\nRecords are in ${dir}`);
process.exit(0);
