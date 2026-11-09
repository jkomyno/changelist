/**
 * Optional Composio provider for recoverable plan sessions
 * (`codemode plan run|resume --bridge`), for one operation family: add one
 * existing label to one GitHub issue, through the `composio run` helper.
 *
 * Rust keeps the journal and decides what may happen next; this module is the
 * trusted host that authorizes, executes, and reconciles attempts with the
 * credential the Composio CLI holds. Nothing Composio-specific exists in Rust.
 *
 * Contract (`GITHUB_ADD_LABELS_TO_AN_ISSUE`): `repeat: unsafe`, `reconcile: state`.
 * - A complete, successful read of the issue's labels that contains the label
 *   is `satisfied`: the state holds, and who caused it is unknown.
 * - A label that is absent is `unknown`, never `not_applied`: an earlier request
 *   may still apply. Nothing is sent again automatically.
 * - A failed, malformed, or incomplete read is a probe error: still unknown.
 * - Only this module's own checks before the mutation is sent can refuse an
 *   attempt (`refused`), or report one that was never sent (`not_applied`). Any
 *   failure after the mutation was handed to the CLI is `unknown`. Error text is
 *   never classified by substring.
 * - No receipts, idempotency keys, atomic preconditions, or remote cancellation
 *   exist here: GitHub's label endpoint offers none, and the attempt key is not
 *   sent anywhere. Plans with preconditions are refused before any call.
 *
 * Account and destination: every call (reads included) passes the approved
 * connected-account id to the helper as `{ account }`; the CLI fails an
 * unmatched selector instead of falling back to a default. Each operation's
 * scope must name that account and an allowlisted `owner/repo`, and its frozen
 * arguments must name the same repository.
 *
 * Retention: the declaration says `summary/1`. Replies carry only this module's
 * fixed messages and evidence, codes, and a small projected value; raw Composio
 * results, GitHub error text, and label or account details never cross the bridge.
 */
import type { PlanCall, PlanReply, ProviderDeclaration, Reconciled, ReconcileRequest } from "../shared/bridge-client.ts";
import type { ComposioResult } from "./composio-provider.ts";

/** The only mutation this adapter performs. */
export const ADD_LABELS = "GITHUB_ADD_LABELS_TO_AN_ISSUE";
/** State read for reconciliation. */
export const LIST_ISSUE_LABELS = "GITHUB_LIST_LABELS_FOR_AN_ISSUE";
/** Label existence check before a mutation: GitHub creates a missing label otherwise. */
export const GET_LABEL = "GITHUB_GET_LABEL";
/** Per-session authorization check: the account can still triage the repository. */
export const GET_REPOSITORY = "GITHUB_GET_A_REPOSITORY";
/** Provider name in the declaration and in every operation's scope. */
export const PROVIDER = "composio.github";

/** GitHub's largest page for issue labels. */
const PAGE_SIZE = 100;
/** Pages read before a probe gives up as incomplete. */
const MAX_PAGES = 10;
const ARGUMENT_KEYS = ["issue_number", "labels", "owner", "repo"];

/** The `execute` helper of `composio run` (verified with Composio CLI 0.6.0), with account selection. */
export type AccountExecute = (slug: string, data: Record<string, unknown>, options: { account: string }) => Promise<ComposioResult>;

/** Host-owned settings; never taken from a plan or a model. */
export interface ComposioPlanConfig {
  /** The stable connected-account id (not an alias) every call runs as. */
  account: string;
  /** Repositories, as `owner/repo`, that operations may name. */
  destinations: string[];
}

/** The declaration to approve plans with, and to start sessions with. */
export function declaration(config: ComposioPlanConfig): ProviderDeclaration {
  return {
    provider: PROVIDER,
    account: config.account,
    contracts: { [ADD_LABELS]: { repeat: "unsafe", reconcile: "state" } },
    retention: "summary/1",
  };
}

/** Read and check a host config. */
export function parseConfig(value: unknown): ComposioPlanConfig {
  const record = value as Record<string, unknown> | null;
  const account = record?.account;
  const destinations = record?.destinations;
  if (typeof account !== "string" || !/^[A-Za-z0-9_-]{4,128}$/.test(account)) {
    throw new Error("config.account must be a connected-account id");
  }
  if (!Array.isArray(destinations) || destinations.length === 0 || !destinations.every((item) => typeof item === "string" && canonical(item) !== undefined)) {
    throw new Error("config.destinations must list repositories as owner/repo");
  }
  const extra = Object.keys(record ?? {}).filter((key) => key !== "account" && key !== "destinations");
  if (extra.length > 0) throw new Error(`unexpected config keys: ${extra.join(", ")}`);
  return { account, destinations: destinations.map((item) => canonical(item)!) };
}

