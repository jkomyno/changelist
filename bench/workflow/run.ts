// Workflow comparison driver. See README.md for the protocol it implements.
//
//   node bench/workflow/run.ts [--out DIR] [--scenarios a,b] [--engines a,b] [--reps N]
//                              [--summarize-only]
//
// Per run: start a fresh provider process, run the implementation's adapter as its own process
// group, kill it at the scenario's fault point, apply the scenario's control steps, rerun it per
// the recovery policy, then score the ledger with oracle.ts and append one JSON line to
// runs.jsonl. Per-run ledgers and logs stay under .cache/runs/.
import { execFile, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { cpus, totalmem } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { parseArgs } from "node:util";
import type { Claims } from "./lib/adapter.ts";
import { ROOT, WORKFLOW_DIR } from "./lib/adapter.ts";
import { normalized, readLedger, stateFromLedger, WRITE_SLUGS } from "./lib/github-state.ts";
import { compareLatency, type Distribution, distribution, type Expectation, score, type Verdict } from "./oracle.ts";

interface Scenario {
  id: string;
  kind?: "latency";
  workload?: "triage" | "cpu-loop" | "memory-growth";
  rows?: string[];
  row_overrides?: Record<string, Record<string, number>>;
  faults?: unknown[];
  faults_every_attempt?: boolean;
  kill_on_armed?: boolean;
  probe_properties?: boolean;
  after_kill?: Array<Record<string, unknown>>;
  release_after_kill?: boolean;
  release_after_attempt?: number;
  wait_for_late_effect?: boolean;
  recover_once?: boolean;
  max_recoveries: number;
  widen?: boolean;
  limit?: boolean;
  reps?: number;
  expect: Expectation;
  fault_op?: string;
  unsupported?: Record<string, string>;
}

interface Config {
  protocol: {
    latency_reps: number;
    latency_warmup: number;
    fault_reps: number;
    provider_latency_ms: number;
    wall_ms: number;
    limit_wall_ms: number;
    load_max: number;
    load_wait_ms: number;
    rss_sample_ms: number;
  };
  engines: string[];
  rows: Record<string, Record<string, number>>;
  scenarios: Scenario[];
  capabilities: Record<string, Record<string, string>>;
  harness_checks: Array<{ id: string; scenario: string; engine: string; extra_args: string[]; expect_flag: string }>;
}

interface Attempt {
  n: number;
  exit: number | null;
  signal: string | null;
  killed_by: "fault" | "wall" | null;
  ms: number;
  spawn_ts: number;
  end_ts: number;
}

interface RunRecord {
  scenario: string;
  row: string;
  engine: string;
  rep: number;
  timed: boolean;
  status: "ran" | "unsupported";
  reason?: string;
  workload?: string;
  attempts?: Attempt[];
  recovery_requests?: number;
  total_ms?: number;
  startup_ms?: number | null;
  peak_rss_mb_sampled?: number;
  durable_records?: number | null;
  claims_final?: Claims | null;
  claims_all?: Claims[];
  oracle?: Verdict;
  limit_outcome?: string;
  fault_op_claims?: string[];
  properties?: Record<string, unknown>;
  ledger?: { requests: number; writes_sent: number; effects: number; late_effects: number };
  load?: number[];
  capability?: string;
  adapter_errors?: string[];
}

const { values: cli } = parseArgs({
  options: {
    out: { type: "string" },
    scenarios: { type: "string" },
    engines: { type: "string" },
    reps: { type: "string" },
    "summarize-only": { type: "boolean", default: false },
    help: { type: "boolean", default: false },
  },
});
if (cli.help) {
  process.stdout.write(readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(0, 10).join("\n") + "\n");
  process.exit(0);
}

const config: Config = JSON.parse(readFileSync(join(WORKFLOW_DIR, "scenarios.json"), "utf8"));
// The local calendar date names the results directory.
const today = new Date().toLocaleDateString("sv-SE");
const out = cli.out ?? join(WORKFLOW_DIR, "results", today);
const cacheRuns = join(WORKFLOW_DIR, ".cache", "runs", out.split("/").filter(Boolean).slice(-1)[0]);
const BIN = process.env.CODEMODE_BIN ?? join(ROOT, "target", "release", "codemode");
const selectedScenarios = cli.scenarios?.split(",");
const selectedEngines = cli.engines?.split(",") ?? config.engines;
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const log = (line: string) => process.stderr.write(`${new Date().toISOString().slice(11, 19)} ${line}\n`);

// ---- environment ------------------------------------------------------------------------------------

function sh(cmd: string, args: string[], cwd = ROOT): string {
  const result = spawnSync(cmd, args, { cwd, encoding: "utf8" });
  return (result.stdout ?? "").trim();
}

function loadAverage(): number[] {
  const match = /load averages?: ([\d.]+),? ([\d.]+),? ([\d.]+)/.exec(sh("uptime", []));
  return match ? match.slice(1, 4).map(Number) : [];
}

function duKiB(path: string): number {
  return Number(sh("du", ["-sk", path]).split(/\s+/)[0]);
}

function environment(): unknown {
  const pins = JSON.parse(readFileSync(join(WORKFLOW_DIR, "pins.json"), "utf8"));
  const sha = (path: string) => createHash("sha256").update(readFileSync(path)).digest("hex");
  const pkgs = join(WORKFLOW_DIR, ".cache", "pkgs");
  const packageCount = (name: string) =>
    Object.keys(JSON.parse(readFileSync(join(pkgs, name, "package-lock.json"), "utf8")).packages).filter((k) => k !== "").length;
  return {
    date: new Date().toISOString(),
    machine: {
      model: sh("sysctl", ["-n", "hw.model"]),
      cpu: cpus()[0]?.model,
      cpus: cpus().length,
      memory_gib: Math.round(totalmem() / 2 ** 30),
      os: `${sh("sw_vers", ["-productName"])} ${sh("sw_vers", ["-productVersion"])} (${sh("uname", ["-sr"])})`,
      load_average_at_start: loadAverage(),
    },
    node: process.version,
    codemode: {
      commit: sh("git", ["rev-parse", "HEAD"]),
      dirty: sh("git", ["status", "--porcelain", "--", "src", "Cargo.toml", "Cargo.lock"]) !== "",
      binary: BIN,
      binary_sha256: sha(BIN),
      binary_bytes: statSync(BIN).size,
      rustc: sh("rustc", ["--version"]),
    },
    readme_sha256: sha(join(WORKFLOW_DIR, "README.md")),
    scenarios_sha256: sha(join(WORKFLOW_DIR, "scenarios.json")),
    pins,
    install_effort: {
      codemode: {
        commands: ["mise install", "cargo build --release --locked --bin codemode"],
        binary_mib: Math.round((statSync(BIN).size / 2 ** 20) * 10) / 10,
        note: "builds from source; the build downloads a prebuilt V8 archive (about 36 MiB) and the locked crates",
      },
      "pi-durable": {
        commands: ["npm ci --ignore-scripts (lock/pi-durable)", "write a host program (engines/pi-durable.ts)"],
        packages: packageCount("pi-durable"),
        installed_mib: Math.round(duKiB(join(pkgs, "pi-durable", "node_modules")) / 102.4) / 10,
      },
      opencode: {
        commands: ["npm ci --ignore-scripts (lock/opencode)", "write a runner (engines/opencode.ts)"],
        packages: packageCount("opencode"),
        installed_mib: Math.round(duKiB(join(pkgs, "opencode", "node_modules")) / 102.4) / 10,
      },
      "composio-script": {
        commands: ["none beyond Node and the Composio CLI; the script is engines/composio-script.ts"],
        packages: 0,
        installed_mib: 0,
      },
    },
  };
}

// ---- provider ---------------------------------------------------------------------------------------

interface Provider {
  url: string;
  ledger: string;
  armed: Array<Record<string, unknown>>;
  onArmed?: (detail: Record<string, unknown>) => void;
  control(body: Record<string, unknown>): Promise<unknown>;
  stop(): void;
}

async function startProvider(dir: string): Promise<Provider> {
  const ledger = join(dir, "ledger.jsonl");
  const child = spawn(process.execPath, [join(WORKFLOW_DIR, "provider.ts"), "--ledger", ledger, "--latency-ms", String(config.protocol.provider_latency_ms)], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const provider: Provider = {
    url: "",
    ledger,
    armed: [],
    control: async (body) => (await fetch(`${provider.url}/control`, { method: "POST", body: JSON.stringify(body) })).json(),
    stop: () => {
      child.kill("SIGTERM");
      if (stderr.trim()) writeFileSync(join(dir, "provider.stderr"), stderr);
    },
  };
  await new Promise<void>((resolve, reject) => {
    let buffer = "";
    child.stdout.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      let newline: number;
      while ((newline = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        if (line.startsWith("@@READY ")) {
          provider.url = `http://127.0.0.1:${JSON.parse(line.slice(8)).port}`;
          resolve();
        } else if (line.startsWith("@@ARMED ")) {
          const detail = JSON.parse(line.slice(8));
          provider.armed.push(detail);
          provider.onArmed?.(detail);
        }
      }
    });
    child.on("exit", (code) => reject(new Error(`provider exited ${code}: ${stderr}`)));
  });
  return provider;
}

// ---- one attempt ------------------------------------------------------------------------------------

function sampleGroupRss(pgid: number): Promise<number> {
  return new Promise((resolve) => {
    execFile("ps", ["-A", "-o", "pgid=,rss="], (error, stdout) => {
      if (error) return resolve(0);
      let kib = 0;
      for (const line of stdout.split("\n")) {
        const [group, rss] = line.trim().split(/\s+/).map(Number);
        if (group === pgid) kib += rss;
      }
      resolve(kib / 1024);
    });
  });
}

interface AttemptPlan {
  engine: string;
  dir: string;
  n: number;
  provider: Provider;
  options: Record<string, number>;
  workload: string;
  widen: boolean;
  extraArgs: string[];
  wallMs: number;
  killOnArmed: boolean;
  /** Runs while the fault holds the engine, before the kill. */
  beforeKill?: () => Promise<void>;
}

async function runAttempt(plan: AttemptPlan, rss: { peak: number }): Promise<Attempt> {
  const nodeFlags: string[] = [];
  if (plan.engine === "opencode" && plan.options.heap_mb) nodeFlags.push(`--max-old-space-size=${plan.options.heap_mb}`);
  const args = [
    ...nodeFlags,
    join(WORKFLOW_DIR, "engines", `${plan.engine}.ts`),
    "--provider", plan.provider.url,
    "--run-dir", plan.dir,
    "--attempt", String(plan.n),
    "--workload", plan.workload,
    ...(plan.options.deadline_ms ? ["--deadline-ms", String(plan.options.deadline_ms)] : []),
    ...(plan.options.heap_mb && plan.engine !== "opencode" && plan.engine !== "composio-script" ? ["--heap-mb", String(plan.options.heap_mb)] : []),
    ...(plan.options.max_calls ? ["--max-calls", String(plan.options.max_calls)] : []),
    ...(plan.options.concurrency ? ["--concurrency", String(plan.options.concurrency)] : []),
    ...(plan.widen ? ["--widen"] : []),
    ...plan.extraArgs,
  ];
  const spawn_ts = Date.now();
  const started = performance.now();
  const child = spawn(process.execPath, args, { cwd: plan.dir, detached: true, stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, CODEMODE_BIN: BIN } });
  const pgid = child.pid!;
  let output = "";
  child.stdout.on("data", (chunk) => (output += chunk));
  child.stderr.on("data", (chunk) => (output += chunk));
  let killedBy: Attempt["killed_by"] = null;
  const kill = (why: "fault" | "wall") => {
    if (killedBy) return;
    killedBy = why;
    try {
      process.kill(-pgid, "SIGKILL");
    } catch {
      // Already gone.
    }
  };
  const sampler = setInterval(() => {
    void sampleGroupRss(pgid).then((mb) => (rss.peak = Math.max(rss.peak, mb)));
  }, config.protocol.rss_sample_ms);
  const wall = setTimeout(() => kill("wall"), plan.wallMs);
  plan.provider.onArmed = plan.killOnArmed
    ? () => {
        void (async () => {
          await plan.beforeKill?.();
          kill("fault");
        })();
      }
    : undefined;
  const [exit, signal] = await new Promise<[number | null, string | null]>((resolve) => child.on("exit", (code, sig) => resolve([code, sig])));
  const ms = performance.now() - started;
  clearInterval(sampler);
  clearTimeout(wall);
  plan.provider.onArmed = undefined;
  // Nothing of the group may outlive the attempt (a worker the engine started, say).
  try {
    process.kill(-pgid, "SIGKILL");
  } catch {
    // Already gone.
  }
  writeFileSync(join(plan.dir, `attempt-${plan.n}.log`), output);
  return { n: plan.n, exit, signal, killed_by: killedBy, ms: Math.round(ms * 10) / 10, spawn_ts, end_ts: Date.now() };
}

