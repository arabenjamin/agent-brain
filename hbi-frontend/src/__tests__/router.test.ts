import { describe, expect, it } from "vitest";
import { applyUpdate, formatHash, parseHash } from "../router";

describe("parseHash", () => {
  it("reads the workspace and params", () => {
    expect(parseHash("#/knowledge?note=abc&x=1")).toEqual({
      path: "knowledge",
      params: { note: "abc", x: "1" },
    });
  });

  it("treats a missing or bare hash as the empty path", () => {
    expect(parseHash("")).toEqual({ path: "", params: {} });
    expect(parseHash("#")).toEqual({ path: "", params: {} });
    expect(parseHash("#/")).toEqual({ path: "", params: {} });
  });

  it("accepts a hash without the leading slash", () => {
    expect(parseHash("#tasks").path).toBe("tasks");
  });

  it("keeps only the first path segment", () => {
    expect(parseHash("#/work/scheduled?x=1").path).toBe("work");
  });

  it("decodes encoded param values", () => {
    expect(parseHash("#/knowledge?q=a%20b%26c").params.q).toBe("a b&c");
  });
});

describe("formatHash", () => {
  it("round-trips through parseHash", () => {
    const route = { path: "knowledge", params: { note: "n1", q: "a b&c" } };
    expect(parseHash(formatHash(route))).toEqual(route);
  });

  it("drops empty params and sorts keys so equal routes give equal hashes", () => {
    expect(formatHash({ path: "chat", params: { b: "2", a: "1", c: "" } })).toBe("#/chat?a=1&b=2");
  });

  it("omits the query for no params", () => {
    expect(formatHash({ path: "logs", params: {} })).toBe("#/logs");
  });
});

describe("applyUpdate", () => {
  const base = { path: "graph", params: { note: "n1", keep: "k" } };

  it("keeps params not mentioned", () => {
    expect(applyUpdate(base, { path: "chat" })).toEqual({ path: "chat", params: { note: "n1", keep: "k" } });
  });

  it("removes params set to null and sets new ones", () => {
    expect(applyUpdate(base, { params: { note: null, task: "t1" } })).toEqual({
      path: "graph",
      params: { keep: "k", task: "t1" },
    });
  });

  it("does not mutate the input", () => {
    applyUpdate(base, { params: { note: null } });
    expect(base.params.note).toBe("n1");
  });
});
