import { describe, expect, it } from "vitest";
import type { ChatEvent } from "../api/chat";
import { buildDock, isCited, isWriteTool, linkKey } from "./chatDock";

const NOTE = "7d75fff5-1111-2222-3333-444455556666";
const OTHER = "aaaaaaaa-1111-2222-3333-444455556666";
const TASK = "bbbbbbbb-1111-2222-3333-444455556666";

const turn: ChatEvent[] = [
  { type: "thinking", content: "⚙ provider=ollama-cloud | model=gpt-oss" },
  { type: "tool_call", tool: "search_notes", args: { query: "reticulum" } },
  { type: "tool_result", tool: "search_notes", success: true, preview: `[{"id":"${NOTE}"},{"id":"${OTHER}"}]`, model_saw_chars: 40 },
  { type: "tool_call", tool: "search_web", args: { query: "reticulum mtu" } },
  { type: "tool_result", tool: "search_web", success: true, preview: "1. https://reticulum.network/manual/understanding.html — MTU is 500 bytes" },
  { type: "tool_call", tool: "create_task", args: { goal: "read the RNS spec" } },
  { type: "tool_result", tool: "create_task", success: true, preview: `{"task_id":"${TASK}"}` },
  { type: "tool_call", tool: "http_request", args: { url: "https://example.com/x" } },
  { type: "tool_result", tool: "http_request", success: false, preview: "timeout" },
];

describe("buildDock", () => {
  const answer =
    "Reticulum's MTU is 500 bytes (note:7d75fff5, https://reticulum.network/manual/understanding.html). " +
    "See also https://made-up.example.org/page. I queued a task.";
  const d = buildDock(turn, answer);

  it("lists what ran, in order, with failures and the model's ration", () => {
    expect(d.runs.map((r) => `${r.tool}:${r.ok ? "ok" : "failed"}`)).toEqual([
      "search_notes:ok", "search_web:ok", "create_task:ok", "http_request:failed",
    ]);
    expect(d.runs[0].args).toEqual({ query: "reticulum" });
    expect(d.runs[0].modelSaw).toBe(40);
    expect(d.runs[1].modelSaw).toBeNull();
  });

  it("splits ids into what the turn wrote and what it read", () => {
    expect(d.wrote).toEqual([TASK]);
    expect(d.read).toEqual([NOTE, OTHER]);
  });

  it("finds short-id citations and checks links against tool output", () => {
    expect(d.cited).toEqual(["7d75fff5"]);
    expect(isCited(NOTE, d.cited)).toBe(true);
    expect(isCited(OTHER, d.cited)).toBe(false);
    expect(d.links).toEqual([
      { url: "https://reticulum.network/manual/understanding.html", from: "search_web" },
      { url: "https://made-up.example.org/page", from: null },
    ]);
  });

  it("knows a restored turn has no events to check", () => {
    const restored = buildDock([{ type: "message", content: answer }], answer);
    expect(restored.hasEvents).toBe(false);
    expect(restored.runs).toEqual([]);
    expect(restored.links.every((l) => l.from === null)).toBe(true);
  });
});

describe("helpers", () => {
  it("treats manage_* as a write only for writing actions", () => {
    expect(isWriteTool("store_note", {})).toBe(true);
    expect(isWriteTool("manage_scheduled_task", { action: "upsert" })).toBe(true);
    expect(isWriteTool("manage_scheduled_task", { action: "list" })).toBe(false);
    expect(isWriteTool("search_notes", {})).toBe(false);
  });
  it("compares links without scheme, fragment or trailing slash", () => {
    expect(linkKey("HTTPS://Example.com/a/#top")).toBe("example.com/a");
    expect(linkKey("http://example.com/a.")).toBe("example.com/a");
  });
});
