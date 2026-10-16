/**
 * A todo: what the user must do and who asked, its thread, when it is due,
 * and the tasks made from it. Editing it is where it is handed to an agent,
 * to the brain, or made a schedule. `id = "new"` creates one.
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
import { HandToAgent, ReviewActions } from "../items/ItemActions";
import MakeSchedule from "../items/MakeSchedule";
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

  if (isNew || editing) {
    return (
      <Drawer title={isNew ? "New todo" : "Edit todo"} onClose={isNew ? closeEntity : () => setEditing(false)} wide>
        <TodoForm
          embedded
          existing={!isNew && t ? (t as Todo) : null}
          onCancel={() => (isNew ? closeEntity() : setEditing(false))}
          onSaved={async (savedId) => {
            await invalidateWork();
            setEditing(false);
            if (isNew && savedId) openEntity("todo", savedId);
            else if (isNew) closeEntity();
          }}
        />
        {!isNew && t && (
          <Section title="Hand it over" aside={t.dispatch_count ? `${t.dispatch_count} of 3 runs` : undefined}>
            {["in_progress", "done", "awaiting_approval"].includes(t.status)
              ? <div className="dw-note">It is {STATUS_LABELS[t.status]?.toLowerCase() ?? t.status}; there is nothing to hand over now.</div>
              : (
                <>
                  <div className="dw-note">To an agent: it is told only the prompt below, and its result comes back to you for review.</div>
                  <HandToAgent item={t} onDone={() => q.refetch()} />
                  <div className="dw-note">Or to the brain: it plans the steps itself, from the title as the goal and the description as context, on the scheduler's next tick.</div>
                  <div className="settings-actions">
                    <button type="button" className="btn-ghost" disabled={!!busy}
                      onClick={() => window.confirm(`Create a task for “${t.title}”? The brain will plan and run it on its next tick.`)
                        && act("Hand to the brain", async () => {
                          const r = await todoToTask(t.id);
                          if (r.warning) throw new Error(r.warning);
                          openEntity("task", r.task_id);
                        })}>
                      Hand to the brain
                    </button>
                  </div>
                </>
              )}
            <MakeSchedule item={t} />
            {result && <div className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</div>}
          </Section>
        )}
      </Drawer>
    );
  }

  const actions = t && (
    <>
      <button type="button" className="btn" disabled={!!busy}
        onClick={() => act(t.status === "done" ? "Reopen" : "Mark done", () => setTodoStatus(t.id, t.status === "done" ? "pending" : "done"))}>
        {t.status === "done" ? "Reopen" : "Mark done"}
      </button>
      <button type="button" className="btn-ghost" disabled={!!busy} onClick={() => setEditing(true)}
        title="Edit it, or hand it to an agent or the brain">Edit / hand over</button>
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

          <AskBox item={t} />

          <div className="dw-thread">
            <button type="button" className="btn" disabled={!!busy}
              title="Opens the Ask chat on this item's own conversation; the drawer closes so the chat is usable"
              onClick={() => act("Open the chat", async () => openSession(await ensureThread(t)))}>
              💬 {t.session_id ? "Continue the chat about this" : "Chat with the brain about this"}
            </button>
            <span className="dw-dim">{t.session_id ? "Its conversation so far opens in Ask." : "Starts a conversation that stays linked to this item."}</span>
          </div>

          <Section title="Where it came from">
            <div className="dw-grid">
              <span>From</span>
              <span><Source item={t} /></span>
              {t.source_task && (
                <><span>Run</span><span><EntityLink kind="task" id={t.source_task}>task {t.source_task.slice(0, 8)}</EntityLink></span></>
              )}
              {t.origin && t.origin.kind !== "user" && (
                <><span>Kind</span><span>{ORIGIN_LABELS[t.origin.kind] ?? t.origin.kind}{t.origin.ref && <span className="dw-dim"> · {t.origin.ref}</span>}</span></>
              )}
              <span>For</span><span>{assigneeLabel(t.assignee)}</span>
              {t.snoozed_until && Date.parse(t.snoozed_until) > now && (
                <><span>Snoozed</span><span>until {formatLocal(t.snoozed_until)}</span></>
              )}
            </div>
          </Section>

          {(t.assignee?.startsWith("agent:") || t.status === "awaiting_approval" || t.status === "awaiting_review" || t.result_note_id || t.review_comment) && (
            <Section title="Agent" aside={t.dispatch_count ? `${t.dispatch_count} of 3 runs` : undefined}>
              {t.status === "in_progress" && t.assignee?.startsWith("agent:") && (
                <div className="dw-note">{t.assignee.slice(6)} is working on it; the result will land in the thread for your review.</div>
              )}
              {(t.status === "awaiting_approval" || t.status === "awaiting_review") && <ReviewActions item={t} />}
              {t.result_note_id && t.status !== "awaiting_review" && (
                <div className="dw-dim">Last result: <EntityLink kind="note" id={t.result_note_id}>{t.result_note_id.slice(0, 8)}</EntityLink></div>
              )}
              {t.review_comment && <div className="dw-dim">Your last comment: {t.review_comment}</div>}
            </Section>
          )}

          <Section title="Description">
            {t.description
              ? <div className="entity-content markdown-body"><ReactMarkdown remarkPlugins={[remarkGfm]}>{t.description}</ReactMarkdown></div>
              : <div className="dw-note">No description.</div>}
          </Section>

          {d.tasks.length > 0 && (
            <Section title="Handed to the brain" aside={`${d.tasks.length}`}>
              <ul className="dw-list">
                {d.tasks.map((task) => (
                  <li key={task.id}>
                    <StatusPill status={task.status} />
                    <EntityLink kind="task" id={task.id}>{task.goal}</EntityLink>
                  </li>
                ))}
              </ul>
            </Section>
          )}

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

/** What the user must do. An item the user wrote needs no box; one the brain
 * made says what it wants, or that it never said. */
function AskBox({ item }: { item: Todo }) {
  if (item.ask) {
    return (
      <div className="dw-ask">
        <span className="dw-ask-head">What you need to do</span>
        <span className="dw-ask-body">{item.ask}</span>
      </div>
    );
  }
  if (!item.source && (!item.origin || item.origin.kind === "user")) return null;
  return (
    <div className="dw-ask missing">
      <span className="dw-ask-head">No ask recorded</span>
      <span className="dw-ask-body dw-dim">
        Whatever made this did not say what it needs from you (items made before 2026-10-16 never did).
        The description below is all it left; ask in the chat, or mark it done if there is nothing to do.
      </span>
    </div>
  );
}

/** Who made the item, in words. */
function Source({ item }: { item: Todo }) {
  if (item.source) return <>{item.source}</>;
  const o = item.origin;
  if (!o || o.kind === "user") return <>you</>;
  if (o.kind === "proposal" && o.ref) {
    return <EntityLink kind="task" id={o.ref}>{ORIGIN_LABELS.proposal} from task {o.ref.slice(0, 8)}</EntityLink>;
  }
  return <>{ORIGIN_LABELS[o.kind] ?? o.kind}<span className="dw-dim"> · not recorded which run</span></>;
}
