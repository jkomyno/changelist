/**
 * Offline stand-in for the `composio run` helper `execute(slug, data, { account })`
 * (Composio CLI 0.6.0) over GitHub issue labels. It keeps its own ledger in a
 * JSON file, independent of codemode's journal: repository labels, each issue's
 * labels, and every call it received with the account it was given. The file is
 * read before and written after each call, so a killed host leaves it consistent
 * and a test can change it between sessions (for example, a late effect).
 *
 * It reproduces the helper's observed contract with synthetic values: a resolved
 * `{ successful: true, data }`, an Error carrying an unsuccessful `result` (with
 * GitHub's `status_code`), or an Error without a result when the CLI produced
 * none, as for an unmatched account selector. Every result also carries a
 * `MARK-SECRET` field and message, so checks can prove none is kept.
 *
 * Faults, keyed `"<slug> <owner/repo>#<issue>"` in `faults` and used once:
 * `hold` (record the request, never apply or answer), `fail_after_apply`
 * (apply, then report an unsuccessful result), `unobserved_after_apply` (apply,
 * then end without a result), `fail` (an unsuccessful read), `malformed` (a read
 * without a labels array), `unobserved` (a read without a result).
 * No network, account, or credential is used.
 */
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import type { ComposioResult } from "../composio-provider.ts";

const SECRET = "MARK-SECRET";

interface Repository {
  labels: string[];
  permissions: { push: boolean; triage: boolean };
  issues: Record<string, string[]>;
}

export interface LabelState {
  accounts: string[];
  /** What `composio connections list` reports; every account, active, when absent. */
  connections?: Array<{ id: string; status: string; alias?: string }>;
  repositories: Record<string, Repository>;
  faults: Record<string, string>;
  calls: Array<{ slug: string; account: string; target: string; outcome: string }>;
}

const failure = (message: string, detail: Record<string, unknown>) => Object.assign(new Error(message), detail);

/** An unsuccessful result with a GitHub status, thrown as the helper does. */
function unsuccessful(slug: string, status: number): Error {
  const result = { successful: false, data: { status_code: status, message: `${SECRET} upstream text`, http_error: SECRET }, error: `${SECRET} ${status}` };
  return failure(result.error, { result, slug });
}

/** The stand-in's `composio connections list` for GitHub. */
export function fakeConnections(path: string) {
  return async () => {
    const state: LabelState = JSON.parse(readFileSync(path, "utf8"));
    return state.connections ?? state.accounts.map((id) => ({ id, status: "ACTIVE" }));
  };
}

export function fakeLabels(path: string) {
  const load = (): LabelState => JSON.parse(readFileSync(path, "utf8"));
  const save = (state: LabelState) => {
    writeFileSync(`${path}.tmp`, JSON.stringify(state, null, 2));
    renameSync(`${path}.tmp`, path);
  };

  return async (slug: string, data: Record<string, unknown>, options: { account: string }): Promise<ComposioResult> => {
    const state = load();
    const owner = String(data.owner ?? "");
    const repoName = String(data.repo ?? "");
    const name = `${owner}/${repoName}`.toLowerCase();
    const issue = data.issue_number === undefined ? "" : `#${data.issue_number}`;
    const target = `${name}${issue}`;
    const record = (outcome: string) => {
      state.calls.push({ slug, account: options?.account ?? "", target, outcome });
      save(state);
    };
    const fault = state.faults[`${slug} ${target}`];
    if (fault) delete state.faults[`${slug} ${target}`];

    if (!options?.account || !state.accounts.includes(options.account)) {
      record("account_rejected");
      throw failure(`composio execute ${slug} failed with exit code 1`, { exitCode: 1, stderr: SECRET });
    }
    const repository = state.repositories[name];
    if (!repository) {
      record("not_found");
      throw unsuccessful(slug, 404);
    }
    const issueLabels = repository.issues[String(data.issue_number)];

    switch (slug) {
      case "GITHUB_GET_A_REPOSITORY":
        record("read");
        return { successful: true, data: { full_name: name, secret: SECRET, permissions: { ...repository.permissions, pull: true, admin: false, maintain: false } }, error: null };
      case "GITHUB_GET_LABEL": {
        const label = repository.labels.find((item) => item.toLowerCase() === String(data.name).toLowerCase());
        record(label ? "read" : "label_missing");
        if (!label) throw unsuccessful(slug, 404);
        return { successful: true, data: { name: label, description: SECRET, id: 1 }, error: null };
      }
      case "GITHUB_LIST_LABELS_FOR_AN_ISSUE": {
        if (!issueLabels) {
          record("not_found");
          throw unsuccessful(slug, 404);
        }
        if (fault === "unobserved") {
          record("read_unobserved");
          throw failure(`composio execute ${slug} returned no JSON output`, { exitCode: 1 });
        }
        if (fault === "fail") {
          record("read_failed");
          throw unsuccessful(slug, 502);
        }
        if (fault === "malformed") {
          record("read_malformed");
          return { successful: true, data: { labels: SECRET }, error: null };
        }
        const perPage = Number(data.per_page ?? 30);
        const page = Number(data.page ?? 1);
        record("read");
        const labels = issueLabels.slice((page - 1) * perPage, page * perPage).map((label) => ({ name: label, description: SECRET }));
        return { successful: true, data: { labels }, error: null };
      }
      case "GITHUB_ADD_LABELS_TO_AN_ISSUE": {
        if (!issueLabels) {
          record("not_found");
          throw unsuccessful(slug, 404);
        }
        if (fault === "hold") {
          record("held");
          return new Promise<never>(() => {});
        }
        for (const label of data.labels as string[]) {
          if (!issueLabels.some((item) => item.toLowerCase() === label.toLowerCase())) issueLabels.push(label);
        }
        if (fault === "fail_after_apply") {
          record("applied_then_failed");
          throw unsuccessful(slug, 500);
        }
        if (fault === "unobserved_after_apply") {
          record("applied_then_unobserved");
          throw failure(`composio execute ${slug} returned no JSON output`, { exitCode: 1 });
        }
        record("applied");
        return { successful: true, data: { labels: issueLabels.map((label) => ({ name: label, description: SECRET })) }, error: null };
      }
      default:
        record("unknown_tool");
        throw failure(`composio execute ${slug} failed with exit code 1`, { exitCode: 1 });
    }
  };
}
