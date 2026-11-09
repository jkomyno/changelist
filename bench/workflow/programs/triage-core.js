// Shared GitHub triage decision, plain JavaScript (no imports, timers, or TypeScript), so it runs
// unchanged in codemode's V8 worker, Pi's QuickJS sandbox, and OpenCode's interpreter.
// The engine adapter prepends `async function gh(slug, args)` (data, or a thrown error) and appends
// what to do with the decision. Convergent: every write is derived from the state it reads, so a
// rerun after a partial run proposes only what is still missing.
const OWNER = "bench";
const REPO = "widgets";
const COMMENT_TEXT = "Triaged automatically: reproduced on the fixture and labelled for the next planning meeting.";

async function decide() {
  const issues = await gh("GITHUB_LIST_REPOSITORY_ISSUES", { owner: OWNER, repo: REPO, state: "open" });
  const ops = [];
  for (const issue of issues) {
    const n = issue.number;
    const labels = issue.labels.map((label) => label.name);
    const target = { owner: OWNER, repo: REPO, issue_number: n };
    if (labels.includes("duplicate")) {
      ops.push({ id: "close-" + n, tool: "GITHUB_UPDATE_AN_ISSUE", arguments: { ...target, state: "closed", state_reason: "not_planned" } });
      continue;
    }
    if (!labels.includes("bug")) continue;
    if (!labels.includes("triaged")) {
      ops.push({ id: "add-label-" + n, tool: "GITHUB_ADD_LABELS_TO_AN_ISSUE", arguments: { ...target, labels: ["triaged"] } });
    }
    if (labels.includes("needs-triage")) {
      ops.push({ id: "remove-label-" + n, tool: "GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE", arguments: { ...target, name: "needs-triage" } });
    }
    const comments = await gh("GITHUB_LIST_ISSUE_COMMENTS", target);
    if (!comments.some((comment) => comment.body.startsWith(COMMENT_TEXT))) {
      ops.push({ id: "comment-" + n, tool: "GITHUB_CREATE_AN_ISSUE_COMMENT", arguments: { ...target, body: COMMENT_TEXT } });
    }
  }
  return ops;
}

// Direct-write engines: perform each decided write (plus `extra`, for the widened change list) in
// order and report each outcome.
async function decideAndApply(marker, extra) {
  const ops = (await decide()).concat(extra || []);
  const outcomes = {};
  for (const op of ops) {
    const args = op.tool === "GITHUB_CREATE_AN_ISSUE_COMMENT" ? { ...op.arguments, body: op.arguments.body + "\n" + marker(op.id) } : op.arguments;
    try {
      await gh(op.tool, args);
      outcomes[op.id] = "ok";
    } catch (error) {
      outcomes[op.id] = "error: " + String((error && error.message) || error).slice(0, 300);
    }
  }
  console.log("@@OUTCOMES " + JSON.stringify(outcomes));
  return outcomes;
}
