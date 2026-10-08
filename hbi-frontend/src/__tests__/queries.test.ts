import { describe, expect, it, vi } from "vitest";

// queries.ts relies on this: observers of one key realign their poll timers
// whenever the key's data updates, so N views polling the same status
// endpoint cost one request per tick, not N. A library upgrade that changes
// that must fail here rather than silently multiply the brain's load.
describe("shared polling", () => {
  // Real timers: query-core's timeoutManager binds the global timer
  // functions at import, so fake timers never reach it. Intervals are scaled
  // down (50 ms ≙ 5 s) to keep the test fast.
  // query-core decides at import that a missing `window` means SSR, where it
  // never polls; stub it before importing.
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  vi.stubGlobal("window", globalThis);

  async function fetchesOver500ms(keys: string[][]): Promise<number> {
    const { QueryClient, QueryObserver } = await import("@tanstack/react-query");
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const fetchFn = vi.fn(async () => ({ ok: true }));
    const unsubs: (() => void)[] = [];
    for (const [i, queryKey] of keys.entries()) {
      if (i > 0) await sleep(13 + 8 * i); // mount at different times
      unsubs.push(new QueryObserver(client, { queryKey, queryFn: fetchFn, refetchInterval: 50 }).subscribe(() => {}));
    }
    await sleep(60);
    fetchFn.mockClear();
    await sleep(500);
    unsubs.forEach((u) => u());
    client.clear();
    return fetchFn.mock.calls.length;
  }

  it("three observers of one key fetch once per interval", async () => {
    // ~10 intervals elapse.
    const n = await fetchesOver500ms([["queue"], ["queue"], ["queue"]]);
    expect(n).toBeGreaterThanOrEqual(6);
    expect(n).toBeLessThanOrEqual(12);
  });

  it("control: three different keys fetch three times as often", async () => {
    expect(await fetchesOver500ms([["a"], ["b"], ["c"]])).toBeGreaterThanOrEqual(24);
  });
});
