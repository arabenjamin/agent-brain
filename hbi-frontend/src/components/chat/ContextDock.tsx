/**
 * Under each answer: what the turn actually did, from its own events
 * (`lib/chatDock.ts`). What ran (in order, failures and truncation shown),
 * what it wrote, what the answer cites, what it read, and every link in the
 * answer marked by whether a tool returned it this turn. Each id opens in the
 * drawer. This is the chat loop's tool-trace receipt, made visible.
 */
import { useMemo, useState } from "react";
import type { ChatEvent } from "../../api/chat";
import { useLookupMany, type IdMatch } from "../../api/search";
import { buildDock, isCited, type Dock } from "../../lib/chatDock";
import { openEntity } from "../drawer/entities";
import { Pill, type Tone } from "../ui";

const KIND_TONE: Record<string, Tone> = { note: "accent", task: "purple", schedule: "cyan", todo: "yellow", job: "muted", entity: "green" };

function plural(n: number, one: string, many = `${one}s`) {
  return `${n} ${n === 1 ? one : many}`;
}

function byKind(items: IdMatch[]): string {
  const c = new Map<string, number>();
  for (const i of items) c.set(i.kind, (c.get(i.kind) ?? 0) + 1);
  return [...c.entries()].map(([k, n]) => plural(n, k)).join(", ");
}

function Item({ m, cited }: { m: IdMatch; cited?: boolean }) {
  return (
    <li>
      <button type="button" className="dock-item" onClick={() => openEntity(m.kind, m.id)} title={`Open ${m.kind} ${m.id}`}>
        <Pill tone={KIND_TONE[m.kind] ?? "muted"}>{m.kind}</Pill>
        <span className="dock-label">{m.label?.replace(/\s+/g, " ") || m.id}</span>
        {cited && <span className="dock-cited" title="The answer names this id">cited</span>}
      </button>
    </li>
  );
}

function argsLine(args: unknown): string {
  if (args === undefined || args === null) return "";
  const s = JSON.stringify(args);
  return s.length > 90 ? `${s.slice(0, 89)}…` : s;
}

