/**
 * Host-neutral client for `codemode bridge` (protocol version 2).
 *
 * A host adapter supplies the authorized tool names and one `execute` function;
 * this module owns the subprocess, framing, cancellation, and report. It uses only
 * Node built-ins so any Node-compatible host (Pi's Node, OpenCode's Bun) can load it.
 *
 * Boundaries:
 * - Rust owns admission, the deadline, call allowance, concurrency, output budget,
 *   call identities, and the execution record. The guest runs in restricted Deno.
 * - The bridge starts with an empty environment and explicit executable paths, so
 *   the host's credentials and other environment variables never reach Rust or the guest.
 * - Each call frame is acknowledged with a `received` fact; `execute` can report
 *   later milestones (permission decisions, executor entry). Rust records them as
 *   host claims stamped with the host's `performance.now()` clock.
 * - `startBridge` starts a bridge, and its guest, before the program is known; pass
 *   it to `runBridge` as `started` to take that startup off the run's critical path.
 *   A started bridge serves one run; close its stdin, or kill it, to discard it.
 * - Aborting `signal` sends `cancel` to Rust and aborts outstanding host calls; the
 *   bridge is killed if it has not exited two seconds later. Calls already
 *   dispatched are recorded as unknown; their effects may remain.
 */
import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { accessSync, constants } from "node:fs";
import { delimiter, isAbsolute, join, resolve } from "node:path";

/** Matches `BRIDGE_FRAME_BYTES` in the Rust crate. */
export const HOST_FRAME_BYTES = 1024 * 1024;
/** Rust's per-guest-frame limit, which bounds source and each call's arguments. */
const GUEST_FRAME_BYTES = 64 * 1024;
const OUTPUT_BYTES = 262_144;
const KILL_GRACE_MS = 2_000;
const STDERR_TAIL = 4_096;

/** A host's observed outcome for one call; Rust records it verbatim. */
export type HostReply =
  | { ok: true; value: unknown }
  | { ok: false; error: { code: string; message: string } };

/** The execution record returned by the bridge. */
export interface RunReport {
  status: string;
  output?: unknown;
  diagnostic?: string | null;
  calls: unknown[];
  [key: string]: unknown;
}

/** Milestones a host can report for a running call. */
export type HostFact = "permission_requested" | "permission_granted" | "permission_denied" | "executor_started";

/** One call admitted and dispatched by Rust. `id` is its trace identity. */
export interface BridgeCall {
  id: number;
  tool: string;
  arguments: unknown;
}

/** The process and limits of a bridge; fixed when it starts. */
export interface BridgeLimits {
  /** Absolute path of the `codemode` executable. */
  bin: string;
  /** Absolute path of the Deno executable for the guest. */
  deno: string;
  cwd: string;
  timeoutMs: number;
  concurrency: number;
  maxCalls: number;
  /** Shared reply and output byte budget; defaults to 262144. */
  maxOutputBytes?: number;
}

/** A bridge started ahead of a run by `startBridge`. */
export interface StartedBridge {
  readonly child: ChildProcessWithoutNullStreams;
  /** The bridge's command line, compared with the run's limits. */
  readonly key: string;
  /** Set once the process has exited; it can no longer serve a run. */
  exited: boolean;
  /** The end of the bridge's stderr. */
  stderr: string;
  /** Set if the process could not be started or failed while waiting. */
  error?: Error;
}

export interface BridgeRun extends BridgeLimits {
  code: string;
  /** The host's authorization list, sent in the start frame. */
  tools: readonly string[];
  /** A bridge from `startBridge` with the same limits; one is started otherwise. */
  started?: StartedBridge;
  /** Aborting it cancels the run. */
  signal?: AbortSignal;
  /**
   * Run one authorized call. Rejections become `tool_error`. Call `report` at
   * milestones such as `executor_started`, just before effects may begin.
   */
  execute(call: BridgeCall, signal: AbortSignal, report: (fact: HostFact) => void): Promise<HostReply>;
}

/** Resolve an executable name the way a shell would, relative names against `cwd`. */
export function findExecutable(name: string, cwd: string): string {
  if (isAbsolute(name)) return name;
  if (name.includes("/")) return resolve(cwd, name);
  for (const directory of (process.env.PATH ?? "").split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, name);
    try {
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Keep searching.
    }
  }
  throw new Error(`${name} was not found on PATH; set it explicitly`);
}

