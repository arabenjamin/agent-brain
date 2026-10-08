/**
 * Ctrl-K commands: every workspace view and settings section, plus the
 * query itself as a note id (open it) or a phrase (search notes for it).
 */
import { asEntityId } from "../../lib/ids";
import { goTo } from "../../nav";
import { SETTINGS, WORKSPACES } from "../../workspaces";
import { openEntity } from "../drawer/entities";
import { SETTINGS_SECTIONS } from "../settings/sections";

export interface Command {
  id: string;
  label: string;
  group: string;
  keywords: string;
  run: () => void;
}

export function staticCommands(): Command[] {
  const out: Command[] = [];
  for (const w of WORKSPACES) {
    out.push({ id: `go:${w.id}`, label: `${w.icon} ${w.label}`, group: "Go to", keywords: w.keywords, run: () => goTo(w.id) });
    for (const v of w.views ?? []) {
      out.push({
        id: `go:${w.id}/${v.id}`,
        label: `${w.label} › ${v.label}`,
        group: "Go to",
        keywords: `${w.keywords} ${v.id}`,
        run: () => goTo(w.id, { view: v.id }),
      });
    }
  }
  out.push(
    { id: "new:todo", label: "+ New todo", group: "Create", keywords: "add task reminder", run: () => goTo("work", { view: "todos", todo: "new" }) },
    { id: "new:schedule", label: "+ New schedule", group: "Create", keywords: "add recurring cron scheduled", run: () => goTo("work", { view: "scheduled", schedule: "new" }) },
  );
  for (const s of SETTINGS_SECTIONS) {
    out.push({
      id: `settings:${s.id}`,
      label: `${SETTINGS.icon} Settings › ${s.label}`,
      group: "Settings",
      keywords: `${SETTINGS.keywords} ${s.keywords}`,
      run: () => goTo("settings", { section: s.id }),
    });
  }
  return out;
}

/** Commands that depend on what was typed. */
export function queryCommands(query: string): Command[] {
  const q = query.trim();
  if (!q) return [];
  const id = asEntityId(q);
  if (id) {
    return [{ id: `note:${id}`, label: `Open note ${id}`, group: "Note", keywords: "", run: () => openEntity("note", id) }];
  }
  return [{
    id: `search:${q}`,
    label: `Search notes for “${q}”`,
    group: "Search",
    keywords: "",
    run: () => goTo("knowledge", { view: "notes", q }),
  }];
}

/**
 * The full list for a query: a pasted id opens that note first; otherwise the
 * matching commands, with "search notes for …" last, so Enter on "sched"
 * goes to Scheduled rather than searching notes for "sched".
 */
export function paletteResults(all: Command[], query: string, limit = 40): Command[] {
  const typed = queryCommands(query);
  const matches = matchCommands(all, query);
  const ordered = asEntityId(query) ? [...typed, ...matches] : [...matches, ...typed];
  return ordered.slice(0, limit);
}

/**
 * Commands whose label, group or keywords contain every word typed, best
 * first: a label that starts with the query, then one containing it, then
 * keyword-only matches. An empty query lists everything in order.
 */
export function matchCommands(commands: Command[], query: string): Command[] {
  const q = query.trim().toLowerCase();
  if (!q) return commands;
  const words = q.split(/\s+/);
  const scored: { c: Command; score: number; i: number }[] = [];
  commands.forEach((c, i) => {
    const label = c.label.toLowerCase();
    const hay = `${label} ${c.group.toLowerCase()} ${c.keywords.toLowerCase()}`;
    if (!words.every((w) => hay.includes(w))) return;
    const bare = label.replace(/^\W+\s*/, ""); // ignore a leading icon
    const score = bare.startsWith(q) ? 0 : label.includes(q) ? 1 : 2;
    scored.push({ c, score, i });
  });
  return scored.sort((a, b) => a.score - b.score || a.i - b.i).map((s) => s.c);
}
