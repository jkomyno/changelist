/**
 * Deterministic datasets. No wall clock and no ambient randomness: every value derives
 * from a seeded mulberry32 stream and a fixed "now". Timestamps are ISO-8601 UTC with
 * second precision ("2026-09-30T12:34:56Z"), so lexical comparison orders them.
 *
 * Dataset A (seed 1): project "atlas", 240 issues and 12 users.
 * Dataset B (seed 2): repo "orbit", 150 pull requests with CI checks (holdout).
 */
import { deepFreeze, int, mulberry32, pick } from "./util.ts";

export const NOW = "2026-10-01T00:00:00Z";
/** NOW minus 7 days; the triage staleness cutoff. */
export const STALE_CUTOFF = "2026-09-24T00:00:00Z";
/** Holdout release window, [start, end). */
export const RELEASE_WINDOW = { start: "2026-09-01T00:00:00Z", end: "2026-09-15T00:00:00Z" } as const;

const NOW_MS = Date.UTC(2026, 9, 1);
const DAY = 86_400_000;
const iso = (ms: number) => new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");

export const TEAMS = ["platform", "payments", "search", "mobile"] as const;
const NAMES = [
  "Ada Park", "Bruno Silva", "Chen Wei", "Dana Kowalski", "Elif Demir", "Femi Adeyemi",
  "Greta Lind", "Hiro Tanaka", "Ines Moreau", "Jonas Berg", "Kavya Rao", "Luca Bianchi",
];
const TIMEZONES = ["UTC", "Europe/Berlin", "America/New_York", "Asia/Tokyo", "Asia/Kolkata", "America/Sao_Paulo"];
const WORDS = (
  "cache index shard replica token session queue worker scheduler retry timeout payload " +
  "checkout invoice ledger refund webhook search ranking query filter facet mobile render " +
  "layout gesture crash memory leak latency spike deploy rollback migration schema config " +
  "flag rollout metric alert dashboard upload download sync offline login permission audit"
).split(" ");
const OTHER_LABELS = ["bug", "ui", "backend", "perf", "docs", "security", "infra", "flaky"];
const PRIORITIES = ["p0", "p1", "p2", "p3"] as const;

export interface User { id: string; name: string; team: string; on_call: boolean; timezone: string }
export interface IssueSummary {
  id: number; title: string; state: "open" | "closed"; labels: string[]; priority: string;
  assignee: string; updated_at: string; comment_count: number;
}
export interface Comment { author: string; at: string; text: string }
export interface IssueDetail extends IssueSummary { body: string; comments: Comment[]; linked_prs: number[] }
export interface HistoryEntry { at: string; field: string; from: string; to: string; by: string }
export interface LargeSummary extends IssueSummary { body_preview: string; history: HistoryEntry[] }
export interface DatasetA {
  project: string;
  users: User[];
  summaries: IssueSummary[];
  details: IssueDetail[];
  large: LargeSummary[];
  /** Issue ids the denial policy refuses (2 of the open p0/p1 regressions). */
  confidential: number[];
}

export interface Pull { number: number; title: string; author: string; merged_at: string | null; base: string }
export interface Check { name: string; conclusion: "success" | "failure" | "neutral"; duration_s: number }
export interface DatasetB { repo: string; pulls: Pull[]; checks: Record<string, Check[]> }

function words(rng: () => number, minChars: number, maxChars: number): string {
  let out = "";
  while (out.length < minChars) {
    const next = pick(rng, WORDS);
    if (out.length + 1 + next.length > maxChars) break;
    out = out ? `${out} ${next}` : next;
  }
  return out;
}

function prose(rng: () => number, minChars: number): string {
  const sentences: string[] = [];
  let length = 0;
  while (length < minChars) {
    const sentence = words(rng, 50, 110);
    const text = sentence.charAt(0).toUpperCase() + sentence.slice(1) + ".";
    sentences.push(text);
    length += text.length + 1;
  }
  return sentences.join(" ");
}

function weighted<T>(rng: () => number, items: readonly T[], weights: readonly number[]): T {
  let roll = rng();
  for (let i = 0; i < items.length; i++) {
    roll -= weights[i];
    if (roll < 0) return items[i];
  }
  return items[items.length - 1];
}

