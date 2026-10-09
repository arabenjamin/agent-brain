import { describe, expect, it } from "vitest";
import { groupSchedules, mixFlag, type TopicMix } from "./scheduleGroups";

const rows = [
  { id: "a", enabled: true, topic: "ai-ml", kind: "monitor" },
  { id: "b", enabled: false, topic: null, kind: "brief" },
  { id: "c", enabled: true, topic: "agent-brain", kind: "maintenance" },
  { id: "d", enabled: true, topic: "ai-ml", kind: null },
  { id: "e", enabled: true, topic: " ", kind: "brief" },
];

describe("schedule groups", () => {
  it("put every schedule in exactly one group, with 'no topic' last", () => {
    const g = groupSchedules(rows, "topic", { "ai-ml": "AI & machine learning", "agent-brain": "The brain itself" });
    expect(g.map((x) => x.label)).toEqual(["AI & machine learning", "The brain itself", "No topic"]);
    expect(g.flatMap((x) => x.rows.map((r) => r.id)).sort()).toEqual(["a", "b", "c", "d", "e"]);
    expect(g[2]).toMatchObject({ key: "", on: 1, off: 1 });
  });

  it("group by kind, and by nothing", () => {
    expect(groupSchedules(rows, "kind").map((x) => `${x.label}:${x.rows.length}`)).toEqual(["Briefs:2", "Maintenance:1", "Monitors:1", "No kind:1"]);
    expect(groupSchedules(rows, "none")).toHaveLength(1);
    expect(groupSchedules([], "topic")).toEqual([]);
  });
});

describe("mix flag", () => {
  const base: TopicMix = { id: "a", topic: "cybersecurity", kind: "monitor", runs: 5, jobs: 10, notes: 9, routes: { run_stamp: 0, job_result: 2, source_context: 3, claim_of: 4 }, source_contexts: ["x"],
    topics: [{ topic: "ai-ml", notes: 7 }, { topic: "cybersecurity", notes: 1 }], fits_none: 1, waiting: 0, skipped: 0,
    verdict: "mismatch", other_topic: "ai-ml" };

  it("names what the notes are mostly about", () => {
    expect(mixFlag(base, { "ai-ml": "AI & ML" })).toEqual({ tone: "yellow", text: "notes mostly AI & ML (7 of 9)" });
    expect(mixFlag({ ...base, topic: null, verdict: "suggest" })).toEqual({ tone: "accent", text: "suggest ai-ml (7 of 9 notes)" });
    expect(mixFlag({ ...base, verdict: "matches", other_topic: null })).toBeNull();
    expect(mixFlag(undefined)).toBeNull();
  });
});