/** Parse a positive integer option; `name` appears in the error. */
export function positive(name: string, value: string | number): number {
  const text = String(value);
  if (!/^[1-9][0-9]*$/.test(text)) throw new Error(`${name} must be a positive integer`);
  return Number(text);
}

/** Model-facing summary: status, call count, and output or diagnostic. */
export function summarize(report: RunReport): Record<string, unknown> {
  const summary: Record<string, unknown> = { status: report.status, calls: report.calls.length };
  if ("output" in report) summary.output = report.output;
  if (report.diagnostic) summary.diagnostic = report.diagnostic;
  return summary;
}

/**
 * Bound on frames read from the bridge. The report holds the source, every call's
 * arguments, values and errors within the output budget, and per-call events.
 */
function reportLimit(maxCalls: number, outputBytes: number): number {
  // The first call beyond the allowance is also recorded, as rejected.
  return 2 * GUEST_FRAME_BYTES + (maxCalls + 1) * (GUEST_FRAME_BYTES + 4096) + outputBytes + HOST_FRAME_BYTES;
}

function bridgeArgs(limits: BridgeLimits): string[] {
  return [
    "bridge",
    "--timeout-ms",
    String(limits.timeoutMs),
    "--concurrency",
    String(limits.concurrency),
    "--max-calls",
    String(limits.maxCalls),
    "--max-output-bytes",
    String(limits.maxOutputBytes ?? OUTPUT_BYTES),
    "--deno",
    limits.deno,
  ];
}

const keyOf = (limits: BridgeLimits) => JSON.stringify([limits.bin, limits.cwd, ...bridgeArgs(limits)]);

/**
 * Start a bridge and its guest now, before the program is known. The bridge waits
 * for its program until its stdin closes; the run's deadline starts when
 * `runBridge` sends it.
 */
export function startBridge(limits: BridgeLimits): StartedBridge {
  const child = spawn(limits.bin, bridgeArgs(limits), { cwd: limits.cwd, env: {}, stdio: ["pipe", "pipe", "pipe"] });
  const started: StartedBridge = { child, key: keyOf(limits), exited: false, stderr: "" };
  child.once("close", () => {
    started.exited = true;
  });
  // Kept for runBridge, so a failed spawn of an unused bridge cannot crash the host.
  child.on("error", (error) => {
    started.error ??= error;
    started.exited = true;
  });
  child.stdin.on("error", () => {
    // The bridge exited; its report or exit status explains why.
  });
  child.stderr.on("data", (chunk: Buffer) => {
    started.stderr = (started.stderr + chunk.toString("utf8")).slice(-STDERR_TAIL);
  });
  return started;
}

