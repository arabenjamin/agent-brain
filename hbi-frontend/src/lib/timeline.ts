/**
 * When enabled schedules will run inside a window, for the Scheduled lane's
 * timeline. A schedule runs at `next_run_at` and every `interval_seconds`
 * after it. One already past due runs at the next scheduler tick, which is
 * drawn at the window's start and flagged.
 */
import { parseTime } from "./time";

export interface Occurrence {
  at: number;
  /** Past due: it will run at the next tick, not at `at`. */
  due: boolean;
}

export function occurrences(
  nextRunAt: string | null | undefined,
  intervalSeconds: number,
  from: number,
  to: number,
  max = 500,
): Occurrence[] {
  const first = parseTime(nextRunAt)?.getTime();
  if (first === undefined || !(intervalSeconds > 0) || to <= from) return [];
  const step = intervalSeconds * 1000;
  const out: Occurrence[] = [];
  let t = first;
  if (t < from) {
    // Past due: one run as soon as possible, then the cadence resumes from it.
    out.push({ at: from, due: true });
    t = from + step;
  }
  for (; t <= to && out.length < max; t += step) out.push({ at: t, due: false });
  return out;
}

/** Counts per bucket across all schedules, to show where runs pile up. */
export function bucketCounts(all: Occurrence[][], from: number, to: number, buckets: number): number[] {
  const counts = new Array(buckets).fill(0) as number[];
  const width = (to - from) / buckets;
  for (const list of all) {
    for (const o of list) {
      const i = Math.min(buckets - 1, Math.max(0, Math.floor((o.at - from) / width)));
      counts[i] += 1;
    }
  }
  return counts;
}
