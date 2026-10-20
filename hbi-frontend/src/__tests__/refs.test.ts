import { describe, expect, it } from "vitest";
import { refKind, tagLabel } from "../api/refs";

describe("todo tags as references", () => {
  it("knows a reference by its kind", () => {
    expect(refKind("workflow:daily-news")).toBe("workflow");
    expect(refKind("entity:new york city")).toBe("entity");
    expect(refKind("HBI")).toBeNull();
    expect(refKind("colour:red")).toBeNull();
  });

  it("labels a tag by its name, an old free word as itself", () => {
    expect(tagLabel("agent:researcher")).toBe("researcher");
    expect(tagLabel("entity:a:b")).toBe("a:b");
    expect(tagLabel("frontend")).toBe("frontend");
  });
});
