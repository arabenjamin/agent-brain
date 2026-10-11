import { describe, expect, it } from "vitest";
import { diffCounts } from "./system";

describe("diffCounts", () => {
  it("counts added and removed per category, in display order", () => {
    expect(diffCounts({
      workflows: { added: ["weekly"], removed: ["old"] },
      tools: { added: ["a", "b"], removed: [] },
      mcp_servers: { added: [], removed: ["x"] },
    })).toBe("+2 tools, +1 workflows, −1 workflows, −1 MCP servers");
  });
  it("is empty when nothing changed", () => {
    expect(diffCounts({})).toBe("");
  });
});
