/**
 * The one inspection surface. Any workspace opens an entity by putting it in
 * the URL (`openEntity("note", id)` → `#/<workspace>?note=<id>`), so the
 * drawer survives reloads and every open entity is a shareable link.
 *
 * Kinds: note (Phase 0); task, job, schedule, todo, proposal (Phase 2).
 * Entities and topics register here when Knowledge's phases land.
 */
import { useCallback, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ApiError } from "../../api/client";
import { useNote, useRelatedNotes } from "../../api/queries";
import { currentRoute, routeHref, useRoute } from "../../router";
import { closeEntity, drawerTarget, openEntity } from "./entities";
import JobView from "./JobView";
import ProposalView from "./ProposalView";
import ScheduleView from "./ScheduleView";
import TaskView from "./TaskView";
import TodoView from "./TodoView";
import { formatLocal, relTime } from "../../lib/time";
import { Drawer, EmptyState, Pill, Stat, Tabs, type Tone } from "../ui";

export default function EntityDrawer() {
  const route = useRoute();
  const target = drawerTarget(route.params);
  if (!target) return null;
  // Keyed by id, so moving from one entity to another resets view state
  // (an open editor, an action's result).
  switch (target.kind) {
    case "note":
      return <NoteDrawer key={target.id} id={target.id} />;
    case "task":
      return <TaskView key={target.id} id={target.id} />;
    case "job":
      return <JobView key={target.id} id={target.id} />;
    case "schedule":
      return <ScheduleView key={target.id} id={target.id} />;
    case "todo":
      return <TodoView key={target.id} id={target.id} />;
    case "proposal":
      return <ProposalView key={target.id} filename={target.id} />;
  }
}

// ── Note ─────────────────────────────────────────────────────────────────────

const NOTE_TYPE_TONE: Record<string, Tone> = {
  semantic: "accent",
  episodic: "cyan",
  reflection: "purple",
  consolidated: "green",
  outcome: "yellow",
  inference: "red",
  claim: "orange",
  source_record: "muted",
  news: "muted",
};

function CopyLink() {
  const [copied, setCopied] = useState(false);
  const copy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(routeHref(currentRoute()));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // clipboard blocked (insecure origin) — the address bar has the link
    }
  }, []);
  return (
    <button type="button" className="ui-icon-btn" onClick={copy} title="Copy link to this note">
      {copied ? "copied ✓" : "🔗"}
    </button>
  );
}

function NoteDrawer({ id }: { id: string }) {
  const [tab, setTab] = useState<"content" | "related">("content");
  const note = useNote(id);
  const related = useRelatedNotes(tab === "related" ? id : undefined);

  const title = (
    <>
      <span>Note</span>
      {note.data?.note_type && (
        <Pill tone={NOTE_TYPE_TONE[note.data.note_type] ?? "muted"}>{note.data.note_type}</Pill>
      )}
    </>
  );

  let body;
  if (note.isPending) {
    body = <div className="loading">Loading note…</div>;
  } else if (note.isError) {
    const notFound = note.error instanceof ApiError && note.error.status === 404;
    body = notFound ? (
      <EmptyState
        tone="yellow"
        title="No note with this id"
        checked={<>Looked up <code>{id}</code> via /api/notes/{"{id}"}. It may have been merged by consolidation or pruned.</>}
      />
    ) : (
      <EmptyState
        tone="red"
        title="Couldn't load the note"
        checked={note.error.message}
        action={<button type="button" className="btn" onClick={() => note.refetch()}>Retry</button>}
      />
    );
  } else {
    const n = note.data;
    body = (
      <>
        <div className="entity-id">{n.id}</div>
        <div className="ui-stats">
          <Stat label="Created" value={relTime(n.created_at)} title={formatLocal(n.created_at)} />
          <Stat label="Accessed" value={`${n.access_count ?? 0}×`} />
          <Stat label="Length" value={`${n.content.length.toLocaleString()} ch`} />
        </div>
        <Tabs
          tabs={[
            { id: "content", label: "Content" },
            { id: "related", label: related.data ? `Related (${relatedLabel(related.data.count)})` : "Related" },
          ]}
          active={tab}
          onChange={setTab}
        />
        {tab === "content" ? (
          <div className="entity-content markdown-body">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{n.content}</ReactMarkdown>
          </div>
        ) : (
          <RelatedNotes query={related} />
        )}
      </>
    );
  }

  return (
    <Drawer title={title} actions={<CopyLink />} onClose={closeEntity}>
      {body}
    </Drawer>
  );
}

/** /api/notes/{id}/related returns at most this many, most similar first. */
const RELATED_CAP = 10;

function relatedLabel(count: number): string {
  return count >= RELATED_CAP ? `top ${RELATED_CAP}` : String(count);
}

function RelatedNotes({ query }: { query: ReturnType<typeof useRelatedNotes> }) {
  if (query.isPending) return <div className="loading">Loading related notes…</div>;
  if (query.isError) {
    return <EmptyState tone="red" title="Couldn't load related notes" checked={query.error.message} />;
  }
  const items = query.data.related_notes;
  if (items.length === 0) {
    return (
      <EmptyState
        title="No related notes"
        checked="This note has no outgoing RELATES_TO (similarity) edges. They are made when a note is embedded; a note stored during a GPU lease gets them in the post-lease backfill."
      />
    );
  }
  return (
    <div className="related-list">
      {items.length >= RELATED_CAP && (
        <div className="related-cap">
          The {RELATED_CAP} most similar; the endpoint returns no more than that.
        </div>
      )}
      {items.map((r) => (
        <button key={r.id} type="button" className="related-item" onClick={() => openEntity("note", r.id)}>
          <span className="related-item-head">
            <Pill tone={NOTE_TYPE_TONE[r.note_type ?? ""] ?? "muted"}>{r.note_type ?? "note"}</Pill>
            <span className="tone-text-muted">sim {r.similarity.toFixed(2)}</span>
          </span>
          <span className="related-item-text">{r.content}</span>
        </button>
      ))}
    </div>
  );
}
