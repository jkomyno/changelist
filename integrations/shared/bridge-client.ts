/**
 * Host-neutral client for `codemode bridge` (protocol version 1).
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
 * - Aborting `signal` sends `cancel` to Rust and aborts outstanding host calls; the
 *   bridge is killed if it has not exited two seconds later. Calls already
 *   dispatched are recorded as unknown; their effects may remain.
 */
import { spawn } from "node:child_process";
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

/** One call admitted and dispatched by Rust. `id` is its trace identity. */
export interface BridgeCall {
  id: number;
  tool: string;
  arguments: unknown;
}

export interface BridgeRun {
  /** Absolute path of the `codemode` executable. */
  bin: string;
  /** Absolute path of the Deno executable for the guest. */
  deno: string;
  cwd: string;
  code: string;
  /** The host's authorization list, sent in the start frame. */
  tools: readonly string[];
  timeoutMs: number;
  concurrency: number;
  maxCalls: number;
  /** Shared reply and output byte budget; defaults to 262144. */
  maxOutputBytes?: number;
  /** Aborting it cancels the run. */
  signal?: AbortSignal;
  /** Run one authorized call. Rejections become `tool_error`. */
  execute(call: BridgeCall, signal: AbortSignal): Promise<HostReply>;
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

/** Run one program through `codemode bridge` and resolve with its execution record. */
export async function runBridge(run: BridgeRun): Promise<RunReport> {
  const outputBytes = run.maxOutputBytes ?? OUTPUT_BYTES;
  const frameLimit = reportLimit(run.maxCalls, outputBytes);
  const args = [
    "bridge",
    "--timeout-ms",
    String(run.timeoutMs),
    "--concurrency",
    String(run.concurrency),
    "--max-calls",
    String(run.maxCalls),
    "--max-output-bytes",
    String(outputBytes),
    "--deno",
    run.deno,
  ];
  const child = spawn(run.bin, args, { cwd: run.cwd, env: {}, stdio: ["pipe", "pipe", "pipe"] });

  const authorized = new Set(run.tools);
  const outstanding = new Map<number, AbortController>();
  let stderr = "";
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
  child.stdin.on("error", () => {
    // The bridge exited; its report or exit status explains why.
  });
  child.stderr.on("data", (chunk: Buffer) => {
    stderr = (stderr + chunk.toString("utf8")).slice(-STDERR_TAIL);
  });

  const reply = (id: number, result: HostReply) => {
    let frame = JSON.stringify({ type: "result", id, ...result });
    if (Buffer.byteLength(frame) >= HOST_FRAME_BYTES) {
      // Observed but too large to send: Rust records result_too_large and stops the run.
      frame = JSON.stringify({ type: "result", id, ok: result.ok, too_large: true });
    }
    send(frame);
  };

  const onCall = (frame: BridgeCall) => {
    if (!Number.isSafeInteger(frame.id) || outstanding.has(frame.id)) {
      throw new Error("bridge sent an invalid or duplicate call id");
    }
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
    try {
      pending = run.execute(frame, controller.signal);
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

  send(JSON.stringify({ type: "start", version: 1, code: run.code, tools: [...authorized] }));
  if (run.signal?.aborted) onAbort();
  run.signal?.addEventListener("abort", onAbort, { once: true });

  try {
    return await new Promise<RunReport>((resolvePromise, reject) => {
      let buffered = Buffer.alloc(0);
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
        buffered = Buffer.concat([buffered, chunk]);
        let newline: number;
        while ((newline = buffered.indexOf(10)) !== -1) {
          const line = buffered.subarray(0, newline).toString("utf8");
          buffered = buffered.subarray(newline + 1);
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
        if (buffered.length > frameLimit) fail(new Error(`bridge frame exceeds ${frameLimit} bytes`));
      });
      child.on("error", (error) => fail(error));
      child.on("close", (code, killSignal) => {
        if (finished) return;
        abortOutstanding();
        const reason = bridgeError ?? (stderr.trim() || `exit ${code ?? killSignal}`);
        reject(new Error(`codemode bridge ended without a report: ${reason}`));
      });
    });
  } finally {
    run.signal?.removeEventListener("abort", onAbort);
    finished = true;
  }
}
