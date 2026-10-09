import { describe, expect, it } from "vitest";
import type { NeighbourhoodResponse } from "../api/related";
import { mergeNeighbourhood } from "../lib/neighbourhood";

const node = (id: string, mentions = 3) => ({ id, name: id, entity_type: "person", mentions, topic: "us-politics", hidden_from: [] });

const first: NeighbourhoodResponse = {
  center: "a",
  topic: null,
  total_related: 2,
  nodes: [node("a"), node("b"), node("c")],
  links: [
    { source: "a", target: "b", shared: 2, strength: 0.3 },
    { source: "a", target: "c", shared: 1, strength: 0.1 },
  ],
};

describe("mergeNeighbourhood", () => {
  it("builds the first graph around its centre", () => {
    const g = mergeNeighbourhood({ nodes: [], links: [] }, first, "a");
    expect(g.nodes.map((n) => n.id).sort()).toEqual(["a", "b", "c"]);
    expect(g.nodes.find((n) => n.id === "a")).toMatchObject({ center: true, expanded: true });
    expect(g.nodes.find((n) => n.id === "b")).toMatchObject({ center: false, expanded: false });
    expect(g.links).toHaveLength(2);
  });

  it("expanding a node adds its neighbours once, keeps positions and the centre", () => {
    const g1 = mergeNeighbourhood({ nodes: [], links: [] }, first, "a");
    g1.nodes.find((n) => n.id === "b")!.x = 42;
    // Laid-out links carry node objects, not ids.
    g1.links[0] = { ...g1.links[0], source: { id: "a" } as unknown as string, target: { id: "b" } as unknown as string };
    const second: NeighbourhoodResponse = {
      center: "b",
      topic: null,
      total_related: 2,
      nodes: [node("b"), node("a"), node("d")],
      links: [
        { source: "a", target: "b", shared: 2, strength: 0.3 },
        { source: "b", target: "d", shared: 4, strength: 0.5 },
      ],
    };
    const g2 = mergeNeighbourhood(g1, second, "a");
    expect(g2.nodes.map((n) => n.id).sort()).toEqual(["a", "b", "c", "d"]);
    expect(g2.links).toHaveLength(3);
    const b = g2.nodes.find((n) => n.id === "b")!;
    expect(b).toMatchObject({ expanded: true, x: 42, center: false });
    expect(g2.nodes.find((n) => n.id === "a")!.center).toBe(true);
  });
});
