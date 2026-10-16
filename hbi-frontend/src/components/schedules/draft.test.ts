import { describe, expect, it } from "vitest";
import { buildDraftTask, toolSignatures } from "./draft";

const schemas = {
  reason: {
    type: "object",
    properties: { context: { type: "string" }, mode: { type: "string", enum: ["infer", "explain"] }, question: { type: "string" } },
    required: ["question"],
  },
  search_web: { type: "object", properties: { query: { type: "string" }, max_results: { type: "integer" } }, required: ["query"] },
};

describe("toolSignatures", () => {
  it("lists required arguments first, with their types or choices", () => {
    expect(toolSignatures(["reason", "search_web", "mystery"], schemas)).toBe(
      "reason(question*: string, context: string, mode: infer|explain)\nsearch_web(query*: string, max_results: integer)\nmystery",
    );
  });
});

describe("buildDraftTask", () => {
  it("carries the workflow being edited, so an edit is not a fresh draft", () => {
    const t = buildDraftTask({
      request: "also search the news",
      current: { name: "weekly-x", every: "1w", steps: [{ tool_name: "search_web", arguments: { query: "x" } }] },
      tools: ["reason"],
      schemas,
    });
    expect(t).toContain("Change this scheduled workflow");
    expect(t).toContain('"name": "weekly-x"');
    expect(t).toContain("reason(question*: string");
  });

  it("drafts from scratch without one, and names a refused draft's errors", () => {
    const t = buildDraftTask({ request: "daily AI news", tools: [], schemas: {}, fix: { previous: "{}", errors: "question is required" } });
    expect(t).toMatch(/^Draft a scheduled workflow/);
    expect(t).toContain("question is required");
  });
});
