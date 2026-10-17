import { describe, expect, it } from "vitest";
import { buildProgramTask, readProgramDraft } from "./programDraft";

describe("program drafts (PROGRAMS_PLAN Phase 2b)", () => {
  it("gives the agent the schema, what exists and the previous refusal", () => {
    const t = buildProgramTask({
      request: "track grid outages",
      current: { name: "grid" },
      schema: { title: "Program" },
      programs: [{ name: "nia", label: "NIA", kind: "department", parent: null }],
      freeWorkflows: [{ name: "grid-watch", description: "watches", enabled: false }],
      topics: [{ name: "batteries-materials", label: "Batteries" }],
      agents: ["researcher"],
      fix: { previous: "{}", errors: "kind 'team' is not one of …" },
    });
    for (const want of ["track grid outages", '{"title":"Program"}', "- nia: NIA (department)", "- grid-watch (off): watches", "batteries-materials (Batteries)", "kind 'team'"]) {
      expect(t).toContain(want);
    }
  });

  it("reads the program out of an answer and keeps only form fields", () => {
    const r = readProgramDraft('Here:\n```json\n{"name": "grid", "kind": "department", "status": "closed", "charter": "c"}\n```');
    expect(typeof r).not.toBe("string");
    if (typeof r !== "string") expect(r.draft).toEqual({ name: "grid", kind: "department", charter: "c" });
    expect(readProgramDraft("no json")).toMatch(/No JSON object/);
    expect(readProgramDraft('{"verdict": "x"}')).toMatch(/not a program/);
  });
});
