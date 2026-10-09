/**
 * Schedules grouped by their declared topic or kind (HBI plan §9.5), and the
 * words for what their runs' notes were actually about. Every schedule lands
 * in exactly one group; the ones that declare none are a group of their own,
 * shown last, never hidden.
 */

export type GroupBy = "topic" | "kind" | "none";

/** The kinds a schedule may declare; mirrors `SCHEDULE_KINDS` in the brain. */
export const SCHEDULE_KINDS = ["monitor", "brief", "learning", "maintenance", "dataset"] as const;

export const KIND_LABELS: Record<string, string> = {
  monitor: "Monitors",
  brief: "Briefs",
  learning: "Learning",
  maintenance: "Maintenance",
  dataset: "Datasets",
};

export interface Groupable {
  id: string;
  enabled: boolean;
  topic?: string | null;
  kind?: string | null;
}

export interface Group<T> {
  /** The topic or kind, or "" for the group of those declaring none. */
  key: string;
  label: string;
  rows: T[];
  on: number;
  off: number;
}

/**
 * `rows` in groups by `by`, keeping each group's rows in the order given.
 * Named groups sort by label; the "none" group comes last. `by: "none"` is
 * one group holding everything.
 */
export function groupSchedules<T extends Groupable>(
  rows: T[],
  by: GroupBy,
  topicLabels: Record<string, string> = {},
): Group<T>[] {
  const count = (key: string, label: string, list: T[]): Group<T> => ({
    key,
    label,
    rows: list,
    on: list.filter((r) => r.enabled).length,
    off: list.filter((r) => !r.enabled).length,
  });
  if (by === "none") return [count("", "All schedules", rows)];
  const buckets = new Map<string, T[]>();
  for (const r of rows) {
    const key = ((by === "topic" ? r.topic : r.kind) ?? "").trim();
    buckets.set(key, [...(buckets.get(key) ?? []), r]);
  }
  const label = (key: string) =>
    !key
      ? by === "topic" ? "No topic" : "No kind"
      : by === "topic" ? topicLabels[key] ?? key : KIND_LABELS[key] ?? key;
  const named = [...buckets.entries()]
    .filter(([k]) => k)
    .map(([k, list]) => count(k, label(k), list))
    .sort((a, b) => a.label.localeCompare(b.label));
  const none = buckets.get("");
  return none ? [...named, count("", label(""), none)] : named;
}

/** The computed side for one schedule (`/api/work/schedules/topic-mix`). */
export interface TopicMix {
  id: string;
  topic: string | null;
  kind: string | null;
  runs: number;
  jobs: number;
  /** Notes traced to it, each by the first route that reached it. */
  notes: number;
  /** How many notes each route found: the run stamp, a kept job's result, its steps' source_context, or a claim asserted in one of those. */
  routes: { run_stamp: number; job_result: number; source_context: number; claim_of: number };
  /** The literal source_context values its steps write. */
  source_contexts: string[];
  topics: { topic: string; notes: number }[];
  fits_none: number;
  waiting: number;
  skipped: number;
  verdict: "too_few" | "matches" | "mismatch" | "suggest" | "spans";
  /** The topic most notes are in (mismatch), or the one suggested. */
  other_topic: string | null;
}

/** One line on how the declared topic compares with the notes; null when there is nothing to say. */
export function mixFlag(m: TopicMix | undefined, topicLabels: Record<string, string> = {}): { tone: "yellow" | "accent"; text: string } | null {
  if (!m || !m.other_topic) return null;
  const name = topicLabels[m.other_topic] ?? m.other_topic;
  const n = m.topics.find((t) => t.topic === m.other_topic)?.notes ?? 0;
  const of = m.topics.reduce((a, t) => a + t.notes, 0) + m.fits_none;
  if (m.verdict === "mismatch") return { tone: "yellow", text: `notes mostly ${name} (${n} of ${of})` };
  if (m.verdict === "suggest") return { tone: "accent", text: `suggest ${name} (${n} of ${of} notes)` };
  return null;
}
