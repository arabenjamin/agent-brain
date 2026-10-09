/**
 * A todo: what it asks, when it is due, the tasks made from it, and the
 * controls — done/reopen, edit, hand to the brain, delete. `id = "new"`
 * creates one.
 */
import { useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { deleteTodo, invalidateWork, setTodoStatus, todoToTask, useTodoDetail } from "../../api/work";
import { formatLocal } from "../../lib/time";
import { useNow } from "../../lib/useNow";
import TodoForm from "../todos/TodoForm";
import { ORIGIN_LABELS, PRIORITY_LABELS, STATUS_LABELS, assigneeLabel, isTodoOverdue, type Todo } from "../todos/model";
import { ensureThread, openSession } from "../../api/feed";
import { Drawer, Pill } from "../ui";
import { closeEntity, openEntity } from "./entities";
import { EntityLink, LoadState, Section, StatusPill } from "./parts";

const PRIORITY_TONE = { 0: "red", 1: "yellow", 2: "muted", 3: "muted" } as const;

export default function TodoView({ id }: { id: string }) {
  const isNew = id === "new";
  const q = useTodoDetail(id);
  const [editing, setEditing] = useState(isNew);
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const now = useNow();

  if (isNew || editing) {
    return (
      <Drawer title={isNew ? "New todo" : "Edit todo"} onClose={isNew ? closeEntity : () => setEditing(false)} wide>
        <TodoForm
          embedded
          existing={!isNew && q.data ? (q.data.todo as Todo) : null}
          onCancel={() => (isNew ? closeEntity() : setEditing(false))}
          onSaved={async (savedId) => {
            await invalidateWork();
            setEditing(false);
            if (isNew && savedId) openEntity("todo", savedId);
            else if (isNew) closeEntity();
          }}
        />
      </Drawer>
    );
  }

  const d = q.data;
  const t = d?.todo;

  const act = async (label: string, fn: () => Promise<unknown>) => {
    setBusy(label);
    setResult(null);
    try {
      await fn();
      setResult({ ok: true, text: `${label}: done` });
    } catch (e) {
      setResult({ ok: false, text: `${label} failed: ${e instanceof Error ? e.message : String(e)}` });
    } finally {
      setBusy(null);
      q.refetch();
    }
  };

  const actions = t && (
    <>
      <button type="button" className="btn" disabled={!!busy}
        onClick={() => act(t.status === "done" ? "Reopen" : "Mark done", () => setTodoStatus(t.id, t.status === "done" ? "pending" : "done"))}>
        {t.status === "done" ? "Reopen" : "Mark done"}
      </button>
      <button type="button" className="btn-ghost" disabled={!!busy} onClick={() => setEditing(true)}>Edit</button>
    </>
  );

  const overdue = t ? isTodoOverdue(t, now) : false;

  return (
    <Drawer title={<><span>Todo</span>{t && <StatusPill status={t.status} />}</>} actions={actions} onClose={closeEntity} wide>
      {!d || !t ? (
        <LoadState what="todo" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{t.title}</h3>
          <div className="dw-chips">
            <Pill tone={PRIORITY_TONE[t.priority] ?? "muted"}>{PRIORITY_LABELS[t.priority] ?? t.priority}</Pill>
            {t.due_at && (
              <Pill tone={overdue ? "yellow" : "muted"}>due {t.due_at.slice(0, 10)}{overdue && " · overdue"}</Pill>
            )}
            {t.tags.map((tag) => <Pill key={tag}>{tag}</Pill>)}
          </div>
          {result && <div className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</div>}

          <Section title="Item">
            <div className="dw-grid">
              <span>For</span><span>{assigneeLabel(t.assignee)}</span>
              <span>Status</span><span>{STATUS_LABELS[t.status] ?? t.status}</span>
              <span>From</span>
              <span>
                {!t.origin || t.origin.kind === "user" ? "you"
                  : t.origin.kind === "proposal" && t.origin.ref
                    ? <EntityLink kind="proposal" id={t.origin.ref}>{ORIGIN_LABELS.proposal}: {t.origin.ref}</EntityLink>
                    : <>{ORIGIN_LABELS[t.origin.kind] ?? t.origin.kind}{t.origin.ref && <span className="dw-dim"> · {t.origin.ref.slice(0, 8)}</span>}</>}
              </span>
              <span>Thread</span>
              <span>
                <button type="button" className="wk-link" disabled={!!busy}
                  onClick={() => act("Open the thread", async () => openSession(await ensureThread(t)))}>
                  {t.session_id ? "open the conversation" : "start a conversation about it"}
                </button>
              </span>
              {t.snoozed_until && Date.parse(t.snoozed_until) > now && (
                <><span>Snoozed</span><span>until {formatLocal(t.snoozed_until)}</span></>
              )}
            </div>
          </Section>

          <Section title="Description">
            {t.description
              ? <div className="entity-content markdown-body"><ReactMarkdown remarkPlugins={[remarkGfm]}>{t.description}</ReactMarkdown></div>
              : <div className="dw-note">No description.</div>}
          </Section>

          <Section title="Handed to the brain" aside={d.tasks.length ? `${d.tasks.length}` : undefined}>
            {d.tasks.length > 0 && (
              <ul className="dw-list">
                {d.tasks.map((task) => (
                  <li key={task.id}>
                    <StatusPill status={task.status} />
                    <EntityLink kind="task" id={task.id}>{task.goal}</EntityLink>
                  </li>
                ))}
              </ul>
            )}
            {d.tasks.length === 0 && <div className="dw-note">Not yet. Handing it over creates a task with this title as its goal and the description as context; the scheduler plans and runs it on its next tick.</div>}
            <div className="settings-actions">
              <button type="button" className="btn" disabled={!!busy || t.status === "done"}
                title={t.status === "done" ? "Reopen it first" : undefined}
                onClick={() => window.confirm(`Create a task for “${t.title}”? The brain will plan and run it on its next tick.`)
                  && act("Hand to the brain", async () => {
                    const r = await todoToTask(t.id);
                    if (r.warning) throw new Error(r.warning);
                    openEntity("task", r.task_id);
                  })}>
                Hand to the brain
              </button>
            </div>
          </Section>

          <Section title="Danger">
            <button type="button" className="btn danger" disabled={!!busy}
              onClick={() => window.confirm(`Delete “${t.title}”?`) && act("Delete", async () => { await deleteTodo(t.id); closeEntity(); })}>
              Delete todo
            </button>
          </Section>
          <div className="dw-dim">created {formatLocal(t.created_at)} · updated {formatLocal(t.updated_at)} · <code>{t.id}</code></div>
        </>
      )}
    </Drawer>
  );
}
