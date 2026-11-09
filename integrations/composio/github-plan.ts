/**
 * The GitHub issue-triage provider for recoverable plan sessions
 * (`codemode plan run|resume --bridge`), through the `composio run` helper.
 *
 * Rust keeps the journal and decides what may happen next; this module is the
 * trusted host that authorizes, executes, and reconciles attempts with the
 * credential the Composio CLI holds. Nothing Composio-specific exists in Rust.
 *
 * Contracts (every write is `repeat: unsafe`; nothing is ever sent twice automatically):
 * - `GITHUB_ADD_LABELS_TO_AN_ISSUE`, one existing label: reconcile by state.
 *   The label on the issue is `satisfied`; absent is `unknown`.
 * - `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE`: reconcile by state. The label absent
 *   is `satisfied`; present is `unknown`. A 404 after dispatch stays unknown until probed.
 * - `GITHUB_UPDATE_AN_ISSUE`, only `state` and `state_reason`: reconcile by state.
 * - `GITHUB_CREATE_AN_ISSUE_COMMENT`: reconcile by receipt. The body's final line
 *   is the marker `<!-- codemode:<plan id>:<operation id> -->`; a comment whose
 *   final line is that marker and whose author is the approved account's GitHub
 *   login is `applied`. No such comment is a probe error, never `not_applied`:
 *   the request may still land. A marked comment by another author is recorded
 *   as review evidence and settles nothing.
 *
 * State verdicts never attribute the effect. A failed, malformed, or incomplete
 * read is a probe error. Only checks made before a write is handed to the CLI
 * can refuse an attempt (`refused`) or report it as never sent (`not_applied`);
 * any failure after that is `unknown`. Error text is never classified by substring.
 *
 * Preconditions: an operation may carry `{"issue_state": "open"|"closed"}`. The
 * host reads the issue before each attempt and refuses it, sending nothing, when
 * the state differs. GitHub offers no conditional write, so the check is not
 * atomic with the write.
 *
 * Identity: the declaration's account is `<connected account id>:<GitHub login>`
 * and each operation's destination is `<owner>/<repo>:<repository id>`, so the
 * approval binds the login and the numeric repository. Each session re-resolves
 * both and refuses on any drift. Every call passes the connected-account id to
 * the helper as `{ account }`; the CLI fails an unmatched selector instead of
 * falling back to a default.
 *
 * Denied: `GITHUB_SET_LABELS_FOR_AN_ISSUE`, any other `GITHUB_UPDATE_AN_ISSUE`
 * field, and every other tool, including `COMPOSIO_*` meta tools. A result with
 * `dryRun: true` means nothing was sent: the attempt is `not_applied` and the
 * host ends the session.
 *
 * Retention: `summary/1`. Replies carry only this module's fixed messages,
 * evidence, codes, and a small projected value; raw Composio results, GitHub
 * error text, and account details never cross the bridge.
 */
import type { PlanCall, PlanReply, ProviderDeclaration, Reconciled, ReconcileRequest } from "../shared/bridge-client.ts";
import type { ComposioResult } from "./composio-provider.ts";

export const ADD_LABELS = "GITHUB_ADD_LABELS_TO_AN_ISSUE";
export const REMOVE_LABEL = "GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE";
export const UPDATE_ISSUE = "GITHUB_UPDATE_AN_ISSUE";
export const CREATE_COMMENT = "GITHUB_CREATE_AN_ISSUE_COMMENT";
/** The writes a plan may contain. */
export const WRITE_TOOLS = [ADD_LABELS, REMOVE_LABEL, UPDATE_ISSUE, CREATE_COMMENT] as const;

const LIST_ISSUE_LABELS = "GITHUB_LIST_LABELS_FOR_AN_ISSUE";
const LIST_COMMENTS = "GITHUB_LIST_ISSUE_COMMENTS";
const GET_ISSUE = "GITHUB_GET_AN_ISSUE";
/** Label existence: GitHub creates a missing label when adding one. */
export const GET_LABEL = "GITHUB_GET_LABEL";
/** The repository's numeric id and the account's permission on it. */
export const GET_REPOSITORY = "GITHUB_GET_A_REPOSITORY";
/** The GitHub login a connected account acts as. */
export const GET_USER = "GITHUB_GET_THE_AUTHENTICATED_USER";
/** Provider name in the declaration and in every operation's scope. */
export const PROVIDER = "composio.github";