export function buildDatasetA(seed = 1): DatasetA {
  const rng = mulberry32(seed);
  const users: User[] = NAMES.map((name, i) => ({
    id: `u${String(i + 1).padStart(2, "0")}`,
    name,
    team: TEAMS[i % TEAMS.length],
    on_call: rng() < 0.3,
    timezone: pick(rng, TIMEZONES),
  }));

  const summaries: IssueSummary[] = [];
  const details: IssueDetail[] = [];
  const large: LargeSummary[] = [];
  for (let id = 1; id <= 240; id++) {
    const regression = rng() < 0.11;
    const state = rng() < (regression ? 0.85 : 0.55) ? "open" : "closed";
    const priority = regression
      ? weighted(rng, PRIORITIES, [0.4, 0.5, 0.07, 0.03])
      : weighted(rng, PRIORITIES, [0.1, 0.25, 0.35, 0.3]);
    const labels: string[] = [];
    const extra = int(rng, 0, 2);
    for (let i = 0; i < extra; i++) {
      const label = pick(rng, OTHER_LABELS);
      if (!labels.includes(label)) labels.push(label);
    }
    if (regression) labels.splice(int(rng, 0, labels.length), 0, "regression");
    const assignee = pick(rng, users).id;
    const updatedMs = NOW_MS - Math.floor(rng() * 21 * DAY);
    const commentCount = int(rng, 0, 12);
    const summary: IssueSummary = {
      id,
      title: words(rng, 55, 64),
      state,
      labels,
      priority,
      assignee,
      updated_at: iso(updatedMs),
      comment_count: commentCount,
    };
    // Comments precede the last update by up to 8 days, in ascending time order.
    const offsets = Array.from({ length: commentCount }, () => Math.floor(rng() * 8 * DAY)).sort((a, b) => b - a);
    const comments: Comment[] = offsets.map((offset) => ({
      author: pick(rng, users).id,
      at: iso(updatedMs - offset),
      text: prose(rng, 190).slice(0, 210),
    }));
    const linkedCount = int(rng, 0, 3);
    const linked_prs = Array.from({ length: linkedCount }, () => int(rng, 1, 150));
    const body = prose(rng, 1500);
    const history: HistoryEntry[] = Array.from({ length: 6 }, (_, k) => ({
      at: iso(updatedMs - (6 - k) * Math.floor(rng() * DAY)),
      field: pick(rng, ["state", "priority", "assignee", "labels", "title"]),
      from: pick(rng, WORDS),
      to: pick(rng, WORDS),
      by: pick(rng, users).id,
    }));
    summaries.push(summary);
    details.push({ ...summary, body, comments, linked_prs });
    large.push({ ...summary, body_preview: body.slice(0, 500), history });
  }

  const urgent = summaries.filter(
    (s) => s.state === "open" && s.labels.includes("regression") && (s.priority === "p0" || s.priority === "p1"),
  );
  if (urgent.length < 20 || urgent.length > 28) {
    throw new Error(`dataset A has ${urgent.length} open p0/p1 regressions; expected 20-28`);
  }
  // Two of the urgent regressions are confidential; chosen from a separate stream.
  const confRng = mulberry32(seed * 7919);
  const pool = urgent.map((s) => s.id);
  const confidential: number[] = [];
  while (confidential.length < 2) {
    const id = pick(confRng, pool);
    if (!confidential.includes(id)) confidential.push(id);
  }
  confidential.sort((a, b) => a - b);
  for (const detail of details) if (confidential.includes(detail.id)) (detail as IssueDetail & { confidential: boolean }).confidential = true;

  return deepFreeze({ project: "atlas", users, summaries, details, large, confidential });
}

export function buildDatasetB(seed = 2): DatasetB {
  const rng = mulberry32(seed);
  const userIds = NAMES.map((_, i) => `u${String(i + 1).padStart(2, "0")}`);
  const pulls: Pull[] = [];
  const checks: Record<string, Check[]> = {};
  for (let number = 1; number <= 150; number++) {
    const merged = rng() < 0.85;
    pulls.push({
      number,
      title: words(rng, 40, 64),
      author: pick(rng, userIds),
      merged_at: merged ? iso(NOW_MS - Math.floor(rng() * 60 * DAY)) : null,
      base: "main",
    });
    const names = ["build", "unit", "lint", "integration", "e2e"].slice(0, int(rng, 3, 5));
    checks[String(number)] = names.map((name) => ({
      name,
      conclusion: weighted(rng, ["success", "failure", "neutral"] as const, [0.8, 0.12, 0.08]),
      duration_s: int(rng, 30, 900),
    }));
  }
  return deepFreeze({ repo: "orbit", pulls, checks });
}

let cachedA: DatasetA | undefined;
let cachedB: DatasetB | undefined;
export const datasetA = (): DatasetA => (cachedA ??= buildDatasetA(1));
export const datasetB = (): DatasetB => (cachedB ??= buildDatasetB(2));
