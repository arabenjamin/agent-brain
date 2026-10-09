import { describe, expect, it } from "vitest";
import { assessedShare, type TopicMap } from "../api/topics";
import { buildMap } from "../lib/topicMap";

const data: TopicMap = {
  topics: [
    { name: "geopolitics", label: "Geopolitics", notes: 100, week: 10 },
    { name: "us-politics", label: "US politics", notes: 50, week: 30 },
  ],
  entities: [
    { id: "iran", name: "iran", entity_type: "location", mentions: 80, week: 5, claims: 4, certainty: 0.25 },
    { id: "trump", name: "donald trump", entity_type: "person", mentions: 200, week: 40, claims: 10, certainty: 0.85 },
    { id: "orphan", name: "no link", entity_type: "concept", mentions: 999, week: 0, claims: 0, certainty: null },
  ],
  links: [
    { topic: "geopolitics", entity: "iran", notes: 40 },
    { topic: "geopolitics", entity: "trump", notes: 20 },
    { topic: "us-politics", entity: "trump", notes: 35 },
  ],
  per_topic: 25,
  hidden_entity_types: ["url", "date"],
  hidden_by_user: [],
};

describe("topic map", () => {
  it("makes a hub per topic and links entities to theirs", () => {
    const g = buildMap(data, "connections");
    expect(g.nodes.filter((n) => n.kind === "topic").map((n) => n.id)).toEqual(["topic:geopolitics", "topic:us-politics"]);
    expect(g.nodes.some((n) => n.id === "orphan")).toBe(false);
    expect(g.links).toHaveLength(3);
  });

  it("marks an entity two topics share as a bridge, coloured by its strongest topic", () => {
    const g = buildMap(data, "connections");
    const trump = g.nodes.find((n) => n.id === "trump")!;
    const usp = g.nodes.find((n) => n.id === "topic:us-politics")!;
    expect(trump.topics).toBe(2);
    expect(trump.hue).toBe(usp.hue);
    expect(g.nodes.find((n) => n.id === "iran")!.topics).toBe(1);
  });

  it("sizes by the chosen metric", () => {
    const size = (by: "connections" | "certainty" | "activity", id: string) => buildMap(data, by).nodes.find((n) => n.id === id)!.val;
    expect(size("connections", "trump")).toBeGreaterThan(size("connections", "iran"));
    expect(size("certainty", "trump")).toBeGreaterThan(size("certainty", "iran"));
    expect(size("activity", "trump")).toBeGreaterThan(size("activity", "iran"));
    // Topic hubs have no certainty: fixed size.
    expect(size("certainty", "topic:geopolitics")).toBe(size("certainty", "topic:us-politics"));
  });

  it("reports how far classification has got", () => {
    expect(assessedShare({ notes: 100, with_topic: 50, no_topic: 20, waiting: 25, failed: 5, skipped: 0 })).toBe(0.75);
    // Skipped notes are out of scope, not waiting.
    expect(assessedShare({ notes: 120, with_topic: 50, no_topic: 20, waiting: 25, failed: 5, skipped: 20 })).toBe(0.75);
    expect(assessedShare({ notes: 0, with_topic: 0, no_topic: 0, waiting: 0, failed: 0, skipped: 0 })).toBe(1);
  });
});