/** GitHub's largest page. */
const PAGE_SIZE = 100;
/** Pages read before a probe gives up as incomplete. */
const MAX_PAGES = 10;
const MARKER = /^<!-- codemode:([a-z0-9]{8,64}):([A-Za-z0-9._-]{1,64}) -->$/;
const ACCOUNT_ID = /^ca_[A-Za-z0-9_-]{4,124}$/;
const REPOSITORY = /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/;

/** The `execute` helper of `composio run` (verified with Composio CLI 0.6.0), with account selection. */
export type AccountExecute = (slug: string, data: Record<string, unknown>, options: { account: string }) => Promise<ComposioResult>;

/** Host-owned settings from the command line; never taken from a plan or a model. */
export interface Target {
  /** The stable connected-account id (`ca_...`, not an alias) every call runs as. */
  account: string;
  /** The only repository, as lowercase `owner/repo`. */
  repo: string;
}

/** A target plus the identity it resolved to and an approval binds. */
export interface Identity extends Target {
  /** The GitHub login the connected account acts as. */
  login: string;
}

class Refusal extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}

/** Check `--account` and `--repo` before anything is called. */
export function parseTarget(account: string | undefined, repo: string | undefined): Target {
  // The CLI also resolves aliases and word ids, which can be reassigned; only the id is stable.
  if (!account || !ACCOUNT_ID.test(account)) {
    throw new Error("--account must be a connected-account id (ca_...), not an alias or word id");
  }
  if (!repo || !REPOSITORY.test(repo)) throw new Error("--repo must name one repository as owner/repo");
  return { account, repo: repo.toLowerCase() };
}

/** The scope account an approval binds: the connected account and its GitHub login. */
export const scopeAccount = (identity: Identity) => `${identity.account}:${identity.login}`;

/** The declaration to approve plans with, and to start sessions with. */
export function declaration(identity: Identity): ProviderDeclaration {
  return {
    provider: PROVIDER,
    account: scopeAccount(identity),
    contracts: {
      [ADD_LABELS]: { repeat: "unsafe", reconcile: "state" },
      [REMOVE_LABEL]: { repeat: "unsafe", reconcile: "state" },
      [UPDATE_ISSUE]: { repeat: "unsafe", reconcile: "state" },
      [CREATE_COMMENT]: { repeat: "unsafe", reconcile: "receipt" },
    },
    retention: "summary/1",
  };
}

/** The identity an approved declaration names, checked against `--account` and `--repo`. */
export function identityFromDeclaration(value: unknown, target: Target): Identity {
  const record = value as Partial<ProviderDeclaration> | null;
  const [account, login, extra] = typeof record?.account === "string" ? record.account.split(":") : [];
  if (record?.provider !== PROVIDER || account !== target.account || !login || extra !== undefined) {
    throw new Error("the declaration is not this host's declaration for --account");
  }
  const identity = { ...target, login };
  if (canonical(declaration(identity)) !== canonical(record)) {
    throw new Error("the declaration's contracts or retention differ from this host's");
  }
  return identity;
}

/** JSON with object keys sorted, for comparing values regardless of key order. */
function canonical(value: unknown): string {
  return JSON.stringify(value, (_key, item: unknown) =>
    isRecord(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) : item);
}

/** The destination an operation names: the repository and its numeric id. */
export const destination = (repo: string, id: number) => `${repo}:${id}`;

/** Each plan operation's validated form. */
export type Write =
  | { kind: "add" | "remove"; issue: number; label: string }
  | { kind: "state"; issue: number; state: "open" | "closed"; reason?: string }
  | { kind: "comment"; issue: number; marker: string };

interface Checked {
  write: Write;
  repoId: number;
  /** The required issue state, if the operation has a precondition. */
  issueState?: "open" | "closed";
}

