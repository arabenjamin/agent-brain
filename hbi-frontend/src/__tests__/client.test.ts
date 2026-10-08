import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, apiJson, apiRequest, query } from "../api/client";

const store = new Map<string, string>();

beforeEach(() => {
  store.clear();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, v),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function mockFetch(res: Response) {
  const fn = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>(async () => res);
  vi.stubGlobal("fetch", fn);
  return fn;
}

function sentHeaders(fn: ReturnType<typeof mockFetch>): Record<string, string> {
  return (fn.mock.calls[0][1]?.headers ?? {}) as Record<string, string>;
}

describe("apiRequest", () => {
  it("prefixes the configured brain URL and sends the bearer key", async () => {
    store.set("brain_url", "http://brain:3001");
    store.set("api_key", "k1");
    const fn = mockFetch(new Response("{}"));
    await apiRequest("/api/notes");
    expect(fn.mock.calls[0][0]).toBe("http://brain:3001/api/notes");
    expect(sentHeaders(fn).Authorization).toBe("Bearer k1");
  });

  it("marks a string body as JSON", async () => {
    const fn = mockFetch(new Response("{}"));
    await apiRequest("/todos", { method: "POST", body: "{}" });
    expect(sentHeaders(fn)["Content-Type"]).toBe("application/json");
  });

  it("does not add a Content-Type to a bodiless GET", async () => {
    const fn = mockFetch(new Response("{}"));
    await apiRequest("/api/tasks");
    expect(sentHeaders(fn)["Content-Type"]).toBeUndefined();
  });

  it("keeps a caller-supplied Content-Type", async () => {
    const fn = mockFetch(new Response("{}"));
    await apiRequest("/x", { method: "POST", body: "a=b", headers: { "content-type": "text/plain" } });
    const h = sentHeaders(fn);
    expect(h["content-type"]).toBe("text/plain");
    expect(h["Content-Type"]).toBeUndefined();
  });

  it("returns a non-2xx response instead of throwing", async () => {
    mockFetch(new Response("nope", { status: 500 }));
    const res = await apiRequest("/x");
    expect(res.status).toBe(500);
  });
});

describe("apiJson", () => {
  it("parses a 2xx body", async () => {
    mockFetch(new Response(JSON.stringify({ count: 2 })));
    await expect(apiJson<{ count: number }>("/x")).resolves.toEqual({ count: 2 });
  });

  it("throws ApiError carrying the brain's {error} message and status", async () => {
    mockFetch(new Response(JSON.stringify({ error: "Not found" }), { status: 404 }));
    const err = await apiJson("/api/notes/missing").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ status: 404, path: "/api/notes/missing", message: "Not found" });
  });

  it("falls back to the body text, then the status line", async () => {
    mockFetch(new Response("plain failure", { status: 502 }));
    await expect(apiJson("/x")).rejects.toMatchObject({ message: "plain failure" });
    mockFetch(new Response("", { status: 503, statusText: "Service Unavailable" }));
    await expect(apiJson("/x")).rejects.toMatchObject({ message: "503 Service Unavailable" });
  });

  it("returns undefined for 204", async () => {
    mockFetch(new Response(null, { status: 204 }));
    await expect(apiJson("/x", { method: "DELETE" })).resolves.toBeUndefined();
  });
});

describe("query", () => {
  it("drops empty values and encodes the rest", () => {
    expect(query({ status: undefined, limit: 50, q: "a b", empty: "", n: null })).toBe("?limit=50&q=a+b");
  });

  it("is empty when nothing is set", () => {
    expect(query({ status: undefined })).toBe("");
  });
});
