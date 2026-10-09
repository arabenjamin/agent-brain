/**
 * The context dock under a chat answer, derived from the turn's own events
 * (HBI_REDESIGN_PLAN.md §2.5). Nothing here asks the model anything: what ran
 * is the `tool_call` / `tool_result` stream, what the turn wrote is the ids in
 * the results of tools that write, what it read is the ids in the rest, and a
 * link in the answer is checked against the text the tools actually returned.
 */
import type { ChatEvent } from "../api/chat";

export interface ToolRun {
  tool: string;
  ok: boolean;
  /** The arguments, as the model sent them. */
  args: unknown;
  /** Characters of the result the model was given, when less than all of it. */
  modelSaw: number | null;
  resultChars: number;
}

export interface DockLink {
  url: string;
  /** The first tool whose result or arguments contain it; null when none did. */
  from: string | null;
}

export interface Dock {
  /** False for a turn restored from history: its tool events weren't kept. */
  hasEvents: boolean;
  runs: ToolRun[];
  /** Ids in the results of tools that write (store_note, create_task, …). */
  wrote: string[];
  /** Ids in the results of every other tool. */
  read: string[];
  /** Ids (or 8+ character prefixes) the answer itself names. */
  cited: string[];
  links: DockLink[];
}

const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
/** `note:7d75fff5`, `task 1b2c3d4e`, `todo #…`: how answers cite by short id. */
const PREFIXED = /\b(?:note|task|todo|schedule|entity|job)s?[:\s#]+([0-9a-f]{8,36})\b/gi;
const URL_RE = /https?:\/\/[^\s<>"'`)\]]+/g;

/** Tools whose result ids are things the turn created or changed. */
const WRITE_VERB = /^(store|create|write|ingest|record|spawn|decompose|synthesize|consolidate|enqueue|construct|define|update)_/;
const WRITE_ACTIONS = new Set(["upsert", "create", "add", "update", "set"]);

export function isWriteTool(tool: string, args: unknown): boolean {
  if (WRITE_VERB.test(tool)) return true;
  if (tool.startsWith("manage_") && args && typeof args === "object") {
    const action = (args as Record<string, unknown>).action;
    return typeof action === "string" && WRITE_ACTIONS.has(action.toLowerCase());
  }
  return false;
}

function uuids(text: string): string[] {
  return [...text.matchAll(UUID)].map((m) => m[0].toLowerCase());
}

/** A URL as it is compared: no scheme, lowercase, no fragment or trailing slash or punctuation. */
export function linkKey(url: string): string {
  return url
    .replace(/[.,;:!?]+$/, "")
    .replace(/^https?:\/\//i, "")
    .replace(/#.*$/, "")
    .replace(/\/+$/, "")
    .toLowerCase();
}

function uniq(xs: string[]): string[] {
  return [...new Set(xs)];
}

/** The answer text of a turn: the final message, else the streamed tokens. */
export function answerText(events: ChatEvent[]): string {
  return (
    events.find((e) => e.type === "message")?.content ??
    events.filter((e) => e.type === "token").map((e) => e.content ?? "").join("")
  );
}

export function buildDock(events: ChatEvent[], answer: string): Dock {
  const runs: ToolRun[] = [];
  const wrote: string[] = [];
  const read: string[] = [];
  // Pair each result with the latest unanswered call of the same tool.
  const open: { tool: string; args: unknown }[] = [];
  const evidence: { tool: string; text: string }[] = [];
  for (const e of events) {
    if (e.type === "tool_call" && e.tool) {
      open.push({ tool: e.tool, args: e.args });
      evidence.push({ tool: e.tool, text: JSON.stringify(e.args ?? "") });
    } else if (e.type === "tool_result" && e.tool) {
      const i = open.findIndex((c) => c.tool === e.tool);
      const call = i >= 0 ? open.splice(i, 1)[0] : { tool: e.tool, args: undefined };
      const text = e.preview ?? "";
      runs.push({
        tool: e.tool,
        ok: !!e.success,
        args: call.args,
        modelSaw: typeof e.model_saw_chars === "number" ? e.model_saw_chars : null,
        resultChars: text.length,
      });
      evidence.push({ tool: e.tool, text });
      (e.success && isWriteTool(e.tool, call.args) ? wrote : read).push(...uuids(text));
    }
  }

  const cited = uniq([
    ...uuids(answer),
    ...[...answer.matchAll(PREFIXED)].map((m) => m[1].toLowerCase()),
  ]);
  const haystack = evidence.map((x) => ({ tool: x.tool, text: x.text.toLowerCase() }));
  const links = uniq([...answer.matchAll(URL_RE)].map((m) => m[0].replace(/[.,;:!?]+$/, ""))).map((url) => {
    const key = linkKey(url);
    return { url, from: haystack.find((h) => h.text.includes(key))?.tool ?? null };
  });

  const wroteSet = uniq(wrote);
  return {
    hasEvents: events.some((e) => e.type === "tool_call" || e.type === "tool_result" || e.type === "thinking"),
    runs,
    wrote: wroteSet,
    // An id a writer returned is not also "read".
    read: uniq(read).filter((id) => !wroteSet.includes(id)),
    cited,
    links,
  };
}

/** Is `id` (a full id) named by any of the answer's citations, full or prefix? */
export function isCited(id: string, cited: string[]): boolean {
  return cited.some((c) => id === c || (c.length < 36 && id.startsWith(c)));
}
