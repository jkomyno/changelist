/**
 * Composio reads for `codemode bridge`: the investigation step of the GitHub
 * triage workflow.
 *
 * The guest calls GitHub read tools by slug, for example
 * `tools.call("GITHUB_LIST_REPOSITORY_ISSUES", { owner, repo, state: "open" })`.
 * Rust admits, schedules, and records each call; this module runs it in the
 * trusted host with an executor that holds the Composio credential. Nothing
 * Composio-specific exists in the Rust core.
 *
 * Boundaries:
 * - Credentials live only in the host process: the `composio run` helper context or an
 *   SDK client. The bridge starts with an empty environment (see bridge-client.ts).
 * - Only `READ_TOOLS` are authorized, so Rust refuses every write, `COMPOSIO_*`
 *   meta tool, and other slug with `tool_denied` before the host sees it. The
 *   guest has no `proxy` or sub-agent. A read whose `owner`/`repo` do not name
 *   the investigated repository is refused with `tool_denied` without calling Composio.
 * - Every read runs as one connected account, passed to the helper as `{ account }`.
 * - A guest-visible value is the tool's `data`, with signed download URLs
 *   replaced. A tool that reports `successful: false` becomes the call error
 *   `composio_tool_failed` with this module's fixed message, never Composio's or
 *   GitHub's text, which can name account ids. No call is retried.
 * - When an execution ends without an observed result, the bridge protocol has
 *   no way to say "unknown" for one call. The adapter leaves that call unanswered
 *   and cancels the run, so Rust records it as `unknown` instead of a failure the
 *   guest could act on, for example by retrying.
 * - Cancellation stops waiting for a call. The `composio run` helper cannot abort an
 *   in-flight execution; Rust records the call as unknown.
 */
import { type BridgeRun, type HostReply, type RunReport, runBridge } from "../shared/bridge-client.ts";

