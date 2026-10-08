/**
 * System → Logs: the brain's own log stream, about the machine rather than its
 * activity. Filter by level, module (tracing target) and text; "hide routine
 * activity" (on by default) quiets the INFO chatter of the scheduler, queue,
 * tools and chat while keeping their warnings and errors. Live polls every
 * 5 s; paused freezes the page. An id in a line opens it in the drawer.
 *
 * Tasks and jobs live in Work, not here.
 */
import { useMemo, useState, type ReactNode } from "react";
import { lookupId } from "../../api/search";
import { ACTIVITY_TARGETS, useLogs, type LogEntry } from "../../api/system";
import { formatLocal } from "../../lib/time";
import { openEntity } from "../drawer/entities";
import { EmptyState, Pill, type Tone } from "../ui";

const LEVELS = ["error", "warn", "info", "debug"];
const LEVEL_TONE: Record<string, Tone> = { ERROR: "red", WARN: "yellow", INFO: "accent", DEBUG: "muted" };
const PAGE = 300;
const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;

/** Open whatever an id names (note, task, job, …), or say nothing has it. */
async function openId(id: string, setMsg: (m: string | null) => void) {
  try {
    const m = await lookupId(id);
    openEntity(m.kind, m.id);
    setMsg(null);
  } catch {
    setMsg(`Nothing the HBI can open has the id ${id} (it may be a session or a pruned job).`);
  }
}

function Message({ text, onId }: { text: string; onId: (id: string) => void }) {
  const parts: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(UUID)) {
    const i = m.index ?? 0;
    if (i > last) parts.push(text.slice(last, i));
    parts.push(
      <button key={i} type="button" className="lg-id" title="Open what this id names" onClick={() => onId(m[0])}>{m[0].slice(0, 8)}…</button>,
    );
    last = i + m[0].length;
  }
  parts.push(text.slice(last));
  return <span className="lg-msg">{parts}</span>;
}

function shortTarget(t: string) {
  return t.replace(/^agent_brain(_repository)?::/, "");
}

export default function LogsView() {
  const [level, setLevel] = useState("info");
  const [target, setTarget] = useState("");
  const [text, setText] = useState("");
  const [q, setQ] = useState("");
  const [hideActivity, setHideActivity] = useState(true);
  const [live, setLive] = useState(true);
  const [msg, setMsg] = useState<string | null>(null);

  const logs = useLogs(
    { level, target: target || undefined, quiet: hideActivity ? ACTIVITY_TARGETS.join(",") : undefined, q: q || undefined, limit: PAGE },
    live,
  );
  const d = logs.data;
  // Modules seen in this page, for the picker.
  const targets = useMemo(() => {
    const c = new Map<string, number>();
    for (const e of d?.entries ?? []) c.set(e.target, (c.get(e.target) ?? 0) + 1);
    return [...c.entries()].sort((a, b) => b[1] - a[1]);
  }, [d]);

  return (
    <div className="lg">
      <form className="lg-bar" onSubmit={(e) => { e.preventDefault(); setQ(text.trim()); }}>
        <select className="ui-input auto" value={level} onChange={(e) => setLevel(e.target.value)} title="Lowest level shown">
          {LEVELS.map((l) => <option key={l} value={l}>{l}+</option>)}
        </select>
        <input className="ui-input lg-target" list="lg-targets" value={target} onChange={(e) => setTarget(e.target.value)}
          placeholder="module, e.g. services::gpu_lease (‑prefix to drop)" title="Target prefixes, comma-separated; -prefix drops one" />
        <datalist id="lg-targets">
          {targets.map(([t]) => <option key={t} value={t} />)}
        </datalist>
        <input className="ui-input" value={text} onChange={(e) => setText(e.target.value)} placeholder="text in the line…" />
        <button type="submit" className="btn-ghost">Filter</button>
        <label className="wk-check" title={`Show only warnings and errors from: ${ACTIVITY_TARGETS.map(shortTarget).join(", ")}`}>
          <input type="checkbox" checked={hideActivity} onChange={(e) => setHideActivity(e.target.checked)} /> hide routine activity
        </label>
        <button type="button" className={live ? "btn" : "btn-ghost"} onClick={() => setLive(!live)} title={live ? "Polling every 5 s; click to pause" : "Paused; click to follow"}>
          {live ? "● live" : "❚❚ paused"}
        </button>
      </form>

      <div className="lg-summary">
        {d ? (
          <>
            <span><strong>{d.count}</strong>{d.matched > d.count ? ` of ${d.matched}` : ""} lines, newest first</span>
            <span className="wk-dim">
              the buffer holds the last {d.buffered.toLocaleString()} of up to {d.capacity.toLocaleString()} lines
              {d.oldest ? `, back to ${formatLocal(d.oldest)}` : ""}; older lines are in <code>docker compose logs agent-brain</code>
            </span>
          </>
        ) : logs.error ? (
          <span className="tone-text-red">Couldn't read the log buffer: {logs.error.message}</span>
        ) : <span className="loading">Reading logs…</span>}
      </div>
      {msg && <div className="tone-text-yellow lg-note">{msg}</div>}

      <div className="lg-lines scroll">
        {d && d.entries.length === 0 && (
          <EmptyState title="No lines match"
            checked={`Checked ${d.buffered.toLocaleString()} buffered lines at ${level}+${target ? `, module ${target}` : ""}${q ? `, text “${q}”` : ""}${hideActivity ? ", routine activity hidden" : ""}.`} />
        )}
        {d?.entries.map((e: LogEntry, i) => (
          <div key={`${e.timestamp}-${i}`} className={`lg-line lv-${e.level.toLowerCase()}`}>
            <span className="lg-time" title={formatLocal(e.timestamp)}>{formatLocal(e.timestamp).split(", ").pop()}</span>
            <Pill tone={LEVEL_TONE[e.level] ?? "muted"}>{e.level.toLowerCase()}</Pill>
            <button type="button" className="lg-target-btn" title={`Show only ${e.target}`} onClick={() => setTarget(e.target)}>{shortTarget(e.target)}</button>
            <Message text={e.message} onId={(id) => openId(id, setMsg)} />
          </div>
        ))}
      </div>
    </div>
  );
}
