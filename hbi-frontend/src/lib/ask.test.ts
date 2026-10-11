import { describe, expect, it } from "vitest";
import { askState } from "./ask";

describe("askState", () => {
  it("is closed without the param", () => {
    expect(askState({})).toEqual({ open: false });
    expect(askState({ ask: "" })).toEqual({ open: false });
  });

  it("opens on the current conversation with ask=1", () => {
    expect(askState({ ask: "1" })).toEqual({ open: true });
  });

  it("opens a named conversation", () => {
    expect(askState({ ask: "jobsearch-2026-10-11" })).toEqual({ open: true, session: "jobsearch-2026-10-11" });
  });
});