type Scope = PlanCall["scope"];

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const sameName = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Check scope: provider, the exact account and login, and `--repo` with an id. */
function checkScope(scope: Scope, identity: Identity): number {
  if (scope?.provider !== PROVIDER) throw new Refusal("provider_mismatch", "the scope names another provider");
  if (scope.account !== scopeAccount(identity)) {
    throw new Refusal("account_mismatch", "the scope names another account or GitHub login than the approved one");
  }
  const match = typeof scope.destination === "string" ? /^(.+):([1-9][0-9]{0,15})$/.exec(scope.destination) : null;
  if (!match || match[1].toLowerCase() !== identity.repo) {
    throw new Refusal("destination_not_allowed", "the scope's destination is not --repo with its repository id");
  }
  return Number(match[2]);
}

function exactKeys(args: Record<string, unknown>, required: string[], optional: string[] = []): void {
  const keys = Object.keys(args);
  if (!required.every((key) => key in args) || !keys.every((key) => required.includes(key) || optional.includes(key))) {
    const allowed = [...required, ...optional.map((key) => `optional ${key}`)].join(", ");
    throw new Refusal("unsupported_arguments", `arguments must be exactly ${allowed}`);
  }
}

/** The marker a plan's create-comment operation ends with. */
export const marker = (planId: string, operation: string) => `<!-- codemode:${planId}:${operation} -->`;

/** Check one operation against everything this host supports; nothing is called. */
export function check(operation: string, tool: string, args: Record<string, unknown>, scope: Scope, preconditions: unknown[], identity: Identity): Checked {
  const repoId = checkScope(scope, identity);
  if (!isRecord(args)) throw new Refusal("unsupported_arguments", "arguments must be an object");
  const { owner, repo, issue_number: issue } = args;
  let write: Write;
  switch (tool) {
    case ADD_LABELS: {
      exactKeys(args, ["owner", "repo", "issue_number", "labels"]);
      const labels = args.labels;
      if (!Array.isArray(labels) || labels.length !== 1 || typeof labels[0] !== "string" || labels[0].trim() === "") {
        throw new Refusal("unsupported_labels", "labels must hold exactly one label name");
      }
      write = { kind: "add", issue: issue as number, label: labels[0] };
      break;
    }
    case REMOVE_LABEL:
      exactKeys(args, ["owner", "repo", "issue_number", "name"]);
      if (typeof args.name !== "string" || args.name.trim() === "") throw new Refusal("unsupported_labels", "name must be a label name");
      write = { kind: "remove", issue: issue as number, label: args.name };
      break;
    case UPDATE_ISSUE: {
      exactKeys(args, ["owner", "repo", "issue_number", "state"], ["state_reason"]);
      const state = args.state;
      const reason = args.state_reason;
      if (state !== "open" && state !== "closed") throw new Refusal("unsupported_state", "state must be open or closed");
      const reasons = state === "closed" ? ["completed", "not_planned"] : ["reopened"];
      if (reason !== undefined && !reasons.includes(reason as string)) {
        throw new Refusal("unsupported_state", `state_reason for ${state} must be ${reasons.join(" or ")}`);
      }
      write = reason === undefined ? { kind: "state", issue: issue as number, state } : { kind: "state", issue: issue as number, state, reason: reason as string };
      break;
    }
    case CREATE_COMMENT: {
      exactKeys(args, ["owner", "repo", "issue_number", "body"]);
      const lines = typeof args.body === "string" ? args.body.split("\n") : [];
      const last = lines[lines.length - 1] ?? "";
      const found = MARKER.exec(last);
      if (!found || found[2] !== operation) {
        throw new Refusal("comment_unmarked", "the body's final line must be this operation's <!-- codemode:<plan id>:<operation id> --> marker");
      }
      write = { kind: "comment", issue: issue as number, marker: last };
      break;
    }
    default:
      throw new Refusal("unsupported_tool", `${tool} is not one of the supported issue writes`);
  }
  if (typeof owner !== "string" || typeof repo !== "string" || `${owner}/${repo}`.toLowerCase() !== identity.repo) {
    throw new Refusal("destination_mismatch", "owner and repo must name --repo");
  }
  if (typeof issue !== "number" || !Number.isSafeInteger(issue) || issue < 1) {
    throw new Refusal("invalid_issue", "issue_number must be a positive integer");
  }
  if (preconditions.length > 1) throw new Refusal("unsupported_precondition", "at most one issue_state precondition");
  const [condition] = preconditions;
  if (condition === undefined) return { write, repoId };
  if (!isRecord(condition) || Object.keys(condition).join() !== "issue_state" || (condition.issue_state !== "open" && condition.issue_state !== "closed")) {
    throw new Refusal("unsupported_precondition", 'the only precondition is {"issue_state": "open"|"closed"}');
  }
  return { write, repoId, issueState: condition.issue_state };
}

