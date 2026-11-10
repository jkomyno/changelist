/**
 * Offline stand-in for the `composio run` helper `execute(slug, data, { account })`
 * (Composio CLI 0.6.0) over a small GitHub: repositories with numeric ids and
 * labels, issues with state, labels, and authored comments, and workflow runs,
 * jobs, and logs. It keeps its own ledger in a JSON file, independent of
 * codemode's journal: the state, and every call it received with the account
 * it was given. The file is read before and written after each call, so a
 * killed host leaves it consistent and a test can change it between sessions
 * (a late effect, a revoked account, a renamed repository).
 *
 * It reproduces the helper's observed contract with synthetic values: a resolved
 * `{ successful: true, data }`, an Error carrying an unsuccessful `result` (with
 * GitHub's `status_code`), or an Error without a result when the CLI produced
 * none, as for an unmatched account selector. Results and error text carry
 * `MARK-SECRET`, error text also names a `ca_` id, job logs carry
 * `MARK-LOG-BODY`, and run logs carry a signed URL with `MARK-SIGNED`, so checks
 * can prove none is kept.
 *
 * Faults, keyed `"<slug> <owner/repo>"` or `"<slug> <owner/repo>#<issue>"` in
 * `faults` and used once: `hold` (record the request, never apply or answer),
 * `fail_after_apply` (apply, then report an unsuccessful result),
 * `unobserved_after_apply` (apply, then end without a result), `dry_run` (answer
 * as `composio run --dry-run` does, sending nothing), `fail` (an unsuccessful
 * read), `malformed` (a read of the wrong shape), `unobserved` (a read without a
 * result), `slow` (a read that takes 10 s and says so on stderr).
 * No network, account, or credential is used.
 */
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import type { ComposioResult } from "../composio-provider.ts";
import { ADD_LABELS, CREATE_COMMENT, REMOVE_LABEL, UPDATE_ISSUE, WRITE_TOOLS } from "../github-plan.ts";

const SECRET = "MARK-SECRET";
/** A connected-account id as the CLI prints it in errors; it must never be kept. */
export const LEAKED_ACCOUNT = "ca_LEAKEDinERROR";

export interface Comment {
  id: number;
  user: string;
  body: string;
}

export interface Issue {
  title: string;
  state: "open" | "closed";
  state_reason?: string | null;
  labels: string[];
  comments: Comment[];
}

export interface Repository {
  id: number;
  labels: string[];
  permissions: { push: boolean; triage: boolean };
  issues: Record<string, Issue>;
  runs?: Array<{ id: number; name: string; conclusion: string; head_sha: string; jobs: Array<{ id: number; name: string; conclusion: string }> }>;
}

export interface GithubState {
  /** Connected-account id to the GitHub login it acts as. */
  accounts: Record<string, string>;
  /** What `composio connections list` reports; every account, active, when absent. */
  connections?: Array<{ id: string; status: string; alias?: string }>;
  repositories: Record<string, Repository>;
  faults: Record<string, string>;
  calls: Array<{ slug: string; account: string; target: string; outcome: string }>;
}

const failure = (message: string, detail: Record<string, unknown>) => Object.assign(new Error(message), detail);
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** An unsuccessful result with a GitHub status, thrown as the helper does. */
function unsuccessful(slug: string, status: number): Error {
  const result = { successful: false, data: { status_code: status, message: `${SECRET} upstream text`, http_error: SECRET }, error: `${SECRET} ${status} for ${LEAKED_ACCOUNT}` };
  return failure(result.error, { result, slug });
}

const noResult = (slug: string) => failure(`composio execute ${slug} returned no JSON output (${LEAKED_ACCOUNT})`, { exitCode: 1, stderr: `${SECRET} ${LEAKED_ACCOUNT}` });

