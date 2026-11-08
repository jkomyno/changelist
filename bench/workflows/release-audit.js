// Release audit (holdout): page through pull requests, keep those merged in the release
// window, fetch their CI checks and authors in parallel, and summarize failing checks.
const start = "2026-09-01T00:00:00Z";
const end = "2026-09-15T00:00:00Z";

const merged = [];
let cursor = null;
let more = true;
while (more) {
  const args = cursor === null ? { repo: "orbit", limit: 50 } : { repo: "orbit", cursor: cursor, limit: 50 };
  const page = await tools.call("vcs.list_pulls", args);
  for (const pull of page.items) {
    if (pull.merged_at !== null && pull.merged_at >= start && pull.merged_at < end) merged.push(pull);
  }
  cursor = page.next_cursor;
  more = cursor !== null;
}

const checks = await Promise.all(merged.map((pull) => tools.call("ci.get_checks", { pr: pull.number })));
const authors = [];
for (const pull of merged) {
  if (!authors.includes(pull.author)) authors.push(pull.author);
}
const users = await Promise.all(authors.map((id) => tools.call("directory.get_user", { id: id })));
const teams = {};
for (const user of users) teams[user.id] = user.team;

const failing_by_team = {};
const failing_by_check = {};
const failing_prs = [];
for (let i = 0; i < merged.length; i++) {
  const failures = checks[i].checks.filter((check) => check.conclusion === "failure");
  if (failures.length === 0) continue;
  failing_prs.push(merged[i].number);
  const team = teams[merged[i].author];
  failing_by_team[team] = (failing_by_team[team] || 0) + failures.length;
  for (const check of failures) failing_by_check[check.name] = (failing_by_check[check.name] || 0) + 1;
}

return {
  window: { start: start, end: end },
  merged_count: merged.length,
  failing_prs: failing_prs.sort((a, b) => a - b),
  failing_by_team: failing_by_team,
  failing_by_check: failing_by_check,
};
