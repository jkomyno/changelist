/**
 * Optional Composio tool provider for `codemode bridge`.
 *
 * The guest calls Composio tools by slug, for example
 * `tools.call("GITHUB_GET_THE_AUTHENTICATED_USER", {})`. Rust admits, schedules, and
 * records each call; this module runs it in the trusted host with an executor that
 * holds the Composio credentials. Nothing Composio-specific exists in the Rust core.
 *
 * Boundaries:
 * - Credentials live only in the host process: the `composio run` helper context or an
 *   SDK client. The bridge starts with an empty environment (see bridge-client.ts).
 * - Only slugs in `tools` are authorized. Rust rejects other names before the host
 *   sees them; this module refuses non-object arguments before calling Composio.
 * - A guest-visible value is the tool's `data`. A tool that reports
 *   `successful: false` becomes the call error `composio_tool_failed`, and the
 *   program may continue. No call is retried.
 * - When an execution ends without an observed result (the CLI exited without
 *   printing one, or an SDK request failed after it may have been sent), the bridge
 *   protocol has no way to say "unknown" for one call. The adapter therefore leaves
 *   that call unanswered and cancels the run, so Rust records it as `unknown`
 *   instead of a failure the guest could act on, for example by retrying.
 * - Cancellation stops waiting for a call. The `composio run` helper cannot abort an
 *   in-flight execution, so its effect may still occur; Rust records the call as
 *   unknown. The SDK executor forwards the abort signal to its HTTP request, which
 *   also cannot prove the remote effect did not happen.
 */
import { type BridgeRun, type HostReply, type RunReport, runBridge } from "../shared/bridge-client.ts";

/** A Composio execution result, shared by the CLI `execute` helper and `@composio/core`. */
export interface ComposioResult {
  successful: boolean;
  data?: unknown;
  error?: string | null;
}

/** An execution whose outcome was not observed; `reason` explains why. */
export interface Unobserved {
  unobserved: string;
}

/**
 * Runs one authorized Composio tool in the host. Return a reply for an observed
 * outcome and `Unobserved` otherwise. Rejections also count as unobserved.
 */
export type ComposioExecutor = (
  slug: string,
  args: Record<string, unknown>,
  signal: AbortSignal,
) => Promise<HostReply | Unobserved>;

/** The `execute(slug, data)` global that `composio run` injects into its scripts. */
export type RunHelperExecute = (slug: string, data?: Record<string, unknown>) => Promise<ComposioResult>;

/** The subset of `@composio/core`'s `Composio` client this module uses. */
export interface ComposioSdk {
  tools: {
    execute(
      slug: string,
      body: { userId?: string; connectedAccountId?: string; arguments: Record<string, unknown>; version?: string },
      options?: { signal?: AbortSignal },
    ): Promise<ComposioResult>;
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Map an observed Composio result to a bridge reply. */
export function toReply(result: unknown): HostReply {
  if (!isRecord(result) || typeof result.successful !== "boolean") {
    throw new Error("Composio returned a result without a boolean successful field");
  }
  if (result.successful) return { ok: true, value: result.data ?? null };
  const message = typeof result.error === "string" && result.error ? result.error : "Composio reported an unsuccessful execution";
  return { ok: false, error: { code: "composio_tool_failed", message } };
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Adapt the `execute` helper of `composio run` (verified with Composio CLI 0.6.0).
 * That helper throws on failure, with the parsed result attached when Composio
 * answered. Any other throw (no JSON output, a crashed CLI) is unobserved.
 */
export function runHelperExecutor(execute: RunHelperExecute): ComposioExecutor {
  return async (slug, args) => {
    try {
      return toReply(await execute(slug, args));
    } catch (error) {
      const detail = isRecord(error) ? error : {};
      if (isRecord(detail.result) && detail.result.successful === false) return toReply(detail.result);
      return { unobserved: `composio execute ended without a result: ${describe(error)}` };
    }
  };
}

/** `@composio/core` 0.28.0 errors raised before a tool executes. */
const SDK_REFUSALS = new Set([
  "ValidationError",
  "ComposioToolNotFoundError",
  "ComposioToolFetchError",
  "ComposioToolVersionRequiredError",
  "ComposioConnectedAccountNotFoundError",
  "ComposioToolExecutionRateLimitError",
]);

/**
 * Adapt an `@composio/core` client. `userId` selects whose connected accounts run the
 * tool; pin tool versions on the client (`toolkitVersions`) or pass `version`.
 * SDK errors raised before execution (validation, missing tool or account, rate
 * limit) become `composio_sdk_refused`; any other thrown error is unobserved.
 */
export function sdkExecutor(
  composio: ComposioSdk,
  options: { userId: string; connectedAccountId?: string; version?: string },
): ComposioExecutor {
  return async (slug, args, signal) => {
    const body = { userId: options.userId, arguments: args } as Parameters<ComposioSdk["tools"]["execute"]>[1];
    if (options.connectedAccountId) body.connectedAccountId = options.connectedAccountId;
    if (options.version) body.version = options.version;
    let result: ComposioResult;
    try {
      result = await composio.tools.execute(slug, body, { signal });
    } catch (error) {
      if (error instanceof Error && SDK_REFUSALS.has(error.name)) {
        return { ok: false, error: { code: "composio_sdk_refused", message: `${error.name}: ${error.message}` } };
      }
      return { unobserved: `Composio request ended without a result: ${describe(error)}` };
    }
    return toReply(result);
  };
}

/** One program run with Composio tools; limits and paths as for `runBridge`. */
export interface ComposioRun extends Omit<BridgeRun, "execute"> {
  executor: ComposioExecutor;
}

/** The Rust execution record, plus why the host cancelled the run, if it did. */
export interface ComposioRunResult {
  report: RunReport;
  /** Set when an unobserved execution made the host cancel the run. */
  hostDiagnostic?: string;
}

/** Run a program whose authorized tool calls execute through Composio. */
export async function runComposio(run: ComposioRun): Promise<ComposioRunResult> {
  const { executor, signal, ...bridge } = run;
  const controller = new AbortController();
  const forward = () => controller.abort();
  if (signal?.aborted) forward();
  signal?.addEventListener("abort", forward, { once: true });
  let hostDiagnostic: string | undefined;
  try {
    const report = await runBridge({
      ...bridge,
      signal: controller.signal,
      execute: async (call, callSignal) => {
        if (!isRecord(call.arguments)) {
          return { ok: false, error: { code: "invalid_arguments", message: "Composio tool arguments must be a JSON object" } };
        }
        let reply: HostReply | Unobserved;
        try {
          reply = await executor(call.tool, call.arguments, callSignal);
        } catch (error) {
          reply = { unobserved: describe(error) };
        }
        if (!("unobserved" in reply)) return reply;
        // Never answer: cancelling leaves this call `unknown` in the Rust record.
        hostDiagnostic ??= `call ${call.id} (${call.tool}): ${reply.unobserved}; run cancelled`;
        controller.abort();
        return new Promise<HostReply>(() => {});
      },
    });
    return hostDiagnostic === undefined ? { report } : { report, hostDiagnostic };
  } finally {
    signal?.removeEventListener("abort", forward);
  }
}