export default function ContextDock({ events, answer }: { events: ChatEvent[]; answer: string }) {
  const dock: Dock = useMemo(() => buildDock(events, answer), [events, answer]);
  const [open, setOpen] = useState(false);
  // What the summary needs is resolved now; what was merely read waits for a click.
  const eager = useLookupMany([...dock.wrote, ...dock.cited]);
  const lazy = useLookupMany(open ? dock.read : []);

  const found = new Map<string, IdMatch>();
  for (const m of [...(eager.data?.found ?? []), ...(lazy.data?.found ?? [])]) found.set(m.id, m);
  const resolve = (ids: string[]) => ids.map((id) => found.get(id)).filter((m): m is IdMatch => !!m);
  const wrote = resolve(dock.wrote);
  // One item per node: an answer may cite the same note by full id and by prefix.
  const citedFound = [...new Map((eager.data?.found ?? []).filter((m) => isCited(m.id, dock.cited)).map((m) => [m.id, m])).values()];
  const citedMissing = (eager.data?.missing ?? []).filter((id) => dock.cited.includes(id));
  const read = resolve(dock.read);

  const failed = dock.runs.filter((r) => !r.ok).length;
  const unsourced = dock.links.filter((l) => !l.from).length;
  const nothing = dock.runs.length === 0 && dock.wrote.length === 0 && dock.cited.length === 0 && dock.links.length === 0;
  if (!dock.hasEvents && nothing) return null;

  return (
    <div className={`dock${open ? " open" : ""}`}>
      <button type="button" className="dock-summary" onClick={() => setOpen(!open)} aria-expanded={open}
        title={open ? "Hide what this turn did" : "Show what this turn did"}>
        {dock.hasEvents ? (
          dock.runs.length === 0
            ? <span className="tone-text-yellow">no tools ran: from the model's own knowledge</span>
            : <span>{plural(dock.runs.length, "tool")} ran{failed > 0 && <span className="tone-text-red"> · {failed} failed</span>}</span>
        ) : <span className="wk-dim">restored turn</span>}
        {dock.wrote.length > 0 && <span>wrote {wrote.length ? byKind(wrote) : plural(dock.wrote.length, "id")}</span>}
        {dock.cited.length > 0 && (
          <span>cites {plural(citedFound.length, "item")}{citedMissing.length > 0 && <span className="tone-text-yellow"> · {citedMissing.length} not found</span>}</span>
        )}
        {dock.links.length > 0 && (
          <span>
            {plural(dock.links.length, "link")}
            {dock.hasEvents && unsourced > 0 && <span className="tone-text-yellow"> · {unsourced} not from a tool</span>}
          </span>
        )}
        <span className="dock-chevron">{open ? "▲" : "▼"}</span>
      </button>

      {open && (
        <div className="dock-body">
          {!dock.hasEvents && (
            <div className="wk-dim dock-note">
              Restored from history: tool events aren't kept for past turns, so only what the answer itself names is shown, unchecked.
            </div>
          )}

          {dock.runs.length > 0 && (
            <section>
              <h4>Ran, in order</h4>
              <ol className="dock-runs">
                {dock.runs.map((r, i) => (
                  <li key={i}>
                    <span className={r.ok ? "tone-text-green" : "tone-text-red"}>{r.ok ? "✓" : "✗"}</span>
                    <code>{r.tool}</code>
                    <span className="wk-dim dock-args">{argsLine(r.args)}</span>
                    {r.modelSaw !== null && (
                      <span className="tone-text-yellow" title="The model was given less than the whole result">
                        model read {r.modelSaw.toLocaleString()} of {r.resultChars.toLocaleString()} chars
                      </span>
                    )}
                  </li>
                ))}
              </ol>
              <div className="wk-dim dock-note">Anything the answer says it did that is not listed here did not happen.</div>
            </section>
          )}

          {dock.wrote.length > 0 && (
            <section>
              <h4>Wrote</h4>
              <ul className="dock-list">
                {wrote.map((m) => <Item key={m.id} m={m} />)}
                {eager.data && dock.wrote.filter((id) => !found.has(id)).map((id) => (
                  <li key={id} className="wk-dim"><code>{id.slice(0, 8)}</code> not found (not a note, task, schedule or todo, or since deleted)</li>
                ))}
              </ul>
            </section>
          )}

          {dock.cited.length > 0 && (
            <section>
              <h4>Cited in the answer</h4>
              <ul className="dock-list">
                {citedFound.map((m) => <Item key={m.id} m={m} cited />)}
                {citedMissing.map((id) => (
                  <li key={id} className="tone-text-yellow"><code>{id}</code>: the answer names it, but nothing has this id</li>
                ))}
              </ul>
            </section>
          )}

          {dock.read.length > 0 && (
            <section>
              <h4>Read ({dock.read.length})</h4>
              {lazy.isPending && <div className="loading">Resolving…</div>}
              <ul className="dock-list">
                {read.map((m) => <Item key={m.id} m={m} cited={isCited(m.id, dock.cited)} />)}
              </ul>
              {lazy.data?.truncated && <div className="wk-dim dock-note">Showing the first {lazy.data.max}.</div>}
            </section>
          )}

          {dock.links.length > 0 && (
            <section>
              <h4>Links in the answer</h4>
              <ul className="dock-list">
                {dock.links.map((l) => (
                  <li key={l.url} className="dock-link">
                    {!dock.hasEvents ? <Pill tone="muted">unchecked</Pill>
                      : l.from ? <Pill tone="green">from {l.from}</Pill>
                      : <Pill tone="yellow">not in any tool result</Pill>}
                    <a href={l.url} target="_blank" rel="noreferrer">{l.url}</a>
                  </li>
                ))}
              </ul>
              {dock.hasEvents && unsourced > 0 && (
                <div className="wk-dim dock-note">A link no tool returned this turn came from the model's memory or an earlier turn: check it before relying on it.</div>
              )}
            </section>
          )}
        </div>
      )}
    </div>
  );
}
