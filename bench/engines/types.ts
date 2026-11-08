/** The adapter contract every engine module in bench/engines implements. */
import type { ToolFn } from "../fixtures/tools.ts";

export type { ToolCtx, ToolFn } from "../fixtures/tools.ts";

export interface Limits {
  timeoutMs: number;
  maxCalls: number;
  concurrency: number;
  maxOutputBytes: number;
}

export interface EngineInput {
  /** Function body that calls `await tools.call(name, args)` and ends with `return`. */
  code: string;
  tools: Record<string, ToolFn>;
  limits: Limits;
  signal?: AbortSignal;
}

/**
 * Normalized error kinds, so scenarios can state one expectation for every engine:
 * timeout, guest_error (the program threw or let a tool failure escape), call_limit,
 * output_limit, cancelled, unsupported (the engine refused the program's syntax),
 * engine_error (anything else), harness_error (the adapter itself failed).
 */
export type ErrorKind =
  | "timeout"
  | "guest_error"
  | "call_limit"
  | "output_limit"
  | "cancelled"
  | "unsupported"
  | "engine_error"
  | "harness_error";

export interface EngineResult {
  ok: boolean;
  output?: unknown;
  /** `engineKind` keeps the engine's own classification next to the normalized one. */
  error?: { kind: ErrorKind; message: string; engineKind?: string };
  engineTrace?: unknown;
  setupMs: number;
  runMs: number;
}

/** What one limit means for an engine: enforced as given, fixed, or not supported. */
export interface LimitSupport {
  timeoutMs: string;
  maxCalls: string;
  concurrency: string;
  maxOutputBytes: string;
}

export interface EngineInfo {
  name: string;
  version: string;
  limits: LimitSupport;
  shim: string;
}

export interface EngineModule {
  info: EngineInfo;
  runEngine(input: EngineInput): Promise<EngineResult>;
}
