// Triage the open issues of `repository` (defined by the host from --repo) and
// propose a short change list. Only GitHub reads of that repository are allowed;
// nothing here changes GitHub. The host turns the returned proposal into a plan
// for review, approval, and execution, and never runs this program again.
//
// - An unlabeled issue that mentions CI or a failure gets `bug`, and a comment
//   naming the jobs of the latest failed workflow run.
// - An issue whose title repeats an earlier open issue's is closed as not planned,
//   with a comment pointing at the original.
// Operations run in no fixed order, so only those that a close cannot disturb
// require the issue to still be open when they run.
/// <reference path="../composio-tools.d.ts" />

const { owner, repo } = repository;
const [open, labels, failed] = await Promise.all([
  tools.call("GITHUB_LIST_REPOSITORY_ISSUES", { owner, repo, state: "open", per_page: 100 }),
  tools.call("GITHUB_LIST_LABELS_FOR_A_REPOSITORY", { owner, repo, per_page: 100 }),
  tools.call("GITHUB_LIST_WORKFLOW_RUNS_FOR_A_REPOSITORY", { owner, repo, status: "failure", per_page: 1 }),
]);

let finding = "";
const run = failed.workflow_runs[0] as { id: number; name: string; head_sha: string } | undefined;
if (run) {
  const { jobs } = await tools.call("GITHUB_LIST_JOBS_FOR_A_WORKFLOW_RUN", { owner, repo, run_id: run.id });
  const names = (jobs as Array<{ name: string; conclusion: string }>).filter((job) => job.conclusion === "failure").map((job) => job.name);
  finding = `The latest failed ${run.name} run (${run.head_sha.slice(0, 7)}) failed in: ${names.join(", ") || "no job reported a failure"}.`;
}

type Issue = { number: number; title: string; labels: Array<{ name: string }>; pull_request?: unknown };
const issues = (open.issues as Issue[]).filter((issue) => !issue.pull_request).sort((a, b) => a.number - b.number);
const hasBug = (labels.labels as Array<{ name: string }>).some((label) => label.name === "bug");
const stillOpen = [{ issue_state: "open" }];
const operations = [];
const firstByTitle = new Map<string, number>();

for (const issue of issues) {
  const at = { owner, repo, issue_number: issue.number };
  const title = issue.title.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
  const original = firstByTitle.get(title);
  if (original !== undefined) {
    operations.push(
      { id: `comment-${issue.number}`, tool: "GITHUB_CREATE_AN_ISSUE_COMMENT", arguments: { ...at, body: `Closing as a duplicate of #${original}.` } },
      { id: `close-${issue.number}`, tool: "GITHUB_UPDATE_AN_ISSUE", arguments: { ...at, state: "closed", state_reason: "not_planned" }, preconditions: stillOpen },
    );
    continue;
  }
  firstByTitle.set(title, issue.number);
  if (hasBug && issue.labels.length === 0 && /\b(ci|build|workflow|test|fail\w*)\b/i.test(issue.title)) {
    operations.push({ id: `label-${issue.number}`, tool: "GITHUB_ADD_LABELS_TO_AN_ISSUE", arguments: { ...at, labels: ["bug"] }, preconditions: stillOpen });
    if (finding) {
      operations.push({ id: `comment-${issue.number}`, tool: "GITHUB_CREATE_AN_ISSUE_COMMENT", arguments: { ...at, body: finding }, preconditions: stillOpen });
    }
  }
}

return { operations };