/** A refusal's code, for reports. */
export const refusalCode = (error: unknown) => (error instanceof Refusal ? error.code : "invalid_operation");

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

/** The data of a successful, real (not dry-run) result, or undefined. */
function data(seen: Observation): Record<string, unknown> | undefined {
  if (!("result" in seen) || !seen.result.successful || seen.result.dryRun === true) return undefined;
  return isRecord(seen.result.data) ? seen.result.data : undefined;
}

/** GitHub's HTTP status from an unsuccessful Composio result, if it states one. */
function status(result: ComposioResult): number | undefined {
  return isRecord(result.data) && typeof result.data.status_code === "number" ? result.data.status_code : undefined;
}

/** The GitHub login and the repository's id and permission, read as `target.account`. */
export async function resolve(execute: AccountExecute, target: Target): Promise<{ login: string; repoId: number; canTriage: boolean } | { error: string }> {
  const [owner, repo] = target.repo.split("/");
  const [user, repository] = await Promise.all([
    observe(execute, GET_USER, {}, target.account),
    observe(execute, GET_REPOSITORY, { owner, repo }, target.account),
  ]);
  const login = data(user)?.login;
  if (typeof login !== "string" || login === "") return { error: "account_unverified" };
  const found = data(repository);
  if (!found || !Number.isSafeInteger(found.id) || (found.id as number) < 1) return { error: "repository_unverified" };
  const permissions = found.permissions;
  const canTriage = isRecord(permissions) && (permissions.triage === true || permissions.push === true);
  return { login, repoId: found.id as number, canTriage };
}

/** One connected GitHub account as `composio connections list` reports it. */
export interface Connection {
  id: string;
  status: string;
  alias?: string | null;
  word_id?: string | null;
}

/** Lists the connected GitHub accounts now. */
export type ListConnections = () => Promise<Connection[]>;

/**
 * Why `account` cannot be trusted to select exactly that account, or undefined.
 * The CLI resolves a selector as an active account id, then as an alias or word
 * id, so an inactive id that another connection uses as its alias would run as
 * that other account. This is checked per session, not atomically with each call.
 */
export function accountProblem(account: string, connections: Connection[]): { code: string; message: string } | undefined {
  if (!connections.some((item) => item.id === account && item.status === "ACTIVE")) {
    return { code: "account_inactive", message: "the approved connected account is not active" };
  }
  if (connections.some((item) => item.id !== account && (item.alias === account || item.word_id === account))) {
    return { code: "account_ambiguous", message: "another connection uses the approved account id as its alias or word id" };
  }
  return undefined;
}

/** A host for one plan session through the Composio CLI helper. */
export interface GithubPlanHost {
  authorize(scope: Scope): Promise<{ ok: true } | { ok: false; error: { code: string; message: string } }>;
  execute(call: PlanCall, signal: AbortSignal): Promise<PlanReply>;
  reconcile(request: ReconcileRequest, signal: AbortSignal): Promise<Reconciled>;
}

const failed = (code: string, message: string) => ({ error: { code, message } });

/**
 * Build the host's three handlers over `execute`, bound to `identity`.
 * `endSession` is called after a dry-run result, which proves nothing was sent.
 */
