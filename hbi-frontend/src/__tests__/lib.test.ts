import { describe, expect, it } from "vitest";
import { asEntityId } from "../lib/ids";
import { formatLocal, parseTime, relTime } from "../lib/time";

describe("asEntityId", () => {
  it("recognises a UUID with surrounding whitespace", () => {
    expect(asEntityId("  867f7a9d-3b11-4158-ba12-b6f167c0f49a \n")).toBe("867f7a9d-3b11-4158-ba12-b6f167c0f49a");
  });

  it("lowercases so a pasted upper-case id still matches", () => {
    expect(asEntityId("867F7A9D-3B11-4158-BA12-B6F167C0F49A")).toBe("867f7a9d-3b11-4158-ba12-b6f167c0f49a");
  });

  it("rejects phrases and partial ids", () => {
    expect(asEntityId("donald trump")).toBeNull();
    expect(asEntityId("867f7a9d-3b11-4158")).toBeNull();
    expect(asEntityId("id 867f7a9d-3b11-4158-ba12-b6f167c0f49a")).toBeNull();
  });
});

describe("parseTime", () => {
  it("parses Neo4j nanosecond timestamps", () => {
    expect(parseTime("2026-10-08T18:15:01.419768250Z")?.toISOString()).toBe("2026-10-08T18:15:01.419Z");
  });

  it("strips a [Zone] suffix that Date rejects", () => {
    expect(parseTime("2026-10-08T18:15:01Z[UTC]")?.toISOString()).toBe("2026-10-08T18:15:01.000Z");
  });

  it("returns null for absent or garbage input", () => {
    expect(parseTime(null)).toBeNull();
    expect(parseTime("")).toBeNull();
    expect(parseTime("yesterday")).toBeNull();
  });
});

describe("relTime / formatLocal", () => {
  const now = Date.parse("2026-10-08T12:00:00Z");

  it("buckets elapsed time", () => {
    expect(relTime("2026-10-08T11:59:58Z", now)).toBe("just now");
    expect(relTime("2026-10-08T11:59:30Z", now)).toBe("30s ago");
    expect(relTime("2026-10-08T11:55:00Z", now)).toBe("5m ago");
    expect(relTime("2026-10-08T09:00:00Z", now)).toBe("3h ago");
    expect(relTime("2026-10-05T12:00:00Z", now)).toBe("3d ago");
  });

  it("says so instead of a negative age", () => {
    expect(relTime("2026-10-09T12:00:00Z", now)).toBe("in the future");
  });

  it("names a missing timestamp rather than inventing one", () => {
    expect(relTime(undefined, now)).toBe("never");
    expect(formatLocal(undefined)).toBe("—");
  });
});
