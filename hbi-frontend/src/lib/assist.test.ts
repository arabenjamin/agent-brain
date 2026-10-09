import { describe, expect, it } from "vitest";
import { answerBody, extractJson, parseRunAgentResult } from "./assist";

const wrapped = (o: unknown, isError = false) => ({
  result: { isError, content: [{ type: "text", text: typeof o === "string" ? o : JSON.stringify(o) }] },
});

describe("run_agent result", () => {
  it("reads the answer, model and trace", () => {
    const r = parseRunAgentResult(
      wrapped({ answer: "done", ok: true, agent: "schedule-author", model: "gemma4-e4b",
        tool_trace: [{ tool: "context", success: true }, { tool: "neo4j_query", success: false }] }),
    );
    expect(r).toEqual({ answer: "done", ok: true, agent: "schedule-author", model: "gemma4-e4b",
      trace: [{ tool: "context", success: true }, { tool: "neo4j_query", success: false }] });
  });

  it("throws a rejection's reason", () => {
    expect(() => parseRunAgentResult(wrapped("'x' was asked to run on a model it cannot use", true))).toThrow(/cannot use/);
    expect(() => parseRunAgentResult({ error: { message: "bad" } })).toThrow("bad");
  });
});

describe("answer parts", () => {
  const answer =
    "[FINDINGS FROM THE `schedule-author` SUB-AGENT — relay these faithfully.]\n\n" +
    "Here:\n```json\n[{\"tool_name\":\"reason\"}]\n```\n\n" +
    "[TOOLS `schedule-author` ACTUALLY RAN: context ×1.]\n\nSOURCES:\n  [S1] https://x";

  it("strips the relay header and the receipt", () => {
    expect(answerBody(answer)).toBe("Here:\n```json\n[{\"tool_name\":\"reason\"}]\n```");
  });

  it("finds a fenced block before a bare one", () => {
    expect(extractJson(answer, "array")).toBe("[{\"tool_name\":\"reason\"}]");
    expect(extractJson("profile: {\"name\": \"a\"} end", "object")).toBe("{\"name\": \"a\"}");
    expect(extractJson("nothing here", "array")).toBeNull();
  });
});