export function githubPlanHost(execute: AccountExecute, identity: Identity, connections: ListConnections, endSession: () => void): GithubPlanHost {
  const account = identity.account;
  const [owner, repo] = identity.repo.split("/");
  const read = (slug: string, args: Record<string, unknown>) => observe(execute, slug, { owner, repo, ...args }, account);
  const refused = (code: string, message: string): PlanReply => ({ ok: false, error: { code, message }, effect: "refused" });
  const notSent = (code: string, message: string): PlanReply => ({ ok: false, error: { code, message }, effect: "not_applied" });

  /** The names of an issue's labels across pages, or a probe error code. */
  async function issueLabels(issue: number): Promise<string[] | string> {
    const names: string[] = [];
    for (let page = 1; page <= MAX_PAGES; page++) {
      const labels = data(await read(LIST_ISSUE_LABELS, { issue_number: issue, per_page: PAGE_SIZE, page }))?.labels;
      if (labels === undefined) return "state_read_failed";
      if (!Array.isArray(labels) || !labels.every((item) => isRecord(item) && typeof item.name === "string")) return "state_malformed";
      names.push(...labels.map((item) => (item as { name: string }).name));
      if (labels.length < PAGE_SIZE) return names;
    }
    return "state_incomplete";
  }

  /** The issue's state and reason, or a probe error code. */
  async function issueState(issue: number): Promise<{ state: string; reason: unknown } | string> {
    const found = data(await read(GET_ISSUE, { issue_number: issue }));
    if (!found) return "state_read_failed";
    if (found.state !== "open" && found.state !== "closed") return "state_malformed";
    return { state: found.state, reason: found.state_reason ?? null };
  }

  /** Comments whose final line is `marker`, as their authors' logins, or a probe error code. */
  async function markedBy(issue: number, wanted: string): Promise<string[] | string> {
    const authors: string[] = [];
    for (let page = 1; page <= MAX_PAGES; page++) {
      const comments = data(await read(LIST_COMMENTS, { issue_number: issue, per_page: PAGE_SIZE, page }))?.comments;
      if (comments === undefined) return "comments_read_failed";
      if (!Array.isArray(comments) || !comments.every((item) => isRecord(item) && typeof item.body === "string" && isRecord(item.user) && typeof item.user.login === "string")) {
        return "comments_malformed";
      }
      for (const comment of comments as Array<{ body: string; user: { login: string } }>) {
        const lines = comment.body.replace(/\s+$/, "").split("\n");
        if (lines[lines.length - 1] === wanted) authors.push(comment.user.login);
      }
      if (comments.length < PAGE_SIZE) return authors;
    }
    return "comments_incomplete";
  }

  return {
    async authorize(scope) {
      let repoId: number;
      try {
        repoId = checkScope(scope, identity);
      } catch (error) {
        const refusal = error as Refusal;
        return { ok: false, error: { code: refusal.code, message: refusal.message } };
      }
      let listed: Connection[];
      try {
        listed = await connections();
      } catch {
        return { ok: false, error: { code: "account_unverified", message: "the connected accounts could not be listed" } };
      }
      const problem = accountProblem(account, listed);
      if (problem) return { ok: false, error: problem };
      const now = await resolve(execute, identity);
      if ("error" in now) return { ok: false, error: { code: now.error, message: "the account or repository could not be read now" } };
      if (now.login !== identity.login) {
        return { ok: false, error: { code: "account_changed", message: "the connected account now acts as another GitHub login than the approved one" } };
      }
      if (now.repoId !== repoId) {
        return { ok: false, error: { code: "repository_changed", message: "the repository name now names another repository than the approved one" } };
      }
      return now.canTriage ? { ok: true } : { ok: false, error: { code: "not_permitted", message: "the account cannot triage issues in this repository" } };
    },

    async execute(call, signal) {
      let checked: Checked;
      try {
        checked = check(call.operation, call.tool, call.arguments, call.scope, call.preconditions, identity);
      } catch (error) {
        return refused(refusalCode(error), error instanceof Error ? error.message : "the operation is malformed");
      }
      const { write } = checked;
      if (checked.issueState) {
        // Not atomic with the write: the issue can change in between.
        const now = await issueState(write.issue);
        if (typeof now === "string") return notSent("precondition_unverified", "the issue's state could not be read; nothing was sent");
        if (now.state !== checked.issueState) {
          return refused("precondition_failed", `the issue is ${now.state}, not ${checked.issueState}; nothing was sent`);
        }
      }
      if (write.kind === "add") {
        // GitHub creates a label that does not exist; refuse instead. Not atomic either.
        const label = await read(GET_LABEL, { name: write.label });
        if ("result" in label && !label.result.successful && status(label.result) === 404) {
          return refused("label_missing", "the label does not exist in the repository");
        }
        const name = data(label)?.name;
        if (typeof name !== "string" || !sameName(name, write.label)) {
          return notSent("label_unverified", "the label's existence could not be confirmed; nothing was sent");
        }
      }
      if (signal.aborted) return notSent("cancelled", "cancelled before the write was sent");
      // From here the write may run; the helper cannot be aborted, and nothing is retried.
      const sent = await observe(execute, call.tool, call.arguments, account);
      if (!("result" in sent)) {
        return { ok: false, error: { code: "composio_unobserved", message: "the CLI ended without a result; the write may have applied" }, effect: "unknown" };
      }
      if (sent.result.dryRun === true) {
        // A preview, not an execution: end the session rather than report success.
        setTimeout(endSession, 0);
        return notSent("composio_dry_run", "the CLI previewed the write without sending it; the session ends");
      }
      if (!sent.result.successful) {
        const http = status(sent.result);
        const code = http === undefined ? "composio_tool_failed" : `github_status_${http}`;
        return { ok: false, error: { code, message: "Composio reported an unsuccessful execution; the write may have applied" }, effect: "unknown" };
      }
      const value = data(sent);
      switch (write.kind) {
        case "add":
          return { ok: true, value: { label_added: true } };
        case "remove":
          return { ok: true, value: { label_removed: true } };
        case "state":
          return { ok: true, value: { state: write.state } };
        case "comment":
          return { ok: true, value: { comment_id: typeof value?.id === "number" ? value.id : null } };
      }
    },

    async reconcile(request) {
      let write: Write;
      try {
        ({ write } = check(request.operation, request.tool, request.arguments, request.scope, [], identity));
      } catch (error) {
        return failed(refusalCode(error), "the operation cannot be reconciled by this host");
      }
      switch (write.kind) {
        case "add":
        case "remove": {
          const labels = await issueLabels(write.issue);
          if (typeof labels === "string") return failed(labels, "the issue's labels could not be read completely");
          const present = labels.some((name) => sameName(name, write.label));
          if (present === (write.kind === "add")) {
            return { verdict: "satisfied", evidence: `the label is ${present ? "on" : "absent from"} the issue; who changed it is unknown` };
          }
          return { verdict: "unknown", evidence: `the label is ${present ? "still on" : "absent from"} the issue; an earlier request may still apply` };
        }
        case "state": {
          const now = await issueState(write.issue);
          if (typeof now === "string") return failed(now, "the issue's state could not be read");
          const holds = now.state === write.state && (write.reason === undefined || now.reason === write.reason);
          return holds
            ? { verdict: "satisfied", evidence: `the issue is ${write.state}; who changed it is unknown` }
            : { verdict: "unknown", evidence: "the issue's state differs; an earlier request may still apply" };
        }
        case "comment": {
          const authors = await markedBy(write.issue, write.marker);
          if (typeof authors === "string") return failed(authors, "the issue's comments could not be read completely");
          const ours = authors.filter((login) => sameName(login, identity.login)).length;
          if (ours > 0) {
            return { verdict: "applied", evidence: `${ours} comment${ours === 1 ? " by the approved login carries" : "s by the approved login carry"} this operation's marker` };
          }
          if (authors.length > 0) {
            return { verdict: "unknown", evidence: "a comment by another GitHub user carries this operation's marker; it settles nothing" };
          }
          return failed("marker_absent", "no comment carries this operation's marker yet; the request may still land");
        }
      }
    },
  };
}