// ---- properties: inspection from another process, and diagnosis after an interrupt ------------------

function codemodeInspect(dir: string): { exit: number; view?: any } {
  const result = spawnSync(BIN, ["plan", "inspect", "--journal", join(dir, "codemode", "journal.db"), "--json"], { encoding: "utf8" });
  if (result.status !== 0) return { exit: result.status ?? -1 };
  return { exit: 0, view: JSON.parse(result.stdout) };
}

function sqliteRows(path: string): { tables: number; rows: number } | { error: string } {
  try {
    const db = new DatabaseSync(path, { readOnly: true });
    try {
      const tables = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all() as Array<{ name: string }>;
      const rows = tables.reduce((total, t) => total + (db.prepare(`SELECT count(*) AS n FROM "${t.name}"`).get() as { n: number }).n, 0);
      return { tables: tables.length, rows };
    } finally {
      db.close();
    }
  } catch (error) {
    return { error: String((error as Error).message).slice(0, 200) };
  }
}

function scriptProgress(dir: string): { decided: number; status: Record<string, string> } | undefined {
  const path = join(dir, "progress.jsonl");
  if (!existsSync(path)) return undefined;
  const entries = readFileSync(path, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  const decided = entries.find((e) => e.type === "decided");
  const status: Record<string, string> = {};
  for (const op of decided?.ops ?? []) status[op.id] = "not_attempted";
  for (const e of entries) {
    if (e.type === "started") status[e.op] = "unknown";
    if (e.type === "done") status[e.op] = "applied";
    if (e.type === "failed") status[e.op] = "unknown";
    if (e.type === "skipped") status[e.op] = "not_applied";
  }
  return { decided: decided?.ops.length ?? 0, status };
}

/** Can another process see progress while the engine runs? */
function inspectMidRun(engine: string, dir: string): Record<string, unknown> {
  switch (engine) {
    case "codemode": {
      const { exit, view } = codemodeInspect(dir);
      const attempts = view?.operations?.reduce((t: number, op: { attempts: unknown[] }) => t + op.attempts.length, 0);
      return { supported: exit === 0 ? "yes" : "no", how: "codemode plan inspect --json (read-only)", exit, attempts_committed: attempts, session_state: view?.sessions?.at(-1)?.state };
    }
    case "pi-durable":
      return { supported: "raw-storage-only", how: "node:sqlite read-only open of Pi's storage; Pi's inspect()/watch() are in-process APIs", storage: sqliteRows(join(dir, "pi", "session.sqlite")) };
    case "composio-script": {
      const progress = scriptProgress(dir);
      return { supported: progress ? "yes" : "no", how: "the script's own progress file", progress };
    }
    default:
      return { supported: "no", how: "OpenCode keeps no state outside the running process" };
  }
}

/** After an interrupt, is there an exportable diagnosis naming each operation applied, not applied, or unknown? */
function diagnoseAfterKill(engine: string, dir: string): Record<string, unknown> {
  switch (engine) {
    case "codemode": {
      const { exit, view } = codemodeInspect(dir);
      const ops = Object.fromEntries((view?.operations ?? []).map((op: { operation: string; conclusion: string; next: string }) => [op.operation, `${op.conclusion}/${op.next}`]));
      return { exportable: exit === 0 && Object.keys(ops).length === 11 ? "yes" : "no", how: "codemode plan inspect --json", operations: ops };
    }
    case "composio-script": {
      const progress = scriptProgress(dir);
      return { exportable: progress && progress.decided === 11 ? "yes" : "no", how: "the script's progress file (its own format)", operations: progress?.status };
    }
    case "pi-durable":
      return {
        exportable: "no",
        how: "no supported export; the operations a program has not reached yet are not recorded anywhere, and recorded calls are in Pi's internal storage",
        storage: sqliteRows(join(dir, "pi", "session.sqlite")),
      };
    default:
      return { exportable: "no", how: "OpenCode keeps no state outside the running process" };
  }
}

// ---- one run ----------------------------------------------------------------------------------------

async function runOne(scenario: Scenario, row: string, engine: string, rep: number, timed: boolean, extraArgs: string[] = []): Promise<RunRecord> {
  const dir = join(cacheRuns, scenario.id, row, `${engine}${extraArgs.length ? "-check" : ""}`, String(rep));
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const provider = await startProvider(dir);
  const options = { ...config.rows[row], ...(scenario.row_overrides?.[row] ?? {}) };
  const workload = scenario.workload ?? "triage";
  const wallMs = scenario.limit ? config.protocol.limit_wall_ms : config.protocol.wall_ms;
  const attempts: Attempt[] = [];
  const rss = { peak: 0 };
  const properties: Record<string, unknown> = {};
  const pauses = (scenario.faults ?? []).some((f) => String((f as { kind: string }).kind).startsWith("pause_"));
  try {
    for (let n = 0; ; n++) {
      const armFaults = n === 0 || scenario.faults_every_attempt;
      await provider.control({ action: "faults", faults: armFaults ? (scenario.faults ?? []) : [] });
      const attempt = await runAttempt(
        {
          engine,
          dir,
          n,
          provider,
          options,
          workload,
          widen: scenario.widen ?? false,
          extraArgs,
          wallMs,
          killOnArmed: (scenario.kill_on_armed ?? false) && armFaults === true,
          beforeKill: scenario.probe_properties && n === 0 ? async () => void (properties.mid_run_inspection = inspectMidRun(engine, dir)) : undefined,
        },
        rss,
      );
      attempts.push(attempt);
      if (attempt.killed_by === "fault") {
        if (pauses) await provider.control({ action: "unpause" });
        if (scenario.probe_properties && n === 0) properties.diagnosis_after_interrupt = diagnoseAfterKill(engine, dir);
        for (const step of scenario.after_kill ?? []) await provider.control(step);
        if (scenario.release_after_kill) await provider.control({ action: "release" });
      }
      if (scenario.release_after_attempt === n) await provider.control({ action: "release" });
      if (scenario.wait_for_late_effect && n === 0) {
        for (let i = 0; i < 60 && !readLedger(provider.ledger).some((r) => r.event === "effect_applied" && r.late); i++) await sleep(100);
      }
      if (n >= scenario.max_recoveries) break;
      if (attempt.killed_by === "fault") continue;
      if (attempt.killed_by === "wall") break;
      if (attempt.exit === 3) continue;
      if (scenario.recover_once && n === 0 && attempt.exit !== 0) continue;
      break;
    }
    // Let any effect still held land, so the final ledger shows every late effect.
    await provider.control({ action: "release" });
    await sleep(50);
  } finally {
    provider.stop();
  }

  const records = readLedger(provider.ledger);
  const claims: Claims[] = attempts
    .map((a) => join(dir, `claims-${a.n}.json`))
    .filter((p) => existsSync(p))
    .map((p) => JSON.parse(readFileSync(p, "utf8")));
  const finalClaims = attempts.at(-1)?.killed_by ? null : (claims.at(-1) ?? null);
  const oracle = score({ records, expect: scenario.expect, claims, final: finalClaims });
  const firstStart = records.find((r) => r.event === "start");
  const record: RunRecord = {
    scenario: scenario.id,
    row,
    engine: extraArgs.length ? `${engine} ${extraArgs.join(" ")}` : engine,
    rep,
    timed,
    status: "ran",
    workload,
    attempts,
    recovery_requests: attempts.length - 1,
    total_ms: Math.round(attempts.reduce((t, a) => t + a.ms, 0) * 10) / 10,
    startup_ms: firstStart ? firstStart.ts - attempts[0].spawn_ts : null,
    peak_rss_mb_sampled: Math.round(rss.peak),
    durable_records: finalClaims?.durable_records ?? claims.at(-1)?.durable_records ?? null,
    claims_final: finalClaims,
    claims_all: claims,
    oracle,
    properties: Object.keys(properties).length ? properties : undefined,
    ledger: {
      requests: records.filter((r) => r.event === "start").length,
      writes_sent: records.filter((r) => r.event === "start" && WRITE_SLUGS.includes(r.slug!)).length,
      effects: records.filter((r) => r.event === "effect_applied").length,
      late_effects: records.filter((r) => r.event === "effect_applied" && r.late).length,
    },
    load: loadAverage(),
  };
  const broken = attempts.filter((a) => a.exit === 2);
  if (broken.length) {
    record.adapter_errors = broken.map((a) => readFileSync(join(dir, `attempt-${a.n}.log`), "utf8").slice(-600));
    log(`ADAPTER ERROR in ${dir}`);
  }
  if (scenario.fault_op) {
    const op = scenario.fault_op;
    record.fault_op_claims = claims.map((c) => `${c.attempt}:${c.ops[op] ?? (c.raw?.[op] ? `(${c.raw[op]})` : "not reported")}`);
  }
  if (scenario.limit) {
    const a = attempts[0];
    record.limit_outcome =
      a.killed_by === "wall"
        ? `unbounded: harness kill at ${wallMs / 1000} s`
        : a.signal
          ? `host process died (${a.signal})`
          : `stopped by the engine: ${claims[0]?.run_status ?? `exit ${a.exit}`}`;
  }
  if (engine === "opencode" && scenario.max_recoveries > 0 && !scenario.limit) record.capability = config.capabilities.opencode.durable_recovery;
  return record;
}

// ---- the protocol -----------------------------------------------------------------------------------

async function waitForQuiet(): Promise<number[]> {
  const readings: number[] = [];
  for (let i = 0; i < 20; i++) {
    const load = loadAverage();
    readings.push(load[0]);
    if (load[0] <= config.protocol.load_max) return readings;
    log(`load ${load[0]} > ${config.protocol.load_max}; waiting`);
    await sleep(config.protocol.load_wait_ms);
  }
  return readings;
}

async function protocol(): Promise<void> {
  mkdirSync(out, { recursive: true });
  const runsPath = join(out, "runs.jsonl");
  writeFileSync(join(out, "environment.json"), `${JSON.stringify(environment(), null, 2)}\n`);
  const batches: Array<{ scenario: string; row: string; load_readings: number[]; started: string }> = [];
  const write = (record: RunRecord) => appendFileSync(runsPath, `${JSON.stringify(record)}\n`);
  for (const scenario of config.scenarios) {
    if (selectedScenarios && !selectedScenarios.includes(scenario.id)) continue;
    for (const row of scenario.rows ?? ["host-default"]) {
      const engines = selectedEngines.filter((e) => config.engines.includes(e));
      for (const engine of engines) {
        const reason = scenario.unsupported?.[engine];
        if (reason) write({ scenario: scenario.id, row, engine, rep: 0, timed: false, status: "unsupported", reason });
      }
      const runnable = engines.filter((e) => !scenario.unsupported?.[e]);
      const latency = scenario.kind === "latency";
      const reps = Number(cli.reps ?? scenario.reps ?? (latency ? config.protocol.latency_reps : config.protocol.fault_reps));
      const warmup = latency && !cli.reps ? config.protocol.latency_warmup : 0;
      const readings = await waitForQuiet();
      batches.push({ scenario: scenario.id, row, load_readings: readings, started: new Date().toISOString() });
      for (let rep = 0; rep < warmup + reps; rep++) {
        // Interleave implementations, rotating the order each round.
        const order = runnable.map((_, i) => runnable[(i + rep) % runnable.length]);
        for (const engine of order) {
          const record = await runOne(scenario, row, engine, rep - warmup, rep >= warmup && latency);
          if (rep >= warmup) write(record);
          log(`${scenario.id} ${row} ${engine} #${rep - warmup}: ${record.oracle?.verdict}${record.limit_outcome ? ` (${record.limit_outcome})` : ""} ${record.total_ms} ms, ${record.attempts?.length} attempt(s)`);
        }
      }
    }
  }
  for (const check of config.harness_checks) {
    if (selectedScenarios && !selectedScenarios.includes(check.scenario)) continue;
    const scenario = config.scenarios.find((s) => s.id === check.scenario)!;
    const record = await runOne(scenario, "host-default", check.engine, 0, false, check.extra_args);
    write({ ...record, scenario: `${check.id}:${check.scenario}` });
    log(`harness check ${check.id}: ${record.oracle?.verdict}`);
  }
  writeFileSync(join(out, "batches.json"), `${JSON.stringify(batches, null, 2)}\n`);
}

// ---- summary ----------------------------------------------------------------------------------------

interface Cell {
  scenario: string;
  row: string;
  engine: string;
  status: "ran" | "unsupported";
  reason?: string;
  n: number;
  verdicts: Record<string, number>;
  flags: Record<string, number>;
  latency_ms: Distribution | null;
  startup_ms_p50: number | null;
  recovery_requests: { median: number | null; max: number | null };
  durable_records_p50: number | null;
  peak_rss_mb_sampled_p50: number | null;
  fault_op_final_claims: Record<string, number>;
  /** The first attempt's claim for the faulted operation: what the engine said about the call in flight. */
  fault_op_first_claims: Record<string, number>;
  limit_outcomes: Record<string, number>;
  capability?: string;
}

const median = (values: number[]) => (values.length ? distribution(values)!.p50 : null);

function cells(records: RunRecord[]): Cell[] {
  const groups = new Map<string, RunRecord[]>();
  for (const r of records) {
    const key = `${r.scenario}|${r.row}|${r.engine}`;
    groups.set(key, [...(groups.get(key) ?? []), r]);
  }
  return [...groups.values()].map((runs): Cell => {
    const first = runs[0];
    if (first.status === "unsupported") {
      return { scenario: first.scenario, row: first.row, engine: first.engine, status: "unsupported", reason: first.reason, n: 0, verdicts: {}, flags: {}, latency_ms: null, startup_ms_p50: null, recovery_requests: { median: null, max: null }, durable_records_p50: null, peak_rss_mb_sampled_p50: null, fault_op_final_claims: {}, fault_op_first_claims: {}, limit_outcomes: {} };
    }
    const count = (f: (r: RunRecord) => boolean) => runs.filter(f).length;
    const tally = (values: string[]) => values.reduce<Record<string, number>>((t, v) => ({ ...t, [v]: (t[v] ?? 0) + 1 }), {});
    const timed = runs.filter((r) => r.timed);
    return {
      scenario: first.scenario,
      row: first.row,
      engine: first.engine,
      status: "ran",
      n: runs.length,
      verdicts: tally(runs.map((r) => r.oracle!.verdict)),
      flags: {
        unjustified_success: count((r) => r.oracle!.unjustified_success.length > 0),
        unjustified_non_effect: count((r) => r.oracle!.unjustified_non_effect.length > 0),
        failed_but_applied: count((r) => r.oracle!.failed_but_applied.length > 0),
        unsafe_duplicate: count((r) => r.oracle!.unsafe_duplicates.length > 0),
        redundant_write: count((r) => r.oracle!.redundant_writes.length > 0),
        unsafe_send: count((r) => r.oracle!.unsafe_sends.length > 0),
        human_review: count((r) => r.oracle!.human_review),
        final_state_match: count((r) => r.oracle!.final_state === "match"),
        widened_write_applied: count((r) => r.oracle!.widened_write_applied === true),
      },
      latency_ms: distribution((timed.length ? timed : runs).map((r) => r.total_ms!)),
      startup_ms_p50: median(runs.map((r) => r.startup_ms).filter((v): v is number => typeof v === "number")),
      recovery_requests: { median: median(runs.map((r) => r.recovery_requests!)), max: Math.max(...runs.map((r) => r.recovery_requests!)) },
      durable_records_p50: median(runs.map((r) => r.durable_records).filter((v): v is number => typeof v === "number")),
      peak_rss_mb_sampled_p50: median(runs.map((r) => r.peak_rss_mb_sampled!)),
      fault_op_final_claims: tally(runs.filter((r) => r.fault_op_claims).map((r) => (r.claims_final ? r.fault_op_claims!.at(-1)!.replace(/^\d+:/, "") : "no final claim"))),
      fault_op_first_claims: tally(runs.filter((r) => r.fault_op_claims?.length).map((r) => r.fault_op_claims![0])),
      limit_outcomes: tally(runs.filter((r) => r.limit_outcome).map((r) => r.limit_outcome!.replace(/: .*/s, (m) => m.slice(0, 80)))),
      capability: first.capability,
    };
  });
}

function summarize(): void {
  const records: RunRecord[] = readFileSync(join(out, "runs.jsonl"), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  const main = records.filter((r) => !r.scenario.includes(":"));
  const table = cells(main);
  const find = (scenario: string, row: string, engine: string) => table.find((c) => c.scenario === scenario && c.row === row && c.engine === engine);

  // Latency comparisons per the tie rule.
  const latency: Array<{ row: string; against: string; result: string; codemode: Distribution | null; other: Distribution | null }> = [];
  for (const row of ["host-default", "equal-limits"]) {
    const cm = find("no-fault", row, "codemode")?.latency_ms;
    for (const other of ["pi-durable", "opencode", "composio-script"]) {
      const o = find("no-fault", row, other)?.latency_ms;
      if (!cm || !o) continue;
      const c = compareLatency(cm, o);
      latency.push({ row, against: other, result: c === "tie" ? "tie" : c === "a-faster" ? "codemode faster" : `${other} faster`, codemode: cm, other: o });
    }
  }

  // Harness checks.
  const noFault = main.filter((r) => r.scenario === "no-fault");
  const finals = new Set(
    noFault.map((r) => {
      const dir = join(cacheRuns, "no-fault", r.row, r.engine, String(r.rep));
      return JSON.stringify(normalized(stateFromLedger(readLedger(join(dir, "ledger.jsonl")))));
    }),
  );
  const blind = records.find((r) => r.scenario.startsWith("oracle-blind-retry"));
  const harness = {
    no_fault_identical_final_state: { runs: noFault.length, distinct_final_states: finals.size, pass: finals.size === 1 && noFault.every((r) => r.oracle!.final_state === "match") },
    blind_retry_flagged_unsafe_duplicate: blind ? { verdict: blind.oracle!.verdict, duplicates: blind.oracle!.unsafe_duplicates, pass: blind.oracle!.unsafe_duplicates.length > 0 } : null,
  };

  // Properties.
  const props: Record<string, unknown> = {};
  for (const engine of config.engines) {
    const late = main.filter((r) => r.scenario === "late-effect" && r.engine === engine && r.properties);
    const widen = main.find((r) => r.scenario === "widen" && r.engine === engine);
    props[engine] = {
      widened_change_list_runs_without_reapproval: widen ? (widen.oracle!.widened_write_applied ? "yes" : "no") : null,
      widen_detail: widen?.claims_final?.detail ?? null,
      inspectable_from_another_process_mid_run: tallyField(late.map((r) => (r.properties!.mid_run_inspection as { supported: string })?.supported)),
      exportable_diagnosis_after_interrupt: tallyField(late.map((r) => (r.properties!.diagnosis_after_interrupt as { exportable: string })?.exportable)),
      example: late[0]?.properties ?? null,
    };
  }
  const hypotheses = evaluateHypotheses(table, latency);
  const summary = { generated: new Date().toISOString(), hypotheses, cells: table, latency, harness, properties: props };
  writeFileSync(join(out, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  writeFileSync(join(out, "summary.md"), markdown(table, latency, harness, props, hypotheses));
}

// ---- hypotheses: the README's rules, applied mechanically ------------------------------------------

interface HeadToHead {
  scenario: string;
  row: string;
  codemode: string;
  other: string;
  /** Safety: fewer runs judged `wrong`. */
  result: "win" | "tie" | "loss" | "n/a";
  /** Completion without a person: more runs judged `correct`. */
  completion: "more" | "same" | "fewer" | "n/a";
}

const ROWS = ["host-default", "equal-limits"];
const wrongRuns = (c: Cell | undefined) => c?.verdicts.wrong ?? 0;

/** Per scenario and row: fewer `wrong` runs wins; equal counts tie. */
function headToHead(table: Cell[], other: string, scenarios: string[]): HeadToHead[] {
  const rows: HeadToHead[] = [];
  for (const scenario of scenarios) {
    for (const row of ROWS) {
      const cm = table.find((c) => c.scenario === scenario && c.row === row && c.engine === "codemode");
      const o = table.find((c) => c.scenario === scenario && c.row === row && c.engine === other);
      if (!cm || !o) continue;
      const fmt = (c: Cell) => (c.status === "unsupported" ? "unsupported" : Object.entries(c.verdicts).map(([k, n]) => `${k} ${n}`).join(", "));
      const a = wrongRuns(cm);
      const b = wrongRuns(o);
      const ca = cm.verdicts.correct ?? 0;
      const cb = o.verdicts.correct ?? 0;
      const na = o.status === "unsupported";
      rows.push({
        scenario,
        row,
        codemode: fmt(cm),
        other: fmt(o),
        result: na ? "n/a" : a < b ? "win" : a > b ? "loss" : "tie",
        completion: na ? "n/a" : ca > cb ? "more" : ca < cb ? "fewer" : "same",
      });
    }
  }
  return rows;
}

function overall(rows: HeadToHead[]): string {
  const wins = rows.filter((r) => r.result === "win").length;
  const losses = rows.filter((r) => r.result === "loss").length;
  const fewer = rows.filter((r) => r.completion === "fewer").length;
  const more = rows.filter((r) => r.completion === "more").length;
  const completion = `; completion without a person: codemode more in ${more} cell(s), fewer in ${fewer}`;
  if (losses > 0 && wins > 0) return `mixed on safety: ${wins} win(s), ${losses} loss(es)${completion}`;
  if (losses > 0) return `lost on safety (${losses} loss(es))${completion}`;
  if (wins > 0) return `demonstrated on safety (${wins} win(s), ${rows.length - wins} tie(s))${completion}`;
  return rows.length ? `tied on safety${completion}` : "not run";
}

function evaluateHypotheses(table: Cell[], latency: Array<{ row: string; against: string; result: string }>): Record<string, unknown> {
  const cell = (scenario: string, row: string, engine: string) => table.find((c) => c.scenario === scenario && c.row === row && c.engine === engine);
  const widen = (engine: string) => cell("widen", "host-default", engine)?.flags.widened_write_applied;
  const sends = (scenario: string, engine: string) => ROWS.reduce((t, row) => t + (cell(scenario, row, engine)?.flags.unsafe_send ?? 0), 0);
  const binding = (other: string) => ({
    widened_write_applied: { codemode: widen("codemode"), [other]: widen(other) },
    runs_with_unsafe_sends: Object.fromEntries(
      ["account-switched", "account-revoked", "scope-changed"].map((s) => [s, { codemode: sends(s, "codemode"), [other]: sends(s, other) }]),
    ),
  });
  const bindingVerdict = (other: string, scenarios: string[]) => {
    const b = binding(other);
    const cmClean = widen("codemode") === 0 && scenarios.every((s) => sends(s, "codemode") === 0);
    const otherExposed = widen(other) === 1 || scenarios.some((s) => sends(s, other) > 0);
    if (!cmClean) return { verdict: "lost", evidence: b };
    return { verdict: otherExposed ? "demonstrated" : "tied", evidence: b };
  };
  const recon = ["lost-response", "late-effect", "probe-budget", "broken-transport", "slow-reply"];
  const recovery = ["lost-response", "kill-before-first-write", "kill-after-first-write", "late-effect", "repeated-crash", "probe-budget", "broken-transport"];
  /** Runs that ended correct without a person, against runs that needed one. */
  const settled = (engine: string) =>
    Object.fromEntries(
      recovery.map((s) => [
        s,
        ROWS.map((row) => {
          const c = cell(s, row, engine);
          return c ? `${row}: correct ${c.verdicts.correct ?? 0}, needs review ${c.verdicts["correct-needs-review"] ?? 0}, safe-incomplete ${c.verdicts["safe-incomplete"] ?? 0}, wrong ${c.verdicts.wrong ?? 0}` : `${row}: -`;
        }).join("; "),
      ]),
    );
  return {
    H1a_vs_script_approval_binding: bindingVerdict("composio-script", ["account-switched"]),
    H1b_vs_script_lost_comment_reconciliation: { verdict: overall(headToHead(table, "composio-script", recon)), head_to_head: headToHead(table, "composio-script", recon) },
    H2_vs_pi_approval_binding: bindingVerdict("pi-durable", ["account-switched", "account-revoked", "scope-changed"]),
    H2_vs_pi_contract_reconciliation: {
      verdict: "see per-scenario settlement: codemode settles from provider evidence where Pi reports interrupted",
      safety: overall(headToHead(table, "pi-durable", recovery)),
      head_to_head_wrong_runs: headToHead(table, "pi-durable", recovery),
      codemode: settled("codemode"),
      "pi-durable": settled("pi-durable"),
    },
    H3_vs_opencode_durable_recovery: {
      verdict: "OpenCode durable recovery: unsupported (capability); codemode runs in the recovery scenarios are scored below, with OpenCode's rerun-from-scratch alongside",
      safety_vs_rerun: overall(headToHead(table, "opencode", recovery)),
      head_to_head_wrong_runs_vs_rerun: headToHead(table, "opencode", recovery),
    },
    H0_latency: latency,
  };
}

function tallyField(values: Array<string | undefined>): Record<string, number> {
  return values.reduce<Record<string, number>>((t, v) => ({ ...t, [v ?? "none"]: (t[v ?? "none"] ?? 0) + 1 }), {});
}

function markdown(
  table: Cell[],
  latency: Array<{ row: string; against: string; result: string; codemode: Distribution | null; other: Distribution | null }>,
  harness: unknown,
  props: Record<string, unknown>,
  hypotheses: Record<string, any>,
): string {
  const lines: string[] = [];
  const env = JSON.parse(readFileSync(join(out, "environment.json"), "utf8"));
  lines.push(`# Workflow comparison results, ${out.split("/").at(-1)}`, "");
  lines.push(`codemode \`${env.codemode.commit.slice(0, 7)}\`${env.codemode.dirty ? " (uncommitted changes in src)" : ""}; Pi Durable ${env.pins.implementations["pi-durable"].version}; OpenCode codemode ${env.pins.implementations.opencode.version}; Node ${env.node}; ${env.machine.cpu}, ${env.machine.cpus} CPUs, ${env.machine.memory_gib} GiB, ${env.machine.os}. Load at start: ${env.machine.load_average_at_start.join(" ")}. Protocol and hypotheses: [README](../../README.md) (SHA-256 \`${env.readme_sha256.slice(0, 12)}\`).`, "");
  lines.push("## Latency, no-fault workflow (ms, process start to exit of the last attempt)", "");
  lines.push("| Row | Implementation | n | p25 | p50 | p75 | p95 | startup p50 | peak RSS MiB (sampled) | durable records |", "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
  for (const c of table.filter((c) => c.scenario === "no-fault")) {
    const d = c.latency_ms!;
    lines.push(`| ${c.row} | ${c.engine} | ${d.n} | ${d.p25} | ${d.p50} | ${d.p75} | ${d.p95} | ${c.startup_ms_p50 ?? "-"} | ${c.peak_rss_mb_sampled_p50 ?? "-"} | ${c.durable_records_p50 ?? "-"} |`);
  }
  lines.push("", "| Row | codemode vs | Result (tie rule) |", "| --- | --- | --- |");
  for (const l of latency) lines.push(`| ${l.row} | ${l.against} | ${l.result} |`);
  lines.push("", "## Fault scenarios", "");
  lines.push("Verdict counts per cell; flags count runs with that flag. `rec` is recovery requests (median/max).", "");
  lines.push("| Scenario | Row | Implementation | n | Verdicts | Unsafe dup | Unjust. success | Unjust. non-effect | Failed but applied | Unsafe send | Review | Fault op: first reported claim (attempt:claim) | Fault op: final claim | rec | p50 ms |", "| --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | ---: |");
  for (const c of table.filter((c) => c.scenario !== "no-fault" && !["cpu-loop", "memory-growth"].includes(c.scenario))) {
    if (c.status === "unsupported") {
      lines.push(`| ${c.scenario} | ${c.row} | ${c.engine} | 0 | unsupported: ${c.reason} | | | | | | | | | | |`);
      continue;
    }
    const v = Object.entries(c.verdicts).map(([k, n]) => `${k} ${n}`).join(", ");
    const first = Object.entries(c.fault_op_first_claims).map(([k, n]) => `${k} ${n}`).join(", ") || "-";
    const claims = Object.entries(c.fault_op_final_claims).map(([k, n]) => `${k} ${n}`).join(", ") || "-";
    const f = c.flags;
    lines.push(`| ${c.scenario} | ${c.row} | ${c.engine}${c.capability ? " (rerun)" : ""} | ${c.n} | ${v} | ${f.unsafe_duplicate} | ${f.unjustified_success} | ${f.unjustified_non_effect} | ${f.failed_but_applied} | ${f.unsafe_send} | ${f.human_review} | ${first} | ${claims} | ${c.recovery_requests.median}/${c.recovery_requests.max} | ${c.latency_ms?.p50 ?? "-"} |`);
  }
  lines.push("", "(rerun): OpenCode keeps no durable state; after a kill the driver reran the convergent program from scratch.", "");
  lines.push("## CPU loop and memory growth", "", "| Scenario | Row | Implementation | n | Outcome | Time to stop p50 ms | Peak RSS MiB p50 (sampled) |", "| --- | --- | --- | ---: | --- | ---: | ---: |");
  for (const c of table.filter((c) => ["cpu-loop", "memory-growth"].includes(c.scenario))) {
    if (c.status === "unsupported") {
      lines.push(`| ${c.scenario} | ${c.row} | ${c.engine} | 0 | unsupported: ${c.reason} | | |`);
      continue;
    }
    lines.push(`| ${c.scenario} | ${c.row} | ${c.engine} | ${c.n} | ${Object.entries(c.limit_outcomes).map(([k, n]) => `${k} (${n})`).join("; ")} | ${c.latency_ms?.p50} | ${c.peak_rss_mb_sampled_p50} |`);
  }
  lines.push("", "## Properties", "", "| Implementation | Widened change list runs without re-approval | Inspectable from another process mid-run | Exportable diagnosis after interrupt |", "| --- | --- | --- | --- |");
  for (const [engine, p] of Object.entries(props) as Array<[string, Record<string, unknown>]>) {
    const fmt = (v: unknown) => (v && typeof v === "object" ? Object.entries(v).map(([k, n]) => `${k} ${n}`).join(", ") : String(v));
    lines.push(`| ${engine} | ${fmt(p.widened_change_list_runs_without_reapproval)} | ${fmt(p.inspectable_from_another_process_mid_run)} | ${fmt(p.exportable_diagnosis_after_interrupt)} |`);
  }
  lines.push("", "## Hypotheses (README rules applied to these cells)", "");
  lines.push(`- H1a vs the script, approval binding: **${hypotheses.H1a_vs_script_approval_binding.verdict}**`);
  lines.push(`- H1b vs the script, lost-comment reconciliation: **${hypotheses.H1b_vs_script_lost_comment_reconciliation.verdict}**`);
  lines.push(`- H2 vs Pi Durable, approval binding: **${hypotheses.H2_vs_pi_approval_binding.verdict}**`);
  lines.push(`- H2 vs Pi Durable, contract reconciliation: ${hypotheses.H2_vs_pi_contract_reconciliation.verdict}; ${hypotheses.H2_vs_pi_contract_reconciliation.safety}`);
  lines.push(`- H3 vs OpenCode: ${hypotheses.H3_vs_opencode_durable_recovery.verdict}; against the rerun: ${hypotheses.H3_vs_opencode_durable_recovery.safety_vs_rerun}`);
  lines.push("", "Head-to-head: safety by `wrong` runs (fewer wins); completion by `correct` runs (finished without a person).", "", "| Against | Scenario | Row | codemode | Other | Safety | Completion |", "| --- | --- | --- | --- | --- | --- | --- |");
  const h2h: Array<[string, HeadToHead[]]> = [
    ["composio-script", hypotheses.H1b_vs_script_lost_comment_reconciliation.head_to_head],
    ["pi-durable", hypotheses.H2_vs_pi_contract_reconciliation.head_to_head_wrong_runs],
    ["opencode (rerun)", hypotheses.H3_vs_opencode_durable_recovery.head_to_head_wrong_runs_vs_rerun],
  ];
  for (const [against, rows] of h2h) for (const r of rows) lines.push(`| ${against} | ${r.scenario} | ${r.row} | ${r.codemode} | ${r.other} | ${r.result} | ${r.completion} |`);
  lines.push("", "## Harness checks", "", "```json", JSON.stringify(harness, null, 2), "```", "");
  return `${lines.join("\n")}\n`;
}

if (!cli["summarize-only"]) await protocol();
summarize();
log(`results in ${out}`);
