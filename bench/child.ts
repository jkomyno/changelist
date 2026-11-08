/**
 * One fresh-process sample, spawned by run.ts. Prints one JSON line on stdout.
 *
 *   child.ts --mode cold   --engine E --scenario S --profile P
 *   child.ts --mode memory --engine E --scenario S --profile P
 *   child.ts --mode noop [--hold MS]  (fixtures and datasets loaded, no engine: the baseline)
 *
 * Fixtures, datasets, and the program are loaded before the clock starts. The cold clock
 * starts immediately before the engine module's dynamic import and stops when runEngine
 * returns, so it includes import, engine setup, and the run.
 */
import { datasetA, datasetB } from "./fixtures/datasets.ts";
import { loadEngine, program, runOnce, scenario } from "./harness.ts";

const args = new Map<string, string>();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1]);
const mode = args.get("mode") ?? "noop";

// Build the datasets up front in every mode, so baselines hold the same fixture state.
datasetA();
datasetB();

const memory = () => ({ maxRssKiB: process.resourceUsage().maxRSS, rssKiB: Math.round(process.memoryUsage().rss / 1024) });

if (mode === "noop") {
  // --hold keeps the process alive so the parent's ps sampler can observe it.
  const hold = Number(args.get("hold") ?? 0);
  if (hold > 0) await new Promise((resolve) => setTimeout(resolve, hold));
  process.stdout.write(`${JSON.stringify({ mode, ...memory() })}\n`);
  process.exit(0);
}

const engine = args.get("engine")!;
const s = scenario(args.get("scenario")!);
const profile = args.get("profile") as "zero" | "equal" | "mixed";
const code = program(s);

const importStart = performance.now();
const mod = await loadEngine(engine);
const importMs = performance.now() - importStart;
let runStart = 0;
const record = await runOnce(engine, mod, s, profile, code, { onStart: () => (runStart = performance.now()) });
process.stdout.write(
  `${JSON.stringify({
    mode,
    engine,
    scenario: s.name,
    profile,
    importMs,
    setupMs: record.setupMs,
    runMs: record.runMs,
    taskMs: record.taskMs,
    // From before the dynamic import to runEngine's return.
    totalMs: runStart - importStart + record.taskMs,
    ok: record.ok,
    errorKind: record.errorKind,
    correct: record.correct,
    correctness: record.correctness,
    ledger: record.ledger,
    violations: record.reconciliation.violations.length,
    ...memory(),
  })}\n`,
);
process.exit(0);
