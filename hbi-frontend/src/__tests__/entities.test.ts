import { describe, expect, it } from "vitest";
import { isMergedAway, REASON_LABEL } from "../api/entities";
import type { EntityDetail } from "../api/search";

describe("entity resolution client", () => {
  it("tells a merged-away id from an entity", () => {
    const merged = { merged_into: { alias: "trump", id: "e1", name: "donald trump" } };
    const entity = {
      entity: { id: "e1", name: "donald trump", entity_type: "person", created_at: null, mentions: 3 },
      notes: [], notes_shown: 0, co_mentioned: [],
    } satisfies EntityDetail;
    expect(isMergedAway(merged)).toBe(true);
    expect(isMergedAway(entity)).toBe(false);
    expect(isMergedAway(undefined)).toBe(false);
  });

  it("labels every proposal reason", () => {
    expect(Object.keys(REASON_LABEL).sort()).toEqual(["acronym", "plural", "surname", "title"]);
  });
});