/** The stand-in's `composio connections list` for GitHub. */
export function fakeConnections(path: string) {
  return async () => {
    const state: GithubState = JSON.parse(readFileSync(path, "utf8"));
    return state.connections ?? Object.keys(state.accounts).map((id) => ({ id, status: "ACTIVE" }));
  };
}

const page = <T>(items: T[], data: Record<string, unknown>) => {
  const size = Number(data.per_page ?? 30);
  const number = Number(data.page ?? 1);
  return items.slice((number - 1) * size, number * size);
};
const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function fakeGithub(path: string) {
  const load = (): GithubState => JSON.parse(readFileSync(path, "utf8"));
  const save = (state: GithubState) => {
    writeFileSync(`${path}.tmp`, JSON.stringify(state, null, 2));
    renameSync(`${path}.tmp`, path);
  };

  return async (slug: string, data: Record<string, unknown>, options: { account: string }): Promise<ComposioResult> => {
    const state = load();
    const name = `${String(data.owner ?? "")}/${String(data.repo ?? "")}`.toLowerCase();
    const target = data.issue_number === undefined ? name : `${name}#${data.issue_number}`;
    const record = (outcome: string) => {
      state.calls.push({ slug, account: options?.account ?? "", target, outcome });
      save(state);
    };
    const fault = state.faults[`${slug} ${target}`];
    if (fault) delete state.faults[`${slug} ${target}`];

    const login = state.accounts[options?.account ?? ""];
    const connection = state.connections?.find((item) => item.id === options?.account);
    if (!login || (connection && connection.status !== "ACTIVE")) {
      record("account_rejected");
      throw noResult(slug);
    }
    const ok = (outcome: string, value: unknown): ComposioResult => {
      record(outcome);
      return { successful: true, data: value, error: null };
    };
    if (slug === "GITHUB_GET_THE_AUTHENTICATED_USER") return ok("read", { login, id: 7, bio: SECRET });

    const repository = state.repositories[name];
    if (!repository) {
      record("not_found");
      throw unsuccessful(slug, 404);
    }
    const issue = repository.issues[String(data.issue_number)];
    const issueView = (number: string, item: Issue) => ({
      number: Number(number), title: item.title, state: item.state, state_reason: item.state_reason ?? null,
      labels: item.labels.map((label) => ({ name: label, description: SECRET })), body: SECRET,
    });
    const isRead = !(WRITE_TOOLS as readonly string[]).includes(slug);
    if (isRead && fault) {
      if (fault === "slow") {
        process.stderr.write(`started ${slug}\n`);
        await sleep(10_000);
      } else if (fault === "unobserved") {
        record("read_unobserved");
        throw noResult(slug);
      } else if (fault === "fail") {
        record("read_failed");
        throw unsuccessful(slug, 502);
      } else if (fault === "malformed") {
        return ok("read_malformed", { labels: SECRET, comments: SECRET });
      }
    }
    if (slug.includes("ISSUE") && !slug.startsWith("GITHUB_LIST_REPOSITORY") && !issue) {
      record("not_found");
      throw unsuccessful(slug, 404);
    }

    switch (slug) {
      case "GITHUB_GET_A_REPOSITORY":
        return ok("read", { id: repository.id, full_name: name, secret: SECRET, permissions: { ...repository.permissions, pull: true, admin: false, maintain: false } });
      case "GITHUB_GET_LABEL": {
        const label = repository.labels.find((item) => same(item, String(data.name)));
        if (!label) {
          record("label_missing");
          throw unsuccessful(slug, 404);
        }
        return ok("read", { name: label, description: SECRET, id: 1 });
      }
      case "GITHUB_LIST_LABELS_FOR_A_REPOSITORY":
        return ok("read", { labels: page(repository.labels, data).map((label) => ({ name: label, description: SECRET })) });
      case "GITHUB_LIST_REPOSITORY_ISSUES": {
        const wanted = String(data.state ?? "open");
        const issues = Object.entries(repository.issues).filter(([, item]) => wanted === "all" || item.state === wanted);
        return ok("read", { issues: page(issues, data).map(([number, item]) => issueView(number, item)) });
      }
      case "GITHUB_GET_AN_ISSUE":
        return ok("read", issueView(String(data.issue_number), issue));
      case "GITHUB_LIST_LABELS_FOR_AN_ISSUE":
        return ok("read", { labels: page(issue.labels, data).map((label) => ({ name: label, description: SECRET })) });
      case "GITHUB_LIST_ISSUE_COMMENTS":
        return ok("read", { comments: page(issue.comments, data).map((comment) => ({ id: comment.id, body: comment.body, user: { login: comment.user, bio: SECRET } })) });
      case "GITHUB_LIST_WORKFLOW_RUNS_FOR_A_REPOSITORY": {
        const runs = (repository.runs ?? []).filter((run) => data.status === undefined || run.conclusion === data.status);
        return ok("read", { total_count: runs.length, workflow_runs: page(runs, data).map(({ jobs: _, ...run }) => ({ ...run, display_title: SECRET })) });
      }
      case "GITHUB_LIST_JOBS_FOR_A_WORKFLOW_RUN": {
        const run = (repository.runs ?? []).find((item) => item.id === data.run_id);
        return ok("read", { total_count: run?.jobs.length ?? 0, jobs: run?.jobs ?? [] });
      }
      case "GITHUB_DOWNLOAD_JOB_LOGS_FOR_A_WORKFLOW_RUN":
        return ok("read", { details: `MARK-LOG-BODY ${SECRET}\nerror: the check failed\nmore MARK-LOG-BODY` });
      case "GITHUB_DOWNLOAD_WORKFLOW_RUN_LOGS":
        return ok("read", {
          logs_archive: {
            name: "logs.zip",
            mimetype: "application/zip",
            s3url: "https://temp.example.r2.cloudflarestorage.com/logs.zip?X-Amz-Credential=MARK-SIGNED&X-Amz-Signature=MARK-SIGNED&x-id=GetObject",
          },
        });
    }

    // Writes.
    if (fault === "hold") {
      record("held");
      return new Promise<never>(() => {});
    }
    if (fault === "dry_run") {
      record("dry_run");
      return { successful: true, dryRun: true, error: null, slug, arguments: data } as ComposioResult;
    }
    let value: unknown;
    switch (slug) {
      case ADD_LABELS:
        for (const label of data.labels as string[]) {
          if (!issue.labels.some((item) => same(item, label))) issue.labels.push(label);
        }
        value = { labels: issue.labels.map((label) => ({ name: label, description: SECRET })) };
        break;
      case REMOVE_LABEL: {
        const index = issue.labels.findIndex((item) => same(item, String(data.name)));
        if (index < 0) {
          record("label_not_on_issue");
          throw unsuccessful(slug, 404);
        }
        issue.labels.splice(index, 1);
        value = { labels: issue.labels.map((label) => ({ name: label })) };
        break;
      }
      case UPDATE_ISSUE:
        issue.state = data.state as Issue["state"];
        issue.state_reason = (data.state_reason as string | undefined) ?? null;
        value = { number: data.issue_number, state: issue.state, state_reason: issue.state_reason, body: SECRET };
        break;
      case CREATE_COMMENT: {
        const id = 1000 + Object.values(repository.issues).reduce((count, item) => count + item.comments.length, 0);
        issue.comments.push({ id, user: login, body: String(data.body) });
        value = { id, body: data.body, user: { login }, html_url: SECRET };
        break;
      }
      default:
        record("unknown_tool");
        throw failure(`composio execute ${slug} failed with exit code 1`, { exitCode: 1 });
    }
    if (fault === "fail_after_apply") {
      record("applied_then_failed");
      throw unsuccessful(slug, 500);
    }
    if (fault === "unobserved_after_apply") {
      record("applied_then_unobserved");
      throw noResult(slug);
    }
    return ok("applied", value);
  };
}