/** `owner/repo` lowercased (GitHub names are case-insensitive), or undefined if malformed. */
function canonical(destination: string): string | undefined {
  return /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/.test(destination) ? destination.toLowerCase() : undefined;
}

interface Target {
  owner: string;
  repo: string;
  issue: number;
  label: string;
}

type Scope = PlanCall["scope"];

class Refusal extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}

/** Check scope against the host config: provider, exact account, allowlisted destination. */
function checkScope(scope: Scope, config: ComposioPlanConfig): string {
  if (scope?.provider !== PROVIDER) throw new Refusal("provider_mismatch", "the scope names another provider");
  if (scope.account !== config.account) throw new Refusal("account_mismatch", "the scope names another account than the approved one");
  const destination = typeof scope.destination === "string" ? canonical(scope.destination) : undefined;
  if (!destination || !config.destinations.includes(destination)) {
    throw new Refusal("destination_not_allowed", "the scope's destination is not an allowlisted owner/repo");
  }
  return destination;
}

/** Check one operation against everything this adapter supports; nothing is called. */
export function target(tool: string, args: Record<string, unknown>, scope: Scope, preconditions: unknown[], config: ComposioPlanConfig): Target {
  if (tool !== ADD_LABELS) throw new Refusal("unsupported_tool", "only GITHUB_ADD_LABELS_TO_AN_ISSUE is supported");
  const destination = checkScope(scope, config);
  if (preconditions.length > 0) {
    throw new Refusal("unsupported_precondition", "GitHub cannot enforce a precondition atomically with adding a label");
  }
  const keys = Object.keys(args ?? {}).sort();
  if (keys.join() !== ARGUMENT_KEYS.join()) {
    throw new Refusal("unsupported_arguments", "arguments must be exactly owner, repo, issue_number, and labels");
  }
  const { owner, repo, issue_number: issue, labels } = args as { owner: unknown; repo: unknown; issue_number: unknown; labels: unknown };
  if (typeof owner !== "string" || typeof repo !== "string" || canonical(`${owner}/${repo}`) !== destination) {
    throw new Refusal("destination_mismatch", "owner and repo must name the scope's destination");
  }
  if (typeof issue !== "number" || !Number.isSafeInteger(issue) || issue < 1) {
    throw new Refusal("invalid_issue", "issue_number must be a positive integer");
  }
  if (!Array.isArray(labels) || labels.length !== 1 || typeof labels[0] !== "string" || labels[0].trim() === "") {
    throw new Refusal("unsupported_labels", "labels must hold exactly one label name");
  }
  return { owner, repo, issue, label: labels[0] };
}

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);

/**
 * What the helper produced: a result Composio reported (successful or not), or
 * nothing observed. The helper throws for unsuccessful results with `result`
 * attached; any other throw means no result was observed.
 */
export type Observation = { result: ComposioResult } | { unobserved: true };

export async function observe(execute: AccountExecute, slug: string, data: Record<string, unknown>, account: string): Promise<Observation> {
  try {
    const result = await execute(slug, data, { account });
    return isRecord(result) && typeof result.successful === "boolean" ? { result } : { unobserved: true };
  } catch (error) {
    const result = isRecord(error) ? error.result : undefined;
    return isRecord(result) && typeof result.successful === "boolean" ? { result: result as unknown as ComposioResult } : { unobserved: true };
  }
}

/** GitHub's HTTP status from an unsuccessful Composio result, if it states one. */
function status(result: ComposioResult): number | undefined {
  const data = result.data;
  return isRecord(data) && typeof data.status_code === "number" ? data.status_code : undefined;
}

const sameName = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** A host for one plan session through the Composio CLI helper. */
export interface ComposioPlanHost {
  authorize(scope: Scope): Promise<{ ok: true } | { ok: false; error: { code: string; message: string } }>;
  execute(call: PlanCall, signal: AbortSignal): Promise<PlanReply>;
  reconcile(request: ReconcileRequest, signal: AbortSignal): Promise<Reconciled>;
}

