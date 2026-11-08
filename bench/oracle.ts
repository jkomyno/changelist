/**
 * Expected outputs computed directly from the datasets, with no engine and no tools.
 * `bench/check_expected.py` recomputes the same values independently from a JSON dump.
 *
 *   bun bench/oracle.ts            print expected outputs as JSON
 *   bun bench/oracle.ts --write    write bench/expected.json
 */
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { type IssueDetail, RELEASE_WINDOW, STALE_CUTOFF, type User, datasetA, datasetB } from "./fixtures/datasets.ts";

const PRIORITY_RANK: Record<string, number> = { p0: 0, p1: 1, p2: 2, p3: 3 };

function lastActivity(detail: IssueDetail): string {
  return detail.comments.reduce((latest, comment) => (comment.at > latest ? comment.at : latest), detail.comments.length ? detail.comments[0].at : detail.updated_at);
}

function triage(denyConfidential: boolean) {
  const a = datasetA();
  const users = new Map<string, User>(a.users.map((user) => [user.id, user]));
  const urgent = a.details.filter(
    (d) => d.state === "open" && d.labels.includes("regression") && (d.priority === "p0" || d.priority === "p1"),
  );
  const denied = denyConfidential ? urgent.filter((d) => a.confidential.includes(d.id)).map((d) => d.id) : [];
  const readable = urgent.filter((d) => !denied.includes(d.id));
  const stale = readable
    .filter((d) => lastActivity(d) < STALE_CUTOFF && !users.get(d.assignee)!.on_call)
    .sort(
      (x, y) =>
        PRIORITY_RANK[x.priority] - PRIORITY_RANK[y.priority] ||
        (x.updated_at < y.updated_at ? -1 : x.updated_at > y.updated_at ? 1 : 0) ||
        x.id - y.id,
    );
  const byTeam: Record<string, number> = {};
  for (const d of stale) {
    const team = users.get(d.assignee)!.team;
    byTeam[team] = (byTeam[team] ?? 0) + 1;
  }
  const output: Record<string, unknown> = {
    stale_count: stale.length,
    top: stale.slice(0, 5).map((d) => {
      const owner = users.get(d.assignee)!;
      return { id: d.id, title: d.title, owner: owner.name, team: owner.team };
    }),
    by_team: byTeam,
  };
  if (denyConfidential) output.denied = [...denied].sort((x, y) => x - y);
  return output;
}

function releaseAudit() {
  const b = datasetB();
  const teams = new Map(datasetA().users.map((user) => [user.id, user.team]));
  const merged = b.pulls.filter(
    (p) => p.merged_at !== null && p.merged_at >= RELEASE_WINDOW.start && p.merged_at < RELEASE_WINDOW.end,
  );
  const byTeam: Record<string, number> = {};
  const byCheck: Record<string, number> = {};
  const failingPrs: number[] = [];
  for (const pull of merged) {
    const failures = b.checks[String(pull.number)].filter((c) => c.conclusion === "failure");
    if (failures.length === 0) continue;
    failingPrs.push(pull.number);
    const team = teams.get(pull.author)!;
    byTeam[team] = (byTeam[team] ?? 0) + failures.length;
    for (const check of failures) byCheck[check.name] = (byCheck[check.name] ?? 0) + 1;
  }
  return {
    window: { start: RELEASE_WINDOW.start, end: RELEASE_WINDOW.end },
    merged_count: merged.length,
    failing_prs: failingPrs.sort((x, y) => x - y),
    failing_by_team: byTeam,
    failing_by_check: byCheck,
  };
}

/** Expected outputs keyed by the `expect.output` names in scenarios.json. */
export function expectedOutputs(): Record<string, unknown> {
  return {
    triage: triage(false),
    "triage-large": triage(false),
    denied: triage(true),
    "release-audit": releaseAudit(),
  };
}

if (import.meta.main ?? process.argv[1] === fileURLToPath(import.meta.url)) {
  const text = JSON.stringify(expectedOutputs(), null, 2) + "\n";
  if (process.argv.includes("--write")) {
    writeFileSync(join(dirname(fileURLToPath(import.meta.url)), "expected.json"), text);
  } else process.stdout.write(text);
}
