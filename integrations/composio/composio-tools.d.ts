/// <reference path="../../types/codemode.d.ts" />
// What an investigation program sees (`codemode-composio investigate`): the
// repository it may read, and the GitHub read tools the host authorizes. A value
// is the tool's `data` as Composio normalizes it, with signed download URLs
// replaced. Input schemas come from `composio execute <SLUG> --get-schema`.

/** The `--repo` repository; every read must name exactly it. */
declare const repository: { readonly owner: string; readonly repo: string };

type RepositoryArgs = { owner: string; repo: string };
type Paged = { per_page?: number; page?: number };
type Data = { [key: string]: JsonValue };

interface CodemodeTools {
  GITHUB_LIST_REPOSITORY_ISSUES: { arguments: RepositoryArgs & Paged & { state?: "open" | "closed" | "all"; labels?: string; since?: string }; result: { issues: Data[] } };
  GITHUB_GET_AN_ISSUE: { arguments: RepositoryArgs & { issue_number: number }; result: Data };
  GITHUB_LIST_LABELS_FOR_AN_ISSUE: { arguments: RepositoryArgs & Paged & { issue_number: number }; result: { labels: Data[] } };
  GITHUB_LIST_LABELS_FOR_A_REPOSITORY: { arguments: RepositoryArgs & Paged; result: { labels: Data[] } };
  GITHUB_LIST_ISSUE_COMMENTS: { arguments: RepositoryArgs & Paged & { issue_number: number; since?: string }; result: { comments: Data[] } };
  GITHUB_GET_AN_ISSUE_COMMENT: { arguments: RepositoryArgs & { comment_id: number }; result: Data };
  GITHUB_LIST_ISSUE_EVENTS: { arguments: RepositoryArgs & Paged & { issue_number: number }; result: Data };
  GITHUB_LIST_TIMELINE_EVENTS_FOR_AN_ISSUE: { arguments: RepositoryArgs & Paged & { issue_number: number }; result: Data };
  GITHUB_LIST_WORKFLOW_RUNS_FOR_A_REPOSITORY: { arguments: RepositoryArgs & Paged & { status?: string; branch?: string; head_sha?: string; event?: string }; result: { total_count: number; workflow_runs: Data[] } };
  GITHUB_GET_A_WORKFLOW_RUN: { arguments: RepositoryArgs & { run_id: number }; result: Data };
  GITHUB_LIST_JOBS_FOR_A_WORKFLOW_RUN: { arguments: RepositoryArgs & Paged & { run_id: number; filter?: "latest" | "all" }; result: { total_count: number; jobs: Data[] } };
  GITHUB_DOWNLOAD_JOB_LOGS_FOR_A_WORKFLOW_RUN: { arguments: RepositoryArgs & { job_id: number }; result: { details: string } };
  GITHUB_DOWNLOAD_WORKFLOW_RUN_LOGS: { arguments: RepositoryArgs & { run_id: number }; result: { logs_archive: Data } };
  GITHUB_LIST_CHECK_RUNS_FOR_A_REF: { arguments: RepositoryArgs & Paged & { ref: string }; result: Data };
  GITHUB_LIST_CHECK_RUN_ANNOTATIONS: { arguments: RepositoryArgs & Paged & { check_run_id: number }; result: Data };
  GITHUB_GET_CHECK_RUN: { arguments: RepositoryArgs & { check_run_id: number }; result: Data };
}
