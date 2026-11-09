// The offline GitHub repository every implementation acts on, and the pure functions that derive
// its state from the provider's ledger. The provider applies effects with these functions; the
// oracle re-derives state from the ledger file alone, so neither trusts an engine's report.
import { existsSync, readFileSync } from "node:fs";

export const OWNER = "bench";
export const REPO = "widgets";
export const REPO_ID = 4242;
export const ACCOUNT = "ca_bench_triage";
export const LOGIN = "triage-bot";
export const REPO_LABELS = ["bug", "enhancement", "duplicate", "needs-triage", "good-first-issue", "triaged", "wontfix"];
/** First line of every triage comment; the last line is the implementation's own marker. */
export const COMMENT_TEXT = "Triaged automatically: reproduced on the fixture and labelled for the next planning meeting.";

export interface Comment {
  id: number;
  author: string;
  body: string;
}

export interface Issue {
  number: number;
  title: string;
  state: "open" | "closed";
  state_reason: string | null;
  labels: string[];
  comments: Comment[];
}

const issue = (number: number, title: string, labels: string[], state: "open" | "closed" = "open"): Issue => ({
  number,
  title,
  state,
  state_reason: state === "closed" ? "completed" : null,
  labels,
  comments: [],
});

/** Seven issues; the triage workflow derives eleven writes from them. */
export const FIXTURE: readonly Issue[] = Object.freeze([
  issue(1, "Crash on empty config", ["bug", "needs-triage"]),
  issue(2, "Add dark mode", ["enhancement"]),
  issue(3, "Typo in README", ["bug", "needs-triage", "good-first-issue"]),
  issue(4, "Old duplicate of #1", ["duplicate"]),
  issue(5, "Panic when the cache directory is read-only", ["bug", "needs-triage"]),
  issue(6, "Duplicate of #3", ["duplicate", "bug"]),
  issue(7, "Fixed crash", ["bug"], "closed"),
]);

/** The four GitHub writes, by Composio tool slug. */
export const WRITES = {
  addLabels: "GITHUB_ADD_LABELS_TO_AN_ISSUE",
  removeLabel: "GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE",
  updateIssue: "GITHUB_UPDATE_AN_ISSUE",
  createComment: "GITHUB_CREATE_AN_ISSUE_COMMENT",
} as const;

export const READS = {
  user: "GITHUB_GET_THE_AUTHENTICATED_USER",
  repository: "GITHUB_GET_A_REPOSITORY",
  listIssues: "GITHUB_LIST_REPOSITORY_ISSUES",
  getIssue: "GITHUB_GET_AN_ISSUE",
  listComments: "GITHUB_LIST_ISSUE_COMMENTS",
} as const;

export const WRITE_SLUGS: readonly string[] = Object.values(WRITES);
export const READ_SLUGS: readonly string[] = Object.values(READS);

export interface LedgerRecord {
  seq: number;
  ts: number;
  /** For `start`: when the request reached the provider, before the simulated latency. */
  received?: number;
  event: "start" | "effect_applied" | "held" | "terminal" | "control";
  call_id?: string;
  slug?: string;
  args?: Record<string, unknown>;
  account?: string;
  /** The GitHub login the call acted as (start) or the author of an effect. */
  actor?: string;
  late?: boolean;
  cause?: string;
  status?: string;
  delivered?: boolean;
  action?: string;
  detail?: unknown;
}

export function readLedger(path: string): LedgerRecord[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as LedgerRecord);
}

export function freshState(): Issue[] {
  return FIXTURE.map((item) => ({ ...item, labels: [...item.labels], comments: [] }));
}

export class GithubError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

function find(issues: Issue[], args: Record<string, unknown>): Issue {
  if (args.owner !== OWNER || args.repo !== REPO) throw new GithubError(404, "repository not found");
  const found = issues.find((item) => item.number === args.issue_number);
  if (!found) throw new GithubError(404, `issue ${String(args.issue_number)} not found`);
  return found;
}

/** Check a write against current state without changing it; throws what GitHub would answer. */
export function validateWrite(issues: Issue[], slug: string, args: Record<string, unknown>): void {
  const target = find(issues, args);
  switch (slug) {
    case WRITES.addLabels: {
      const labels = args.labels;
      if (!Array.isArray(labels) || labels.length === 0 || !labels.every((l) => typeof l === "string")) {
        throw new GithubError(422, "labels must be a non-empty string array");
      }
      return;
    }
    case WRITES.removeLabel:
      if (!target.labels.includes(String(args.name))) throw new GithubError(404, "label does not exist on the issue");
      return;
    case WRITES.updateIssue:
      if (args.state !== "open" && args.state !== "closed") throw new GithubError(422, "state must be open or closed");
      return;
    case WRITES.createComment:
      if (typeof args.body !== "string" || args.body.length === 0) throw new GithubError(422, "body is required");
      return;
    default:
      throw new GithubError(404, `unknown tool ${slug}`);
  }
}

/** Apply one write as `actor`. */
export function applyWrite(issues: Issue[], slug: string, args: Record<string, unknown>, actor: string): void {
  validateWrite(issues, slug, args);
  const target = find(issues, args);
  switch (slug) {
    case WRITES.addLabels:
      for (const label of args.labels as string[]) if (!target.labels.includes(label)) target.labels.push(label);
      return;
    case WRITES.removeLabel:
      target.labels = target.labels.filter((label) => label !== args.name);
      return;
    case WRITES.updateIssue:
      target.state = args.state as "open" | "closed";
      target.state_reason = typeof args.state_reason === "string" ? args.state_reason : null;
      return;
    case WRITES.createComment: {
      const id = 1000 + issues.reduce((total, item) => total + item.comments.length, 0);
      target.comments.push({ id, author: actor, body: args.body as string });
      return;
    }
  }
}

export function stateFromLedger(records: LedgerRecord[]): Issue[] {
  const issues = freshState();
  for (const record of records) {
    if (record.event === "effect_applied") applyWrite(issues, record.slug!, record.args!, record.actor!);
  }
  return issues;
}

/** The operation id the triage workflow gives a write, e.g. `add-label-1`; the same for every implementation. */
export function operationId(slug: string, args: Record<string, unknown>): string {
  const n = String(args.issue_number);
  switch (slug) {
    case WRITES.addLabels:
      return `add-label-${n}`;
    case WRITES.removeLabel:
      return `remove-label-${n}`;
    case WRITES.updateIssue:
      return `close-${n}`;
    case WRITES.createComment:
      return `comment-${n}`;
    default:
      return `${slug}-${n}`;
  }
}

/** State with each comment's marker line removed, so implementations with different markers compare equal. */
export function normalized(issues: Issue[]): unknown {
  return issues.map((item) => ({
    number: item.number,
    state: item.state,
    state_reason: item.state_reason,
    labels: [...item.labels].sort(),
    comments: item.comments.map((c) => ({ author: c.author, text: c.body.split("\n").filter((line) => !line.startsWith("<!--")).join("\n") })),
  }));
}
