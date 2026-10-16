/**
 * Feed: one list of what wants the user (HBI plan §9.3), from `/api/feed`.
 * Items to act on (done, snooze, reply), notifications to read (opening one
 * opens its conversation and marks it read), and one card per system signal
 * that drills to its rows. `?mode=merges` is the entity-merge list the
 * merges card opens; `?mode=relations` the relationships to review;
 * `?mode=types` the entity types to check.
 */
import { useState, type ReactNode } from "react";
import { useMergeCandidates } from "../../api/entities";
import { completeItem, ensureThread, markRead, openSession, snoozeItem, useFeed, type FeedItem } from "../../api/feed";
import type { AgentNotification } from "../../api/notifications";
import { relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { openEntity } from "../drawer/entities";
import { assigneeLabel, ORIGIN_LABELS, PRIORITY_LABELS, STATUS_LABELS } from "../todos/model";
import { EmptyState, Pill } from "../ui";
import MergeCandidates from "./MergeCandidates";
import RelationReview from "./RelationReview";
import TypeReview from "./TypeReview";
import { HandToAgent, ReviewActions } from "../items/ItemActions";

const MERGE_SHOWN = 30;

function Section({ title, count, aside, children }: { title: string; count: number; aside?: ReactNode; children: ReactNode }) {
  if (count === 0) return null;
  return (
    <section className="wk-group">
      <header className="wk-group-head">
        <span>{title}</span>
        <span className="wk-count">{count}</span>
        {aside}
      </header>
      {children}
    </section>
  );
}

/** Tomorrow at 09:00 local, and the same next week. */
function snoozeTimes(now = new Date()): { label: string; at: Date }[] {
  const at = (days: number) => {
    const d = new Date(now);
    d.setDate(d.getDate() + days);
    d.setHours(9, 0, 0, 0);
    return d;
  };
  return [{ label: "until tomorrow", at: at(1) }, { label: "for a week", at: at(7) }];
}

function ItemRow({ t, openId }: { t: FeedItem; openId?: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const origin = t.origin?.kind;
  const [handing, setHanding] = useState(false);
  const awaiting = t.status === "awaiting_approval" || t.status === "awaiting_review";
  const agent = t.assignee?.startsWith("agent:") ? t.assignee.slice(6) : null;
  return (
    <>
    <div className={`wk-row wk-feed-item${openId === t.id ? " active" : ""}`}>
      <span className="wk-row-lead">
        {t.overdue ? <Pill tone="yellow">overdue</Pill>
          : t.status.startsWith("awaiting") ? <Pill tone="accent">{STATUS_LABELS[t.status]}</Pill>
          : <Pill tone={t.priority <= 1 ? "red" : "muted"}>{PRIORITY_LABELS[t.priority]}</Pill>}
      </span>
      <button type="button" className="wk-row-main wk-link" onClick={() => openEntity("todo", t.id)}>
        <span className="wk-row-title">{t.title}</span>
        {t.source ? <span className="wk-dim"> · from {t.source}</span>
          : origin && origin !== "user" && <span className="wk-dim"> · {ORIGIN_LABELS[origin] ?? origin}</span>}
        {t.assignee && t.assignee !== "user" && <span className="wk-dim"> · for {assigneeLabel(t.assignee)}</span>}
        {t.due_at && !t.overdue && <span className="wk-dim"> · due {t.due_at.slice(0, 10)}</span>}
        {error && <span className="tone-text-red"> · {error}</span>}
      </button>
      <span className="wk-row-meta wk-feed-actions">
        <button type="button" className="btn-ghost" disabled={busy} title="Chat with the brain about this item, in its own conversation"
          onClick={() => act(async () => openSession(await ensureThread(t)))}>💬 Chat</button>
        {awaiting ? (
          <ReviewActions item={t} compact />
        ) : (
          <>
            {snoozeTimes().map((s) => (
              <button key={s.label} type="button" className="btn-ghost" disabled={busy} title={`Hide it until ${s.at.toLocaleString()}`}
                onClick={() => act(() => snoozeItem(t.id, s.at))}>Snooze {s.label}</button>
            ))}
            <button type="button" className="btn-ghost" disabled={busy} onClick={() => setHanding((h) => !h)}
              title="Let an agent do it; you review the result">Hand to an agent</button>
            <button type="button" className="btn" disabled={busy} onClick={() => act(() => completeItem(t.id))}>Done</button>
          </>
        )}
      </span>
    </div>
    {t.ask && !awaiting && <div className="wk-hint">To do: {t.ask}</div>}
    {t.status === "awaiting_approval" && agent && (
      <div className="wk-hint">{agent} is assigned this and waits for your approval; its reason is in the thread (💬 Chat).</div>
    )}
    {t.status === "awaiting_review" && agent && (
      <div className="wk-hint">{agent} finished run {t.dispatch_count ?? 1}; the result is in the thread and in its note.</div>
    )}
    {handing && <HandToAgent item={t} onDone={() => setHanding(false)} />}
    </>
  );
}

function ReadRow({ n }: { n: AgentNotification }) {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const read = async () => {
    try {
      await markRead(n.id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <div className="wk-row wk-feed-read">
      <span className="wk-row-lead"><Pill tone="cyan">read</Pill></span>
      <button type="button" className="wk-row-main wk-link"
        onClick={() => {
          if (n.related_session_id) {
            read();
            openSession(n.related_session_id);
          } else setOpen((o) => !o);
        }}
        title={n.related_session_id ? "Open its conversation (marks it read)" : "Show the message"}>
        <span className="wk-row-title">{n.context || n.message.split("\n")[0].slice(0, 100)}</span>
        {open && <div className="wk-feed-message">{n.message}</div>}
        {error && <span className="tone-text-red"> · {error}</span>}
      </button>
      <span className="wk-row-meta">
        <span className="wk-dim">{relTime(n.created_at)}</span>
        {!n.related_session_id && <button type="button" className="btn-ghost" onClick={read}>Mark read</button>}
      </span>
    </div>
  );
}

function Merges() {
  const merges = useMergeCandidates(MERGE_SHOWN, undefined, 300_000);
  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <a className="btn-ghost" href={hrefTo("work", { view: "feed" })}>← Feed</a>
        <span className="wk-dim">
          {merges.data ? `${merges.data.total} to decide, ${Math.min(MERGE_SHOWN, merges.data.total)} shown` : "Loading…"}
        </span>
      </div>
      {merges.error && <EmptyState tone="red" title="Couldn't load merge proposals" checked={merges.error.message} />}
      <div className="wk-hint">
        Names the brain stored separately that may mean one thing. A merge moves the mentions and keeps the old name as an
        alias; undo it from the entity's drawer. A local classifier reads the notes behind each pair: the ones it is fairly
        sure are the same come first, then those it leans on or hasn't judged yet, then related, then the ones it couldn't show are the same,
        each by shared notes. Its verdict only orders the list.
      </div>
      <MergeCandidates items={merges.data?.candidates ?? []} />
    </div>
  );
}

export default function FeedLane({ openId, mode }: { openId?: string; mode?: string }) {
  const q = useFeed();
  if (mode === "merges") return <Merges />;
  if (mode === "relations") return <RelationReview />;
  if (mode === "types") return <TypeReview />;
  const f = q.data;
  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <span className="wk-row-title">{f ? `${f.total} ${f.total === 1 ? "thing wants" : "things want"} you` : "Loading the feed…"}</span>
        {f && f.snoozed > 0 && <span className="wk-dim">{f.snoozed} snoozed</span>}
        <button type="button" className="btn" style={{ marginLeft: "auto" }} onClick={() => openEntity("todo", "new")}>+ New item</button>
      </div>
      {q.error && <EmptyState tone="red" title="Couldn't load the feed" checked={q.error.message} />}
      {f?.errors.map((e) => <EmptyState key={e.part} tone="red" title={`Part of the feed could not be read: ${e.part}`} checked={e.error} />)}

      {f && (
        <>
          <Section title="For you" count={f.items.length}>
            {f.items.map((t) => <ItemRow key={t.id} t={t} openId={openId} />)}
          </Section>

          <Section title="To read" count={f.reads.length}>
            {f.reads.map((n) => <ReadRow key={n.id} n={n} />)}
          </Section>

          <Section title="Signals" count={f.digests.length}>
            {f.digests.map((d) => (
              <a key={d.kind} className="wk-row wk-feed-digest" href={hrefTo("work", d.drill)}>
                <span className="wk-row-lead"><Pill tone={["entity_merges", "relations", "entity_types"].includes(d.kind) ? "purple" : "red"}>{d.count}</Pill></span>
                <span className="wk-row-main">{d.title}</span>
                <span className="wk-row-meta wk-dim">open →</span>
              </a>
            ))}
          </Section>

          {f.total === 0 && f.errors.length === 0 && (
            <EmptyState
              title="Nothing wants you"
              checked={`0 open items of yours${f.snoozed ? ` (${f.snoozed} snoozed)` : ""} · 0 unread notifications · no entity merges to decide, no failed tasks or dead jobs this week, no schedule with steps that can't run`}
            />
          )}
        </>
      )}
    </div>
  );
}
