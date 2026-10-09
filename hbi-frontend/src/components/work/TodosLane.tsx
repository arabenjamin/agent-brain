/**
 * Todos: every item (HBI plan §9.2), filtered by who it is for (mine, agents,
 * all) and by status. Rows open the drawer; the box ticks one off.
 */
import { useMemo, useState } from "react";
import { useTodos } from "../../api/queries";
import { deleteTodo, setTodoStatus } from "../../api/work";
import { useNow } from "../../lib/useNow";
import { openEntity } from "../drawer/entities";
import { ORIGIN_LABELS, PRIORITY_LABELS, SORT_LABELS, STATUS_LABELS, assigneeLabel, compareTodos, isTodoOverdue, type SortBy, type Todo } from "../todos/model";
import { EmptyState, Pill, Tabs } from "../ui";

type Filter = "open" | "pending" | "in_progress" | "awaiting" | "done" | "all";
type Who = "mine" | "agents" | "all";

/** Unassigned items are the user's, as every todo before items was. */
const isMine = (t: Todo) => !t.assignee || t.assignee === "user";

const PRIORITY_TONE = { 0: "red", 1: "yellow", 2: "muted", 3: "muted" } as const;

export default function TodosLane({ openId }: { openId?: string }) {
  const q = useTodos(30_000);
  const now = useNow();
  const [filter, setFilter] = useState<Filter>("open");
  const [who, setWho] = useState<Who>("mine");
  const [sortBy, setSortBy] = useState<SortBy>("priority");
  const [search, setSearch] = useState("");
  const [error, setError] = useState<string | null>(null);

  const all = useMemo(() => (q.data ?? []) as Todo[], [q.data]);
  const rows = useMemo(() => {
    const s = search.trim().toLowerCase();
    return all
      .filter((t) => who === "all" || (who === "mine" ? isMine(t) : !isMine(t)))
      .filter((t) => filter === "all"
        || (filter === "open" ? t.status !== "done"
          : filter === "awaiting" ? t.status.startsWith("awaiting")
          : t.status === filter))
      .filter((t) => !s || `${t.title} ${t.description ?? ""} ${t.tags.join(" ")}`.toLowerCase().includes(s))
      .sort((a, b) => compareTodos(a, b, sortBy));
  }, [all, filter, who, search, sortBy]);
  const agentCount = all.filter((t) => !isMine(t)).length;
  const done = all.filter((t) => t.status === "done");

  const toggle = async (t: Todo) => {
    setError(null);
    try { await setTodoStatus(t.id, t.status === "done" ? "pending" : "done"); }
    catch (e) { setError(e instanceof Error ? e.message : String(e)); }
  };

  const clearDone = async () => {
    if (!window.confirm(`Delete ${done.length} done todo${done.length === 1 ? "" : "s"}? This cannot be undone.`)) return;
    const results = await Promise.allSettled(done.map((t) => deleteTodo(t.id)));
    const failed = results.filter((r) => r.status === "rejected").length;
    setError(failed ? `Failed to delete ${failed} of ${done.length}` : null);
  };

  return (
    <div className="wk-lane-frame">
      <div className="wk-lane-bar">
        <Tabs
          tabs={(["open", "pending", "in_progress", "awaiting", "done", "all"] as Filter[]).map((f) => ({
            id: f,
            label: f === "open" ? "Open" : f === "all" ? "All" : f === "awaiting" ? "Awaiting you" : STATUS_LABELS[f],
          }))}
          active={filter}
          onChange={setFilter}
        />
        <button type="button" className="btn" onClick={() => openEntity("todo", "new")}>+ New todo</button>
      </div>
      <div className="wk-lane scroll">
        <div className="wk-toolbar">
          <Tabs
            tabs={[{ id: "mine" as Who, label: "Mine" }, { id: "agents" as Who, label: `Agents' (${agentCount})` }, { id: "all" as Who, label: "All" }]}
            active={who}
            onChange={setWho}
          />
          <input className="ui-input" placeholder="Search title, description, tags…" value={search} onChange={(e) => setSearch(e.target.value)} />
          <select className="ui-input auto" value={sortBy} onChange={(e) => setSortBy(e.target.value as SortBy)}>
            {(Object.keys(SORT_LABELS) as SortBy[]).map((k) => <option key={k} value={k}>{SORT_LABELS[k]}</option>)}
          </select>
          {done.length > 0 && <button type="button" className="btn-ghost" onClick={clearDone}>Clear done ({done.length})</button>}
        </div>
        {error && <div className="tone-text-red">{error}</div>}
        {!q.data ? (
          q.error ? <EmptyState tone="red" title="Couldn't load todos" checked={q.error.message} /> : <div className="loading">Loading todos…</div>
        ) : rows.length === 0 ? (
          <EmptyState title="No items here" checked={`${all.length} items in total (${all.length - agentCount} yours, ${agentCount} assigned to agents); none ${who === "all" ? "" : who === "mine" ? "of yours " : "of agents' "}${filter === "all" ? "" : `with status “${filter.replace("_", " ")}”`}${search ? ` matching “${search}”` : ""}.`} />
        ) : (
          rows.map((t) => {
            const overdue = isTodoOverdue(t, now);
            return (
              <div key={t.id} className={`wk-row wk-todo${openId === t.id ? " active" : ""}${t.status === "done" ? " done" : ""}`}>
                <input type="checkbox" checked={t.status === "done"} onChange={() => toggle(t)} title={t.status === "done" ? "Reopen" : "Mark done"} />
                <button type="button" className="wk-row-main wk-link" onClick={() => openEntity("todo", t.id)}>
                  <span className="wk-row-title">{t.title}</span>
                  {t.tags.length > 0 && <span className="wk-dim"> · {t.tags.join(", ")}</span>}
                  {!isMine(t) && <span className="wk-dim"> · for {assigneeLabel(t.assignee)}</span>}
                  {t.origin && t.origin.kind !== "user" && <span className="wk-dim"> · {ORIGIN_LABELS[t.origin.kind] ?? t.origin.kind}</span>}
                </button>
                <span className="wk-row-meta">
                  <Pill tone={PRIORITY_TONE[t.priority] ?? "muted"}>{PRIORITY_LABELS[t.priority]}</Pill>
                  {t.status === "in_progress" && <Pill tone="yellow">in progress</Pill>}
                  {t.status.startsWith("awaiting") && <Pill tone="accent">{STATUS_LABELS[t.status].toLowerCase()}</Pill>}
                  {t.due_at && <span className={overdue ? "tone-text-yellow" : "wk-dim"}>{overdue ? "overdue " : "due "}{t.due_at.slice(0, 10)}</span>}
                </span>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
