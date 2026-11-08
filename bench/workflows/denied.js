// Triage under a host permission policy: some issue reads are denied. The program keeps
// going with the readable issues and reports which ids were denied.
const cutoff = "2026-09-24T00:00:00Z"; // fixed now (2026-10-01T00:00:00Z) minus 7 days
const rank = { p0: 0, p1: 1, p2: 2, p3: 3 };

const candidates = [];
let cursor = null;
let more = true;
while (more) {
  const args = cursor === null ? { project: "atlas", limit: 40 } : { project: "atlas", cursor: cursor, limit: 40 };
  const page = await tools.call("tracker.list_issues", args);
  for (const issue of page.items) {
    const urgent = issue.priority === "p0" || issue.priority === "p1";
    if (issue.state === "open" && urgent && issue.labels.includes("regression")) candidates.push(issue);
  }
  cursor = page.next_cursor;
  more = cursor !== null;
}

const settled = await Promise.allSettled(candidates.map((issue) => tools.call("tracker.get_issue", { id: issue.id })));
const details = [];
const denied = [];
for (let i = 0; i < settled.length; i++) {
  if (settled[i].status === "fulfilled") details.push(settled[i].value);
  else denied.push(candidates[i].id);
}
const deniedIds = denied.sort((a, b) => a - b);

const assignees = [];
for (const detail of details) {
  if (!assignees.includes(detail.assignee)) assignees.push(detail.assignee);
}
const users = await Promise.all(assignees.map((id) => tools.call("directory.get_user", { id: id })));
const owners = {};
for (const user of users) owners[user.id] = user;

const stale = [];
for (const detail of details) {
  let last = detail.updated_at;
  if (detail.comments.length > 0) {
    last = detail.comments[0].at;
    for (const comment of detail.comments) {
      if (comment.at > last) last = comment.at;
    }
  }
  const owner = owners[detail.assignee];
  if (last < cutoff && !owner.on_call) stale.push({ issue: detail, owner: owner });
}

const compareText = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
// Use sort's return value: OpenCode's sort returns a sorted copy and leaves `stale` as is.
const ordered = stale.sort(
  (a, b) =>
    rank[a.issue.priority] - rank[b.issue.priority] ||
    compareText(a.issue.updated_at, b.issue.updated_at) ||
    a.issue.id - b.issue.id,
);

const by_team = {};
for (const entry of ordered) by_team[entry.owner.team] = (by_team[entry.owner.team] || 0) + 1;

return {
  stale_count: ordered.length,
  top: ordered.slice(0, 5).map((entry) => ({
    id: entry.issue.id,
    title: entry.issue.title,
    owner: entry.owner.name,
    team: entry.owner.team,
  })),
  by_team: by_team,
  denied: deniedIds,
};
