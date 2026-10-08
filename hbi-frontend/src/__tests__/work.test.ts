import { describe, expect, it } from "vitest";
import { bucketCounts, occurrences } from "../lib/timeline";
import { isTodoOverdue, localDate } from "../components/todos/model";
import { fmtEvery, statusTone } from "../lib/status";

const H = 3_600_000;
const from = Date.parse("2026-10-08T00:00:00Z");

describe("occurrences", () => {
  it("lists every run in the window from next_run_at on", () => {
    const o = occurrences("2026-10-08T01:00:00Z", 6 * 3600, from, from + 24 * H);
    expect(o.map((x) => (x.at - from) / H)).toEqual([1, 7, 13, 19]);
    expect(o.every((x) => !x.due)).toBe(true);
  });

  it("draws a past-due schedule at the window start, flagged, then resumes", () => {
    const o = occurrences("2026-10-07T20:00:00Z", 12 * 3600, from, from + 24 * H);
    expect(o[0]).toEqual({ at: from, due: true });
    expect((o[1].at - from) / H).toBe(12);
  });

  it("returns nothing for a run beyond the window, a bad interval or no timestamp", () => {
    expect(occurrences("2026-10-10T00:00:00Z", 3600, from, from + 24 * H)).toEqual([]);
    expect(occurrences("2026-10-08T01:00:00Z", 0, from, from + 24 * H)).toEqual([]);
    expect(occurrences(null, 3600, from, from + 24 * H)).toEqual([]);
  });

  it("caps a very frequent schedule", () => {
    expect(occurrences("2026-10-08T00:00:00Z", 1, from, from + 24 * H, 50)).toHaveLength(50);
  });
});

describe("bucketCounts", () => {
  it("counts runs per bucket across schedules", () => {
    const a = occurrences("2026-10-08T01:00:00Z", 6 * 3600, from, from + 24 * H);
    const b = occurrences("2026-10-08T01:30:00Z", 24 * 3600, from, from + 24 * H);
    const c = bucketCounts([a, b], from, from + 24 * H, 4);
    // a runs at 1, 7, 13, 19 h; b at 1.5 h — both of the first two land in bucket 0.
    expect(c).toEqual([2, 1, 1, 1]);
  });
});

describe("isTodoOverdue", () => {
  // 2026-10-08 22:00 local is still the 8th, whatever the zone.
  const evening = new Date(2026, 9, 8, 22, 0).getTime();

  it("is not overdue on its due date, even in the evening west of UTC (the old bug)", () => {
    expect(isTodoOverdue({ due_at: "2026-10-08T00:00:00Z", status: "pending" }, evening)).toBe(false);
  });

  it("is overdue once the date has passed, and never when done or undated", () => {
    expect(isTodoOverdue({ due_at: "2026-10-07T00:00:00Z", status: "pending" }, evening)).toBe(true);
    expect(isTodoOverdue({ due_at: "2026-10-07T00:00:00Z", status: "done" }, evening)).toBe(false);
    expect(isTodoOverdue({ due_at: null, status: "pending" }, evening)).toBe(false);
  });

  it("formats the local date", () => {
    expect(localDate(evening)).toBe("2026-10-08");
  });
});

describe("status helpers", () => {
  it("colours every never-runs-again job state red", () => {
    expect(["failed", "dead", "dead_letter"].map(statusTone)).toEqual(["red", "red", "red"]);
    expect(statusTone("something new")).toBe("muted");
  });

  it("formats cadences", () => {
    expect(fmtEvery(300)).toBe("every 5 min");
    expect(fmtEvery(43200)).toBe("every 12 h");
    expect(fmtEvery(604800)).toBe("every 7 d");
  });
});