/** A Composio execution result, shared by the CLI `execute` helper and `@composio/core`. */
export interface ComposioResult {
  successful: boolean;
  data?: unknown;
  error?: string | null;
  /** Set by `composio run --dry-run`: nothing was sent. */
  dryRun?: boolean;
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

/** The `execute(slug, data, { account })` global that `composio run` injects into its scripts. */
export type RunHelperExecute = (slug: string, data: Record<string, unknown>, options: { account: string }) => Promise<ComposioResult>;

/** GitHub reads an investigation program may call; every one requires `owner` and `repo`. */
export const READ_TOOLS = [
  "GITHUB_LIST_REPOSITORY_ISSUES",
  "GITHUB_GET_AN_ISSUE",
  "GITHUB_LIST_LABELS_FOR_AN_ISSUE",
  "GITHUB_LIST_LABELS_FOR_A_REPOSITORY",
  "GITHUB_LIST_ISSUE_COMMENTS",
  "GITHUB_GET_AN_ISSUE_COMMENT",
  "GITHUB_LIST_ISSUE_EVENTS",
  "GITHUB_LIST_TIMELINE_EVENTS_FOR_AN_ISSUE",
  "GITHUB_LIST_WORKFLOW_RUNS_FOR_A_REPOSITORY",
  "GITHUB_GET_A_WORKFLOW_RUN",
  "GITHUB_LIST_JOBS_FOR_A_WORKFLOW_RUN",
  "GITHUB_DOWNLOAD_JOB_LOGS_FOR_A_WORKFLOW_RUN",
  "GITHUB_DOWNLOAD_WORKFLOW_RUN_LOGS",
  "GITHUB_LIST_CHECK_RUNS_FOR_A_REF",
  "GITHUB_LIST_CHECK_RUN_ANNOTATIONS",
  "GITHUB_GET_CHECK_RUN",
] as const;

/** The SDK client subset this module uses. */
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

/** Map an observed Composio result to a bridge reply, without Composio's or GitHub's text. */
export function toReply(result: unknown): HostReply {
  if (!isRecord(result) || typeof result.successful !== "boolean") {
    throw new Error("Composio returned a result without a boolean successful field");
  }
  if (result.dryRun === true) {
    return { ok: false, error: { code: "composio_dry_run", message: "the CLI previewed the call without executing it" } };
  }
  if (result.successful) return { ok: true, value: result.data ?? null };
  const status = isRecord(result.data) && typeof result.data.status_code === "number" ? ` (GitHub status ${result.data.status_code})` : "";
  return { ok: false, error: { code: "composio_tool_failed", message: `Composio reported an unsuccessful execution${status}` } };
}

/**
 * Adapt the `execute` helper of `composio run` (verified with Composio CLI 0.6.0)
 * for one connected account. The helper throws on failure, with the parsed
 * result attached when Composio answered. Any other throw (no JSON output, an
 * unmatched account, a crashed CLI) is unobserved; its text is not kept.
 */
export function runHelperExecutor(execute: RunHelperExecute, account: string): ComposioExecutor {
  return async (slug, args) => {
    try {
      return toReply(await execute(slug, args, { account }));
    } catch (error) {
      const detail = isRecord(error) ? error : {};
      if (isRecord(detail.result) && detail.result.successful === false) return toReply(detail.result);
      return { unobserved: "composio execute ended without a result" };
    }
  };
}

/** Query parameters that make a URL a signed, credential-bearing download link. */
const SIGNED = /(^|[-_])(signature|credential|sig|token|security-token)$/i;

/** `value` with every signed URL (an S3 or Azure download link, for example) replaced. */
export function stripSignedUrls(value: unknown): unknown {
  if (typeof value === "string") {
    if (!/^https?:\/\//i.test(value)) return value;
    try {
      const keys = [...new URL(value).searchParams.keys()];
      return keys.some((key) => SIGNED.test(key)) ? "[signed URL removed]" : value;
    } catch {
      return value;
    }
  }
  if (Array.isArray(value)) return value.map(stripSignedUrls);
  if (isRecord(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, stripSignedUrls(item)]));
  return value;
}

/**
 * Restrict `executor` to reads of one repository (lowercase `owner/repo`) and
 * strip signed URLs from what reaches the guest. A refused read never calls Composio.
 */
export function investigationExecutor(executor: ComposioExecutor, repo: string): ComposioExecutor {
  const tools: readonly string[] = READ_TOOLS;
  return async (slug, args, signal) => {
    const named = typeof args.owner === "string" && typeof args.repo === "string" ? `${args.owner}/${args.repo}`.toLowerCase() : undefined;
    if (!tools.includes(slug) || named !== repo) {
      return { ok: false, error: { code: "tool_denied", message: "investigation reads are limited to the --repo repository" } };
    }
    const reply = await executor(slug, args, signal);
    return "ok" in reply && reply.ok ? { ok: true, value: stripSignedUrls(reply.value) } : reply;
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
 * Adapt an `@composio/core` client, which needs a project API key. `userId`
 * selects whose connected accounts run the tool; pin tool versions on the client
 * (`toolkitVersions`) or pass `version`. SDK errors raised before execution
 * become `composio_sdk_refused`; any other thrown error is unobserved.
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
        return { ok: false, error: { code: "composio_sdk_refused", message: error.name } };
      }
      return { unobserved: "the Composio request ended without a result" };
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
      // No executor_started fact: the CLI and the SDK can both refuse a call
      // before the tool runs, so this adapter cannot tell when it starts.
      execute: async (call, callSignal) => {
        if (!isRecord(call.arguments)) {
          return { ok: false, error: { code: "invalid_arguments", message: "Composio tool arguments must be a JSON object" } };
        }
        let reply: HostReply | Unobserved;
        try {
          reply = await executor(call.tool, call.arguments, callSignal);
        } catch {
          reply = { unobserved: "the executor failed" };
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