/** Build the host's three handlers over `execute`, bound to `config`. */
export function composioPlanHost(execute: AccountExecute, config: ComposioPlanConfig): ComposioPlanHost {
  const account = config.account;
  const refused = (error: unknown): PlanReply => {
    const refusal = error instanceof Refusal ? error : new Refusal("invalid_operation", "the operation is malformed");
    return { ok: false, error: { code: refusal.code, message: refusal.message }, effect: "refused" };
  };
  return {
    async authorize(scope) {
      let destination: string;
      try {
        destination = checkScope(scope, config);
      } catch (error) {
        const refusal = error as Refusal;
        return { ok: false, error: { code: refusal.code, message: refusal.message } };
      }
      const [owner, repo] = destination.split("/");
      const seen = await observe(execute, GET_REPOSITORY, { owner, repo }, account);
      if (!("result" in seen) || !seen.result.successful) {
        return { ok: false, error: { code: "authorization_unverified", message: "the account could not read the repository now" } };
      }
      const permissions = isRecord(seen.result.data) ? seen.result.data.permissions : undefined;
      const allowed = isRecord(permissions) && (permissions.triage === true || permissions.push === true);
      return allowed ? { ok: true } : { ok: false, error: { code: "not_permitted", message: "the account cannot label issues in this repository" } };
    },

    async execute(call, signal) {
      let op: Target;
      try {
        op = target(call.tool, call.arguments, call.scope, call.preconditions, config);
      } catch (error) {
        return refused(error);
      }
      // GitHub creates a label that does not exist; refuse instead. This read is
      // not atomic with the mutation: the label can still be deleted in between.
      const label = await observe(execute, GET_LABEL, { owner: op.owner, repo: op.repo, name: op.label }, account);
      if ("result" in label && !label.result.successful && status(label.result) === 404) {
        return { ok: false, error: { code: "label_missing", message: "the label does not exist in the repository" }, effect: "refused" };
      }
      const found = "result" in label && label.result.successful && isRecord(label.result.data) && typeof label.result.data.name === "string" && sameName(label.result.data.name, op.label);
      if (!found) {
        return { ok: false, error: { code: "label_unverified", message: "the label's existence could not be confirmed; nothing was sent" }, effect: "not_applied" };
      }
      if (signal.aborted) {
        return { ok: false, error: { code: "cancelled", message: "cancelled before the mutation was sent" }, effect: "not_applied" };
      }
      // From here the mutation may run; the helper cannot be aborted, and nothing is retried.
      const added = await observe(execute, ADD_LABELS, { owner: op.owner, repo: op.repo, issue_number: op.issue, labels: [op.label] }, account);
      if (!("result" in added)) {
        return { ok: false, error: { code: "composio_unobserved", message: "the CLI ended without a result; the label may have been added" }, effect: "unknown" };
      }
      if (!added.result.successful) {
        const http = status(added.result);
        const code = http === undefined ? "composio_tool_failed" : `github_status_${http}`;
        return { ok: false, error: { code, message: "Composio reported an unsuccessful execution; the label may have been added" }, effect: "unknown" };
      }
      return { ok: true, value: { label_added: true } };
    },

    async reconcile(request) {
      let op: Target;
      try {
        op = target(request.tool, request.arguments, request.scope, [], config);
      } catch (error) {
        return { error: { code: (error as Refusal).code ?? "invalid_operation", message: "the operation cannot be reconciled by this host" } };
      }
      for (let page = 1; page <= MAX_PAGES; page++) {
        const read = await observe(execute, LIST_ISSUE_LABELS, { owner: op.owner, repo: op.repo, issue_number: op.issue, per_page: PAGE_SIZE, page }, account);
        if (!("result" in read) || !read.result.successful) {
          return { error: { code: "state_read_failed", message: "the issue's labels could not be read" } };
        }
        const labels = isRecord(read.result.data) ? read.result.data.labels : undefined;
        if (!Array.isArray(labels) || !labels.every((item) => isRecord(item) && typeof item.name === "string")) {
          return { error: { code: "state_malformed", message: "the issue's labels had an unexpected shape" } };
        }
        if (labels.some((item) => sameName((item as { name: string }).name, op.label))) {
          return { verdict: "satisfied", evidence: "the label is on the issue; who added it is unknown" };
        }
        if (labels.length < PAGE_SIZE) {
          return { verdict: "unknown", evidence: "the label is absent; an earlier request may still apply" };
        }
      }
      return { error: { code: "state_incomplete", message: `the issue has more than ${MAX_PAGES * PAGE_SIZE} labels` } };
    },
  };
}
