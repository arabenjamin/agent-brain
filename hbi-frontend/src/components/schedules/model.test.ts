import { describe, expect, it } from "vitest";
import { jsonToStepDrafts, providerLabel, stepDraftsToJson } from "./model";

describe("step drafts", () => {
  it("write back every field the editor doesn't edit", () => {
    const steps = [
      {
        tool_name: "search_web",
        arguments: { query: "{{goal_topic}}", num_results: 5 },
        provider_hint: "ollama",
        priority: 2,
        data_class: "public",
        model: "gemma4-e4b",
        max_attempts: 3,
        description: "Find this week's coverage",
      },
      { tool_name: "reason", arguments: { context: "{{_prev}}" }, is_evaluator: true, min_score: 4 },
    ];
    const drafts = jsonToStepDrafts(JSON.stringify(steps))!;
    expect(JSON.parse(stepDraftsToJson(drafts))).toEqual(steps);
  });

  it("keep a step without a provider hint on cloud", () => {
    const [d] = jsonToStepDrafts(JSON.stringify([{ tool_name: "reason" }]))!;
    expect(d.provider_hint).toBe("");
    expect(JSON.parse(stepDraftsToJson([d]))[0]).not.toHaveProperty("provider_hint");
  });

  it("rewrite an old tool/args step in the fields the queue reads", () => {
    const [d] = jsonToStepDrafts(JSON.stringify([{ tool: "store_note", args: { content: "x" }, purpose: "log" }]))!;
    expect(JSON.parse(stepDraftsToJson([d]))[0]).toEqual({ tool_name: "store_note", arguments: { content: "x" }, purpose: "log" });
  });
});

describe("providerLabel", () => {
  it("says only local vs cloud changes where a step runs", () => {
    expect(providerLabel("ollama")).toBe("Local model");
    expect(providerLabel("", "gpt-oss:120b-cloud")).toBe("Cloud: the brain's active model (gpt-oss:120b-cloud)");
    expect(providerLabel("anthropic", "gpt-oss:120b-cloud")).toBe("anthropic: also cloud, on the brain's active model (gpt-oss:120b-cloud)");
  });
});

describe("step model", () => {
  it("is edited, not carried in extra, and dropped when blank", () => {
    const [d] = jsonToStepDrafts(JSON.stringify([{ tool_name: "reason", model: "gpt-oss:120b-cloud", provider_hint: "ollama-cloud" }]))!;
    expect(d.model).toBe("gpt-oss:120b-cloud");
    expect(d.extra).not.toHaveProperty("model");
    expect(JSON.parse(stepDraftsToJson([{ ...d, model: " " }]))[0]).not.toHaveProperty("model");
  });
});