/** Run one program through `codemode bridge` and resolve with its execution record. */
export async function runBridge(run: BridgeRun): Promise<RunReport> {
  const outputBytes = run.maxOutputBytes ?? OUTPUT_BYTES;
  const frameLimit = reportLimit(run.maxCalls, outputBytes);
  if (run.started && run.started.key !== keyOf(run)) {
    throw new Error("the started bridge has different limits than this run");
  }
  const bridge = run.started ?? startBridge(run);
  const child = bridge.child;

  const authorized = new Set(run.tools);
  const outstanding = new Map<number, AbortController>();
  let finished = false;
  let cancelled = false;
  let killTimer: ReturnType<typeof setTimeout> | undefined;

  const send = (frame: string) => {
    if (!finished && child.stdin.writable) child.stdin.write(frame + "\n");
  };
  const abortOutstanding = () => {
    for (const controller of outstanding.values()) controller.abort();
    outstanding.clear();
  };
  // Escalate only if the bridge does not exit on its own; never keep the host alive for it.
  const terminate = () => {
    killTimer ??= setTimeout(() => child.kill("SIGKILL"), KILL_GRACE_MS);
    killTimer.unref?.();
  };
  child.once("close", () => clearTimeout(killTimer));
  const onAbort = () => {
    cancelled = true;
    send(JSON.stringify({ type: "cancel" }));
    abortOutstanding();
    terminate();
  };

  const reply = (id: number, result: HostReply) => {
    let frame = JSON.stringify({ type: "result", id, ...result });
    if (Buffer.byteLength(frame) >= HOST_FRAME_BYTES) {
      // Observed but too large to send: Rust records result_too_large and stops the run.
      const bytes = Buffer.byteLength(JSON.stringify(result.ok ? result.value : result.error));
      frame = JSON.stringify({ type: "result", id, ok: result.ok, too_large: true, bytes });
    }
    send(frame);
  };

  const fact = (id: number, kind: "received" | HostFact) =>
    send(JSON.stringify({ type: "fact", id, kind, host_ms: performance.now() }));

  const onCall = (frame: BridgeCall) => {
    if (!Number.isSafeInteger(frame.id) || outstanding.has(frame.id)) {
      throw new Error("bridge sent an invalid or duplicate call id");
    }
    fact(frame.id, "received");
    if (cancelled) {
      // A call frame buffered before the abort must not start a host tool afterwards.
      reply(frame.id, { ok: false, error: { code: "cancelled", message: "host cancelled before execution" } });
      return;
    }
    if (!authorized.has(frame.tool)) {
      reply(frame.id, { ok: false, error: { code: "tool_denied", message: "tool is not authorized by the host" } });
      return;
    }
    const controller = new AbortController();
    outstanding.set(frame.id, controller);
    let pending: Promise<HostReply>;
    const report = (kind: HostFact) => {
      if (outstanding.has(frame.id)) fact(frame.id, kind);
    };
    try {
      pending = run.execute(frame, controller.signal, report);
    } catch (error) {
      pending = Promise.reject(error);
    }
    pending.then(
        (result) => {
          if (outstanding.delete(frame.id)) reply(frame.id, result);
        },
        (error: unknown) => {
          const message = error instanceof Error ? error.message : String(error);
          if (outstanding.delete(frame.id)) reply(frame.id, { ok: false, error: { code: "tool_error", message } });
        },
      );
  };

  send(JSON.stringify({ type: "start", version: 2, code: run.code, tools: [...authorized] }));
  if (run.signal?.aborted) onAbort();
  run.signal?.addEventListener("abort", onAbort, { once: true });

  try {
    return await new Promise<RunReport>((resolvePromise, reject) => {
      // Pieces of an incomplete frame; joined once its newline arrives, so a large
      // report is copied and scanned once rather than once per chunk.
      let pieces: Buffer[] = [];
      let pending = 0;
      let bridgeError: string | undefined;
      const fail = (error: Error) => {
        // Start nothing further, even if more frames are already buffered.
        finished = true;
        cancelled = true;
        abortOutstanding();
        child.kill("SIGKILL");
        reject(error);
      };
      child.stdout.on("data", (chunk: Buffer) => {
        let start = 0;
        let newline: number;
        while ((newline = chunk.indexOf(10, start)) !== -1) {
          if (pending + newline - start > frameLimit) {
            fail(new Error(`bridge frame exceeds ${frameLimit} bytes`));
            return;
          }
          pieces.push(chunk.subarray(start, newline));
          const line = Buffer.concat(pieces).toString("utf8");
          pieces = [];
          pending = 0;
          start = newline + 1;
          try {
            const frame = JSON.parse(line);
            if (frame.type === "call") onCall(frame);
            else if (frame.type === "report") {
              finished = true;
              abortOutstanding();
              child.stdin.end();
              terminate();
              resolvePromise(frame.report);
            } else if (frame.type === "error") bridgeError = String(frame.message);
            else throw new Error(`unexpected bridge frame type ${frame.type}`);
          } catch (error) {
            fail(error instanceof Error ? error : new Error(String(error)));
            return;
          }
        }
        if (start < chunk.length) {
          pieces.push(chunk.subarray(start));
          pending += chunk.length - start;
        }
        if (pending > frameLimit) fail(new Error(`bridge frame exceeds ${frameLimit} bytes`));
      });
      child.on("error", (error) => fail(error));
      const ended = (code: number | null, killSignal: NodeJS.Signals | null) => {
        if (finished) return;
        abortOutstanding();
        const reason = bridgeError ?? (bridge.stderr.trim() || `exit ${code ?? killSignal}`);
        reject(new Error(`codemode bridge ended without a report: ${reason}`));
      };
      child.on("close", ended);
      // A started bridge may have failed or exited while it waited.
      if (bridge.error) fail(bridge.error);
      else if (bridge.exited) ended(child.exitCode, child.signalCode);
    });
  } finally {
    run.signal?.removeEventListener("abort", onAbort);
    finished = true;
  }
}
