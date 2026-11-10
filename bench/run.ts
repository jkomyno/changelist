/**
 * Benchmark driver: one set of fixtures and guest programs through several code-mode engines.
 *
 *   bun bench/run.ts --engines codemode,codemode-c4,pi,opencode \
 *     --scenarios triage,triage-large,denied,deadline,guest-exception,lost-response \
 *     --profiles zero,equal,mixed --reps 20 --warmup 3 --cold 8 --out bench/results/<label> \
 *     [--failure-reps N] [--include-holdout] [--memory] [--memory-samples N]
 *
 * Timing pass (default): warm runs in this process (engines round-robin per repetition with a
 * rotating start, after discarded warmups), then `--cold N` fresh-process samples per cell.
 * Memory pass (`--memory`): fresh process per sample, never mixed with timing runs.
 * See bench/README.md for what each number means.
 */
import { execFile, execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import { basename, join, resolve } from "node:path";
import { promisify } from "node:util";
import type { EngineInfo, EngineModule } from "./engines/types.ts";
import type { Profile } from "./fixtures/tools.ts";
import { idealMs } from "./model.ts";
import {
  BENCH,
  ENGINES,
  ROOT,
  type RunRecord,
  SCENARIOS,
  type Scenario,
  limitsFor,
  loadEngine,
  program,
  runOnce,
  stats,
} from "./harness.ts";

const execFileAsync = promisify(execFile);
const RUNTIME = (process.versions as Record<string, string>).bun ? "bun" : "node";
const RUNTIME_VERSION = RUNTIME === "bun" ? (process.versions as Record<string, string>).bun : process.versions.node;

// ---------------------------------------------------------------------------- CLI
const argv = process.argv.slice(2);
const flag = (name: string) => argv.includes(`--${name}`);
const option = (name: string, fallback: string) => {
  const index = argv.indexOf(`--${name}`);
  return index === -1 ? fallback : argv[index + 1];
};
const list = (name: string, fallback: string) => option(name, fallback).split(",").filter(Boolean);
const count = (name: string, fallback: number) => {
  const value = Number(option(name, String(fallback)));
  if (!Number.isInteger(value) || value < 0) throw new Error(`--${name} must be a non-negative integer`);
  return value;
};

const engines = list("engines", "codemode,codemode-c4,pi,opencode");
const includeHoldout = flag("include-holdout");
const scenarioNames = list("scenarios", SCENARIOS.filter((s) => !s.holdout).map((s) => s.name).join(","));
const profiles = list("profiles", "zero,equal,mixed") as Profile[];
const reps = count("reps", 20);
const failureReps = count("failure-reps", reps);
const warmup = count("warmup", 3);
const cold = count("cold", 8);
const memoryMode = flag("memory");
const memorySamples = count("memory-samples", 5);
const out = resolve(option("out", join(BENCH, "results", "local")));

for (const engine of engines) {
  if (!ENGINES[engine]) throw new Error(`unknown engine ${engine}; known: ${Object.keys(ENGINES).join(", ")}`);
  if (RUNTIME === "node" && !ENGINES[engine].node) throw new Error(`${engine} requires Bun`);
}
const scenarios: Scenario[] = scenarioNames.map((name) => {
  const s = SCENARIOS.find((candidate) => candidate.name === name);
  if (!s) throw new Error(`unknown scenario ${name}`);
  return s;
});
const skippedHoldouts = scenarios.filter((s) => s.holdout && !includeHoldout).map((s) => s.name);
const selected = scenarios.filter((s) => !s.holdout || includeHoldout);

interface Cell {
  scenario: Scenario;
  profile: Profile;
}
const cells: Cell[] = [];
const skippedCells: string[] = [];
for (const s of selected) {
  const usable = s.profiles.filter((p) => profiles.includes(p));
  if (usable.length === 0) skippedCells.push(`${s.name} (defines ${s.profiles.join("/")} only)`);
  for (const profile of usable) cells.push({ scenario: s, profile });
}

process.env.CODEMODE_BIN = resolve(process.env.CODEMODE_BIN ?? join(ROOT, "target/release/codemode"));
mkdirSync(out, { recursive: true });

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const rotate = <T>(items: T[], by: number) => items.map((_, i) => items[(i + by) % items.length]);
const log = (message: string) => process.stderr.write(`bench: ${message}\n`);
const isFailureScenario = (s: Scenario) => s.expect.kind !== "success";

// ---------------------------------------------------------------------------- environment
function tryRun(command: string, args: string[]): string | null {
  try {
    return execFileSync(command, args, { cwd: ROOT, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
  } catch {
    return null;
  }
}

function environment() {
  const porcelain = tryRun("git", ["status", "--porcelain", "--untracked-files=all"]) ?? "";
  const changed = porcelain.split("\n").filter(Boolean).map((line) => line.slice(3));
  const outsideBench = changed.filter((path) => !path.startsWith("bench/"));
  const bin = process.env.CODEMODE_BIN!;
  let binSha256: string | null = null;
  let binBytes: number | null = null;
  try {
    const artifact = readFileSync(bin);
    binSha256 = createHash("sha256").update(artifact).digest("hex");
    binBytes = artifact.byteLength;
  } catch {
    // not built
  }
  return {
    date: new Date().toISOString(),
    driver: { runtime: RUNTIME, version: RUNTIME_VERSION, execPath: process.execPath },
    runtimes: {
      bun: tryRun("bun", ["--version"]),
      node: tryRun("node", ["--version"]),
    },
    cpu: { model: os.cpus()[0]?.model, logical: os.cpus().length },
    os: {
      platform: os.platform(),
      release: os.release(),
      arch: os.arch(),
      product: os.platform() === "darwin" ? tryRun("sw_vers", ["-productVersion"]) : null,
      totalMemGiB: +(os.totalmem() / 2 ** 30).toFixed(1),
    },
    loadavg: os.loadavg().map((v) => +v.toFixed(2)),
    git: {
      commit: tryRun("git", ["rev-parse", "HEAD"]),
      dirty: changed.length > 0,
      dirtyOutsideBench: outsideBench.length > 0,
      changedOutsideBench: outsideBench.slice(0, 50),
    },
    codemodeBinary: {
      path: bin,
      sha256: binSha256,
      bytes: binBytes,
      // Build declarations supplied by the operator, not inferred from the current toolchain.
      buildRustc: process.env.CODEMODE_BUILD_RUSTC ?? null,
      buildFeatures: process.env.CODEMODE_BUILD_FEATURES ?? null,
    },
    toolchainRustc: tryRun("mise", ["exec", "--", "rustc", "--version", "--verbose"]),
    pins: JSON.parse(readFileSync(join(BENCH, "engines/pins.json"), "utf8")),
  };
}

// ---------------------------------------------------------------------------- child processes
interface ChildResult {
  wallMs: number;
  code: number | null;
  json?: any;
  stderr: string;
  pid: number;
}

function spawnChild(args: string[], onSpawn?: (pid: number) => void): Promise<ChildResult> {
  return new Promise((resolvePromise) => {
    const started = performance.now();
    const child = spawn(process.execPath, args, { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"], env: process.env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr = (stderr + chunk).slice(-4000)));
    if (child.pid) onSpawn?.(child.pid);
    child.on("close", (code) => {
      const wallMs = performance.now() - started;
      const last = stdout.trim().split("\n").pop();
      let json: unknown;
      try {
        json = last ? JSON.parse(last) : undefined;
      } catch {
        json = undefined;
      }
      resolvePromise({ wallMs, code, json, stderr, pid: child.pid ?? -1 });
    });
  });
}

const childArgs = (mode: string, engine?: string, cell?: Cell) => [
  join(BENCH, "child.ts"),
  "--mode",
  mode,
  ...(engine && cell ? ["--engine", engine, "--scenario", cell.scenario.name, "--profile", cell.profile] : []),
];

// ---------------------------------------------------------------------------- warm pass
const keyOf = (scenarioName: string, profile: string, engine: string) => `${scenarioName}|${profile}|${engine}`;

function metrics(record: RunRecord, phase: string, rep: number, position: number) {
  const { raw, ...rest } = record;
  return { phase, rep, position, ...rest };
}

async function warmPass(modules: Map<string, EngineModule>) {
  const runsPath = join(out, "runs.jsonl");
  writeFileSync(runsPath, "");
  const results = new Map<string, Array<ReturnType<typeof metrics>>>();
  for (const cell of cells) {
    const code = program(cell.scenario);
    const cellReps = isFailureScenario(cell.scenario) ? failureReps : reps;
    const rounds = warmup + cellReps;
    const buffered = new Map<string, Array<{ m: ReturnType<typeof metrics>; record: RunRecord }>>();
    log(`warm ${cell.scenario.name}/${cell.profile}: ${warmup} warmup + ${cellReps} measured rounds x ${engines.length} engines`);
    for (let round = 0; round < rounds; round++) {
      const measured = round >= warmup;
      const order = rotate(engines, round % engines.length);
      for (let position = 0; position < order.length; position++) {
        const engine = order[position];
        const record = await runOnce(engine, modules.get(engine)!, cell.scenario, cell.profile, code);
        if (!measured) continue;
        const m = metrics(record, "warm", round - warmup, position);
        const key = keyOf(cell.scenario.name, cell.profile, engine);
        buffered.set(key, [...(buffered.get(key) ?? []), { m, record }]);
      }
    }
    // Write metrics for every run; attach the full raw record to the median run (by task
    // latency) and to the first run with a correctness problem or reconciliation violation.
    for (const [key, entries] of buffered) {
      const sorted = [...entries].sort((a, b) => a.m.taskMs - b.m.taskMs);
      const median = sorted[Math.max(0, Math.ceil(sorted.length / 2) - 1)];
      const anomalous = entries.find((e) => !e.m.correct || e.m.reconciliation.violations.length > 0);
      for (const entry of entries) {
        const reasons = [entry === median ? "median" : null, entry === anomalous ? "first_anomaly" : null].filter(Boolean);
        const line = reasons.length > 0 ? { ...entry.m, rawReason: reasons, raw: entry.record.raw } : entry.m;
        appendFileSync(runsPath, `${JSON.stringify(line)}\n`);
      }
      results.set(key, entries.map((e) => e.m));
    }
  }
  return results;
}

// ---------------------------------------------------------------------------- cold pass
async function coldPass() {
  const coldPath = join(out, "cold.jsonl");
  writeFileSync(coldPath, "");
  const baseline = { bare: [] as number[], noop: [] as number[] };
  log(`cold baselines: ${cold} bare ${RUNTIME} processes and ${cold} fixture-only processes`);
  for (let i = 0; i < cold; i++) {
    const bare = await spawnChild(["-e", ""]);
    baseline.bare.push(bare.wallMs);
    const noop = await spawnChild(childArgs("noop"));
    baseline.noop.push(noop.wallMs);
    appendFileSync(coldPath, `${JSON.stringify({ kind: "baseline", sample: i, bareWallMs: bare.wallMs, noopWallMs: noop.wallMs })}\n`);
  }
  const samples = new Map<string, any[]>();
  for (const cell of cells) {
    log(`cold ${cell.scenario.name}/${cell.profile}: ${cold} fresh processes x ${engines.length} engines`);
    for (let i = 0; i < cold; i++) {
      for (const engine of rotate(engines, i % engines.length)) {
        const child = await spawnChild(childArgs("cold", engine, cell));
        const sample = {
          kind: "sample",
          engine,
          scenario: cell.scenario.name,
          profile: cell.profile,
          sample: i,
          processWallMs: child.wallMs,
          exitCode: child.code,
          ...(child.json ?? { error: "no JSON from child", stderr: child.stderr.slice(-800) }),
        };
        appendFileSync(coldPath, `${JSON.stringify(sample)}\n`);
        const key = keyOf(cell.scenario.name, cell.profile, engine);
        samples.set(key, [...(samples.get(key) ?? []), sample]);
      }
    }
  }
  return { baseline: { bareWallMs: stats(baseline.bare), noopWallMs: stats(baseline.noop) }, samples };
}

// ---------------------------------------------------------------------------- memory pass
interface Tree {
  selfKiB: number;
  bridgeKiB: number;
  workerKiB: number;
  otherKiB: number;
  descendantsKiB: number;
  treeKiB: number;
}

async function sampleTree(root: number, otherNames: Set<string>): Promise<Tree | null> {
  let text: string;
  try {
    text = (await execFileAsync("ps", ["-A", "-o", "pid=,ppid=,rss=,comm="])).stdout;
  } catch {
    return null;
  }
  const rows = text
    .split("\n")
    .map((line) => line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/))
    .filter((m): m is RegExpMatchArray => m !== null)
    .map((m) => ({ pid: +m[1], ppid: +m[2], rss: +m[3], comm: m[4].trim().replace(/^\((.*)\)$/, "$1") })); // ps shows an exiting process as "(name)"
  const self = rows.find((row) => row.pid === root);
  if (!self) return null;
  const isCodemode = (comm: string) => ["codemode", basename(process.env.CODEMODE_BIN!)].includes(basename(comm));
  const bridges = new Set(rows.filter((row) => row.ppid === root && isCodemode(row.comm)).map((row) => row.pid));
  const tree: Tree = { selfKiB: self.rss, bridgeKiB: 0, workerKiB: 0, otherKiB: 0, descendantsKiB: 0, treeKiB: 0 };
  const frontier = [root];
  while (frontier.length > 0) {
    const parent = frontier.pop()!;
    for (const row of rows) {
      if (row.ppid !== parent) continue;
      frontier.push(row.pid);
      tree.descendantsKiB += row.rss;
      if (bridges.has(row.pid)) tree.bridgeKiB += row.rss;
      else if (bridges.has(row.ppid) && isCodemode(row.comm)) tree.workerKiB += row.rss;
      else {
        tree.otherKiB += row.rss;
        otherNames.add(row.comm);
      }
    }
  }
  tree.treeKiB = tree.selfKiB + tree.descendantsKiB;
  return tree;
}

/**
 * Run one child while sampling its process tree with `ps`. A new `ps` starts every 10 ms
 * without waiting for the previous one (one `ps -A` takes 15-40 ms here), so snapshots
 * are about 10 ms apart; peaks shorter than that can still be missed.
 */
async function sampledChild(args: string[]) {
  let pid = -1;
  const peaks: Tree = { selfKiB: 0, bridgeKiB: 0, workerKiB: 0, otherKiB: 0, descendantsKiB: 0, treeKiB: 0 };
  const otherNames = new Set<string>();
  const pending = new Set<Promise<void>>();
  const launches: number[] = [];
  let samples = 0;
  const timer = setInterval(() => {
    if (pid === -1) return;
    launches.push(performance.now());
    const sample: Promise<void> = sampleTree(pid, otherNames).then((tree) => {
      pending.delete(sample);
      if (!tree) return;
      samples++;
      for (const k of Object.keys(peaks) as Array<keyof Tree>) peaks[k] = Math.max(peaks[k], tree[k]);
    });
    pending.add(sample);
  }, 10);
  const child = await spawnChild(args, (spawned) => (pid = spawned));
  clearInterval(timer);
  await Promise.all(pending);
  const intervals = launches.slice(1).map((t, i) => t - launches[i]);
  return { child, peaks, samples, intervalMs: stats(intervals), otherNames: [...otherNames] };
}

async function memoryPass() {
  const memoryPath = join(out, "memory.jsonl");
  writeFileSync(memoryPath, "");
  const baselines: number[] = [];
  const psBaselines: number[] = [];
  log(`memory baseline: ${memorySamples} fixture-only processes`);
  for (let i = 0; i < memorySamples; i++) {
    // Held for 300 ms so the ps sampler sees it.
    const { child, peaks, samples } = await sampledChild([...childArgs("noop"), "--hold", "300"]);
    baselines.push(child.json?.maxRssKiB);
    psBaselines.push(peaks.selfKiB);
    appendFileSync(memoryPath, `${JSON.stringify({ kind: "baseline", sample: i, ...child.json, psPeakSelfKiB: peaks.selfKiB, psSamples: samples })}\n`);
  }
  const baseline = stats(baselines);
  const psBaseline = stats(psBaselines);
  const results = new Map<string, any[]>();
  // One profile per scenario, preferring "mixed": its runs last longest, so the ps sampler
  // sees the codemode child processes many times. Memory does not otherwise depend on latency.
  const memoryCells: Cell[] = [];
  for (const s of selected) {
    const preferred = (["mixed", "equal", "zero"] as Profile[]).filter((p) => s.profiles.includes(p) && profiles.includes(p));
    const profile = preferred[0];
    if (profile) memoryCells.push({ scenario: s, profile });
  }
  for (const cell of memoryCells) {
    log(`memory ${cell.scenario.name}/${cell.profile}: ${memorySamples} fresh processes x ${engines.length} engines`);
    for (let i = 0; i < memorySamples; i++) {
      for (const engine of rotate(engines, i % engines.length)) {
        const { child, peaks, samples, intervalMs, otherNames } = await sampledChild(childArgs("memory", engine, cell));
        const sample = {
          kind: "sample",
          engine,
          scenario: cell.scenario.name,
          profile: cell.profile,
          sample: i,
          ok: child.json?.ok,
          correct: child.json?.correct,
          maxRssKiB: child.json?.maxRssKiB,
          maxRssOverBaselineKiB: child.json?.maxRssKiB !== undefined && baseline ? child.json.maxRssKiB - baseline.p50 : null,
          psPeakSelfKiB: peaks.selfKiB,
          psPeakSelfOverBaselineKiB: psBaseline ? peaks.selfKiB - psBaseline.p50 : null,
          psPeakBridgeKiB: peaks.bridgeKiB,
          psPeakWorkerKiB: peaks.workerKiB,
          psPeakOtherChildrenKiB: peaks.otherKiB,
          psPeakDescendantsKiB: peaks.descendantsKiB,
          psPeakTreeKiB: peaks.treeKiB,
          psSamples: samples,
          psIntervalMs: intervalMs,
          psOtherChildNames: otherNames,
        };
        appendFileSync(memoryPath, `${JSON.stringify(sample)}\n`);
        const key = keyOf(cell.scenario.name, cell.profile, engine);
        results.set(key, [...(results.get(key) ?? []), sample]);
      }
    }
  }
  const summary: Record<string, unknown> = { baselineMaxRssKiB: baseline, baselinePsPeakSelfKiB: psBaseline, cells: {} };
  const lines = [
    "# Memory pass",
    "",
    `Driver: ${RUNTIME} ${RUNTIME_VERSION}. ${memorySamples} fresh processes per engine and scenario (the slowest selected latency profile, so the sampler sees each run longer); values are medians (min-max) in MiB.`,
    "",
    `- **Driver peak RSS over baseline**: \`process.resourceUsage().maxRSS\` of the sample process minus the median of ${memorySamples} fixture-only processes (${baseline ? (baseline.p50 / 1024).toFixed(1) : "?"} MiB). Includes everything in that process: the engine, Pi's worker threads and QuickJS heaps, OpenCode's interpreter, and for codemode only the host client.`,
    `- **Driver ps peak over baseline**: the same process's RSS as sampled by \`ps\` from the parent, minus the median ps peak of the fixture-only processes (${psBaseline ? (psBaseline.p50 / 1024).toFixed(1) : "?"} MiB). Under Bun on macOS, \`maxRSS\` reads 8-23 MiB lower than \`ps\` RSS for the same process (checked by hand; Node agrees with ps), so both are shown.`,
    "- **Bridge / worker peak**: codemode controller directly under the driver and the embedded codemode worker directly under a controller, distinguished by process ancestry. Pi and OpenCode spawn no processes.",
    "- **Whole tree peak**: maximum simultaneous driver plus all descendant RSS in one ps snapshot. Children total peak likewise sums descendants in each snapshot; neither is a sum of independent process peaks.",
    "- ps sampling starts every 10 ms; short-lived processes and peaks between snapshots can be missed (see observed interval). Fresh-process memory samples include the 100 ms post-run observation.",
    "- RSS counts shared pages (e.g. the codemode and Bun binaries) in full; it is not a measure of unique memory.",
    "",
    "| Scenario | Engine | Succeeded/N | Driver maxRSS over baseline | Driver ps peak over baseline | Bridge peak | Worker peak | Children total peak | Whole tree peak | ps interval p50 | ps samples p50 |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];
  const mib = (s: ReturnType<typeof stats>) => (s ? `${(s.p50 / 1024).toFixed(1)} (${(s.min / 1024).toFixed(1)}-${(s.max / 1024).toFixed(1)})` : "-");
  for (const cell of memoryCells) {
    for (const engine of engines) {
      const rows = results.get(keyOf(cell.scenario.name, cell.profile, engine)) ?? [];
      const over = stats(rows.map((r) => r.maxRssOverBaselineKiB));
      const psOver = stats(rows.map((r) => r.psPeakSelfOverBaselineKiB));
      const sampleCount = stats(rows.map((r) => r.psSamples));
      const bridge = stats(rows.map((r) => r.psPeakBridgeKiB));
      const worker = stats(rows.map((r) => r.psPeakWorkerKiB));
      const children = stats(rows.map((r) => r.psPeakDescendantsKiB));
      const tree = stats(rows.map((r) => r.psPeakTreeKiB));
      const interval = stats(rows.map((r) => r.psIntervalMs?.p50));
      const codemode = engine.startsWith("codemode");
      (summary.cells as Record<string, unknown>)[keyOf(cell.scenario.name, cell.profile, engine)] = {
        driverMaxRssOverBaselineKiB: over,
        driverPsPeakOverBaselineKiB: psOver,
        psSamples: sampleCount,
        bridgePeakKiB: codemode ? bridge : null,
        workerPeakKiB: codemode ? worker : null,
        descendantsPeakKiB: children,
        treePeakKiB: tree,
        psIntervalMs: interval,
        okCount: rows.filter((r) => r.ok).length,
        n: rows.length,
      };
      lines.push(
        `| ${cell.scenario.name}/${cell.profile} | ${engine} | ${rows.filter((r) => r.ok).length}/${rows.length} | ${mib(over)} | ${mib(psOver)} | ${codemode ? mib(bridge) : "n/a"} | ${codemode ? mib(worker) : "n/a"} | ${mib(children)} | ${mib(tree)} | ${interval ? interval.p50.toFixed(0) : "-"} ms | ${sampleCount?.p50 ?? "-"} |`,
      );
    }
  }
  writeFileSync(join(out, "memory.json"), JSON.stringify(summary, null, 2));
  writeFileSync(join(out, "memory.md"), lines.join("\n") + "\n");
  log(`memory results in ${out}/memory.md`);
}

// ---------------------------------------------------------------------------- summary
function tally(values: string[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const v of values) out[v] = (out[v] ?? 0) + 1;
  return out;
}

function summarize(
  env: ReturnType<typeof environment>,
  infos: Record<string, EngineInfo>,
  warm: Map<string, any[]>,
  coldResult: Awaited<ReturnType<typeof coldPass>> | null,
) {
  const cellsOut: Record<string, unknown> = {};
  for (const cell of cells) {
    for (const engine of engines) {
      const key = keyOf(cell.scenario.name, cell.profile, engine);
      const runs = warm.get(key) ?? [];
      const coldSamples = coldResult?.samples.get(key) ?? [];
      const sum = (pick: (r: any) => number) => stats(runs.map(pick));
      cellsOut[key] = {
        scenario: cell.scenario.name,
        profile: cell.profile,
        engine,
        holdout: cell.scenario.holdout,
        limits: limitsFor(cell.scenario, engine),
        modelMs: idealMs(cell.scenario, cell.profile, ENGINES[engine].effectiveConcurrency ?? limitsFor(cell.scenario, engine).concurrency),
        n: runs.length,
        correct: runs.filter((r) => r.correct).length,
        outcomes: tally(runs.map((r) => (r.ok ? "success" : `${r.errorKind}${r.engineKind ? ` (${r.engineKind})` : ""}`))),
        correctnessProblems: tally(runs.flatMap((r) => r.correctness)),
        knownLimit: cell.scenario.known_limits?.[engine.replace(/-c4$/, "")] ?? null,
        warm: {
          taskMs: sum((r) => r.taskMs),
          setupMs: sum((r) => r.setupMs),
          runMs: sum((r) => r.runMs),
          engineElapsedMs: engine.startsWith("codemode") ? sum((r) => r.engineElapsedMs) : null,
        },
        cold: coldSamples.length
          ? {
              n: coldSamples.length,
              correct: coldSamples.filter((s) => s.correct).length,
              totalMs: stats(coldSamples.map((s) => s.totalMs)),
              importMs: stats(coldSamples.map((s) => s.importMs)),
              processWallMs: stats(coldSamples.map((s) => s.processWallMs)),
            }
          : null,
        ledger: {
          started: stats(runs.map((r) => r.ledger.started)),
          completed: stats(runs.map((r) => r.ledger.completed)),
          thrown: stats(runs.map((r) => r.ledger.thrown)),
          aborted: stats(runs.map((r) => r.ledger.aborted)),
          effectApplied: stats(runs.map((r) => r.ledger.effectApplied)),
          effectAppliedWithoutComplete: stats(runs.map((r) => r.ledger.effectAppliedWithoutComplete)),
          maxInFlight: stats(runs.map((r) => r.ledger.maxInFlight)),
          runsWithLateStarts: runs.filter((r) => r.ledger.startedAfterReturn > 0).length,
          runsWithToolStillRunningAfterReturn: runs.filter((r) => {
            const late = r.ledger.stillRunningAfterReturn;
            return late.completedLate + late.threwLate + late.open > 0;
          }).length,
          stillRunningAfterReturnTotals: {
            completedLate: runs.reduce((s, r) => s + r.ledger.stillRunningAfterReturn.completedLate, 0),
            threwLate: runs.reduce((s, r) => s + r.ledger.stillRunningAfterReturn.threwLate, 0),
            abortedLate: runs.reduce((s, r) => s + r.ledger.stillRunningAfterReturn.abortedLate, 0),
            openAtSettle: runs.reduce((s, r) => s + r.ledger.stillRunningAfterReturn.open, 0),
          },
        },
        reconciliation: {
          method: runs[0]?.reconciliation.method,
          runsWithViolations: runs.filter((r) => r.reconciliation.violations.length > 0).length,
          violations: tally(runs.flatMap((r) => r.reconciliation.violations)),
          notes: tally(runs.flatMap((r) => r.reconciliation.notes)),
          counts: tally(runs.flatMap((r) => Object.entries(r.reconciliation.counts).map(([k, v]) => `${k}=${v}`))),
        },
        bytes: {
          payload: stats(runs.map((r) => r.bytes.payload)),
          ledgerResults: stats(runs.map((r) => r.bytes.ledgerResults)),
          returned: stats(runs.map((r) => r.bytes.returned)),
          report: engine.startsWith("codemode") ? stats(runs.map((r) => r.bytes.report)) : null,
          engineBoundary: null,
        },
      };
    }
  }
  const summary = {
    label: out,
    environment: env,
    config: {
      engines,
      scenarios: selected.map((s) => s.name),
      skippedHoldouts,
      skippedCells,
      profiles,
      reps,
      failureReps,
      warmup,
      cold,
      ordering: "engines round-robin within each repetition; the starting engine rotates every repetition",
      percentiles: "nearest-rank",
    },
    engines: Object.fromEntries(engines.map((e) => [e, { ...infos[e], appliedOverrides: ENGINES[e].overrides }])),
    coldBaseline: coldResult?.baseline ?? null,
    cells: cellsOut,
  };
  writeFileSync(join(out, "summary.json"), JSON.stringify(summary, null, 2));
  writeFileSync(join(out, "summary.md"), markdown(summary));
}

const ms = (s: any) => (s ? `${s.p50.toFixed(1)} / ${s.p95.toFixed(1)}` : "-");
const range = (s: any) => (s ? `${s.min.toFixed(1)}-${s.max.toFixed(1)}` : "-");
const kib = (s: any) => (s ? (s.p50 / 1024).toFixed(1) : "-");

function markdown(summary: any): string {
  const env = summary.environment;
  const lines: string[] = [];
  lines.push(`# Code-mode engine benchmark: ${summary.label.split("/").pop()}`);
  lines.push("");
  lines.push(
    `Driver ${env.driver.runtime} ${env.driver.version}; bun ${env.runtimes.bun}, node ${env.runtimes.node}. ` +
      `${env.cpu.model} (${env.cpu.logical} logical CPUs), ${env.os.platform} ${env.os.product ?? env.os.release} ${env.os.arch}, ${env.os.totalMemGiB} GiB. ` +
      `Commit ${env.git.commit?.slice(0, 12)}${env.git.dirtyOutsideBench ? " (dirty outside bench/)" : env.git.dirty ? " (bench/ uncommitted)" : ""}. Load average at start ${env.loadavg.join(", ")}.`,
  );
  lines.push("");
  const c = summary.config;
  lines.push(
    `Warm: ${c.warmup} discarded + ${c.reps} measured repetitions per cell (${c.failureReps} for failure scenarios), engines round-robin with a rotating start, in one ${env.driver.runtime} process. ` +
      `Cold: ${c.cold} fresh ${env.driver.runtime} processes per cell. Percentiles are nearest-rank. Times in ms, shown as p50 / p95. ` +
      "*Model*: ideal zero-overhead makespan from the fixture latencies at the concurrency the engine applies (bench/model.ts); a model, not a measurement.",
  );
  if (c.skippedHoldouts.length) lines.push(`\nHoldout scenarios skipped (pass --include-holdout): ${c.skippedHoldouts.join(", ")}.`);
  if (c.skippedCells.length) lines.push(`\nScenarios with no selected profile: ${c.skippedCells.join("; ")}.`);
  lines.push("");
  lines.push("## Limits as applied");
  lines.push("");
  lines.push("| Engine | Version | timeoutMs | maxCalls | concurrency | maxOutputBytes |");
  lines.push("| --- | --- | --- | --- | --- | --- |");
  for (const [name, info] of Object.entries(summary.engines) as Array<[string, any]>) {
    lines.push(`| ${name} | ${info.version} | ${info.limits.timeoutMs} | ${info.limits.maxCalls} | ${info.limits.concurrency} | ${info.limits.maxOutputBytes} |`);
  }
  if (summary.coldBaseline) {
    lines.push("");
    lines.push(
      `Process baselines (${c.cold} samples, wall ms p50 / p95): bare \`${env.driver.runtime} -e ""\` ${ms(summary.coldBaseline.bareWallMs)}; ` +
        `fixture-only child (imports fixtures and harness, builds datasets, exits) ${ms(summary.coldBaseline.noopWallMs)}.`,
    );
  }
  const scenarioNames = [...new Set(Object.values(summary.cells).map((cell: any) => cell.scenario))];
  for (const name of scenarioNames) {
    const cellsFor = Object.values(summary.cells).filter((cell: any) => cell.scenario === name) as any[];
    const holdout = cellsFor[0].holdout;
    lines.push("");
    lines.push(`## ${name}${holdout ? " (holdout: correctness only, timing not reported)" : ""}`);
    lines.push("");
    if (holdout) {
      lines.push("| Profile | Engine | Correct | Outcome | Calls started | Max in flight |");
      lines.push("| --- | --- | ---: | --- | ---: | ---: |");
      for (const cell of cellsFor) {
        lines.push(
          `| ${cell.profile} | ${cell.engine} | ${cell.correct}/${cell.n} | ${Object.entries(cell.outcomes).map(([k, v]) => `${k} x${v}`).join(", ")} | ${cell.ledger.started?.p50 ?? "-"} | ${cell.ledger.maxInFlight?.max ?? "-"} |`,
        );
      }
      continue;
    }
    lines.push(
      "| Profile | Engine | Correct | Outcome | Model | Warm task | min-max | Setup p50 | Run p50 | Rust elapsed | Cold total | Cold process wall | Calls started | Max in flight | Payload KiB | Returned B | Report KiB |",
    );
    lines.push("| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for (const cell of cellsFor) {
      const w = cell.warm;
      lines.push(
        `| ${cell.profile} | ${cell.engine} | ${cell.correct}/${cell.n}${cell.cold ? ` (cold ${cell.cold.correct}/${cell.cold.n})` : ""} | ${Object.entries(cell.outcomes).map(([k, v]) => `${k} x${v}`).join(", ")} | ${cell.modelMs ?? "-"} | ${ms(w.taskMs)} | ${range(w.taskMs)} | ${w.setupMs?.p50.toFixed(2) ?? "-"} | ${w.runMs?.p50.toFixed(1) ?? "-"} | ${w.engineElapsedMs ? ms(w.engineElapsedMs) : "n/a"} | ${cell.cold ? ms(cell.cold.totalMs) : "-"} | ${cell.cold ? ms(cell.cold.processWallMs) : "-"} | ${cell.ledger.started ? `${cell.ledger.started.p50}${cell.ledger.started.min !== cell.ledger.started.max ? ` (${cell.ledger.started.min}-${cell.ledger.started.max})` : ""}` : "-"} | ${cell.ledger.maxInFlight?.max ?? "-"} | ${kib(cell.bytes.payload)} | ${cell.bytes.returned?.p50 ?? "-"} | ${cell.bytes.report ? kib(cell.bytes.report) : "n/a"} |`,
      );
    }
    const knownLimits = cellsFor.filter((cell) => cell.knownLimit && cell.correct < cell.n);
    for (const cell of knownLimits.slice(0, 1)) lines.push(`\nKnown limitation (${cell.engine}): ${cell.knownLimit}`);
  }
  lines.push("");
  lines.push("## Correctness and ledger findings");
  lines.push("");
  lines.push(
    "Ledger columns count runs. *Late starts*: a tool started more than 50 ms after the run returned. *Tool running after return*: a tool started before the run returned and completed, threw, or was still open afterwards (aborts after return are counted separately; codemode's host client aborts outstanding calls as it reads the report).",
  );
  lines.push("");
  lines.push("| Scenario/profile | Engine | Correctness problems | Late starts | Tool running after return | Aborted after return (ops) | Effect applied w/o complete (median) | Reconciliation violations | Engine-vs-ledger notes |");
  lines.push("| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |");
  for (const cell of Object.values(summary.cells) as any[]) {
    const problems = Object.entries(cell.correctnessProblems).map(([k, v]) => `${k} (x${v})`).join("; ") || "none";
    const violations = Object.entries(cell.reconciliation.violations).map(([k, v]) => `${k} (x${v})`).join("; ") || "none";
    const notes = Object.entries(cell.reconciliation.notes).map(([k, v]) => `${k} (x${v})`).join("; ") || "-";
    lines.push(
      `| ${cell.scenario}/${cell.profile} | ${cell.engine} | ${problems} | ${cell.ledger.runsWithLateStarts} | ${cell.ledger.runsWithToolStillRunningAfterReturn} | ${cell.ledger.stillRunningAfterReturnTotals.abortedLate} | ${cell.ledger.effectAppliedWithoutComplete?.p50 ?? "-"} | ${violations} | ${notes} |`,
    );
  }
  lines.push("");
  lines.push("## Engine-reported call outcomes (all measured runs)");
  lines.push("");
  lines.push("| Scenario/profile | Engine | Outcome tallies (`status=count` per run, xN runs) |");
  lines.push("| --- | --- | --- |");
  for (const cell of Object.values(summary.cells) as any[]) {
    const counts = Object.entries(cell.reconciliation.counts).map(([k, v]) => `${k} x${v}`).join("; ");
    lines.push(`| ${cell.scenario}/${cell.profile} | ${cell.engine} | ${counts || "-"} |`);
  }
  lines.push("");
  return lines.join("\n");
}

// ---------------------------------------------------------------------------- main
const env = environment();
writeFileSync(join(out, "environment.json"), JSON.stringify(env, null, 2));
log(`driver ${RUNTIME} ${RUNTIME_VERSION}; engines ${engines.join(", ")}; ${cells.length} cells; out ${out}`);

if (memoryMode) {
  await memoryPass();
} else {
  const modules = new Map<string, EngineModule>();
  const infos: Record<string, EngineInfo> = {};
  for (const engine of engines) {
    const mod = await loadEngine(engine);
    modules.set(engine, mod);
    infos[engine] = mod.info;
  }
  const started = performance.now();
  const warm = await warmPass(modules);
  log(`warm pass took ${((performance.now() - started) / 1000).toFixed(0)} s`);
  const coldStarted = performance.now();
  const coldResult = cold > 0 ? await coldPass() : null;
  if (coldResult) log(`cold pass took ${((performance.now() - coldStarted) / 1000).toFixed(0)} s`);
  summarize(env, infos, warm, coldResult);
  log(`results in ${out}/summary.md`);
}
process.exit(0);
