/**
 * Ideal zero-overhead makespan for the triage-shaped success scenarios: sequential page
 * latencies, then the detail fan-out and the owner fan-out, each list-scheduled in
 * submission order on `concurrency` slots (Infinity for an unbounded engine). It ignores
 * process startup, interpreter work, and timer overshoot. It is a model, not a measurement.
 */
import { datasetA } from "./fixtures/datasets.ts";
import { APPROVAL_MS, latencyMs, type Profile } from "./fixtures/tools.ts";
import type { Scenario } from "./harness.ts";

function listSchedule(durations: number[], slots: number): number {
  if (durations.length === 0) return 0;
  const free = Array(Math.min(slots, durations.length)).fill(0);
  for (const duration of durations) {
    free.sort((a, b) => a - b);
    free[0] += duration;
  }
  return Math.max(...free);
}

export function idealMs(s: Scenario, profile: Profile, concurrency: number): number | null {
  if (s.dataset !== "A" || s.expect.kind !== "success") return null;
  const a = datasetA();
  const large = s.tools.includes("tracker.list_issues_large");
  const pageTool = large ? "tracker.list_issues_large" : "tracker.list_issues";
  const pages = Math.ceil(a.summaries.length / (large ? 120 : 40));
  const urgent = a.summaries.filter(
    (i) => i.state === "open" && i.labels.includes("regression") && (i.priority === "p0" || i.priority === "p1"),
  );
  const denied = (id: number) => s.policy === true && a.confidential.includes(id);
  const detail = urgent.map(
    (i) => (s.policy ? APPROVAL_MS : 0) + (denied(i.id) ? 0 : latencyMs(profile, "tracker.get_issue", { id: i.id })),
  );
  const owners: string[] = [];
  for (const i of urgent) if (!denied(i.id) && !owners.includes(i.assignee)) owners.push(i.assignee);
  const users = owners.map((id) => latencyMs(profile, "directory.get_user", { id }));
  return pages * latencyMs(profile, pageTool, {}) + listSchedule(detail, concurrency) + listSchedule(users, concurrency);
}
