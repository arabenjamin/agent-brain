/**
 * A note: its content, where it came from, how sure the brain is of it, what
 * it mentions and what it is related to — and edit / delete. `id = "new"`
 * writes a note.
 */
import { useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ApiError, apiJson, apiSend } from "../../api/client";
import { keys, queryClient, useNote, useRelatedNotes, type Note } from "../../api/queries";
import { statusTone } from "../../lib/status";
import { formatLocal, relTime } from "../../lib/time";
import { currentRoute, routeHref } from "../../router";
import { NOTE_TYPE_TONE } from "../../lib/noteTypes";
import { Drawer, EmptyState, Pill, Stat, Tabs } from "../ui";
import { closeEntity, openEntity } from "./entities";
import { EntityLink, LoadState, Section } from "./parts";


/** Types a person writes by hand (the rest are written by the brain's pipelines). */
const WRITABLE_TYPES = ["semantic", "episodic", "reflection"];

/** /api/notes/{id}/related returns at most this many, most similar first. */
const RELATED_CAP = 10;

function invalidateNotes(id?: string) {
  return Promise.all([
    id ? queryClient.invalidateQueries({ queryKey: keys.note(id) }) : Promise.resolve(),
    queryClient.invalidateQueries({ queryKey: ["search"] }),
    queryClient.invalidateQueries({ queryKey: ["knowledge"] }),
  ]);
}

function CopyLink() {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="ui-icon-btn"
      title="Copy a link to this"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(routeHref(currentRoute()));
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        } catch {
          // clipboard blocked (insecure origin) — the address bar has the link
        }
      }}
    >
      {copied ? "copied ✓" : "🔗"}
    </button>
  );
}

export default function NoteView({ id }: { id: string }) {
  if (id === "new") return <NewNote />;
  return <ExistingNote id={id} />;
}

function NewNote() {
  const [content, setContent] = useState("");
  const [type, setType] = useState("semantic");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      const r = await apiSend<{ note_id?: string; id?: string }>("POST", "/api/notes", { content: content.trim(), note_type: type });
      await invalidateNotes();
      const newId = r.note_id ?? r.id;
      if (newId) openEntity("note", newId);
      else closeEntity();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };
  return (
    <Drawer title="New note" onClose={closeEntity} wide>
      <div className="settings-actions">
        <label className="dw-dim" htmlFor="new-note-type">Type</label>
        <select id="new-note-type" className="ui-input auto" value={type} onChange={(e) => setType(e.target.value)}>
          {WRITABLE_TYPES.map((t) => <option key={t} value={t}>{t}</option>)}
        </select>
      </div>
      <textarea className="ui-input dw-editor" value={content} onChange={(e) => setContent(e.target.value)}
        placeholder="Write the note (Markdown)…" autoFocus />
      {error && <div className="tone-text-red">{error}</div>}
      <div className="settings-actions">
        <button type="button" className="btn" disabled={saving || !content.trim()} onClick={save}>{saving ? "Saving…" : "Store note"}</button>
        <button type="button" className="btn-ghost" onClick={closeEntity}>Cancel</button>
        <span className="dw-dim">It is embedded, linked to similar notes and scanned for entities, like any stored note.</span>
      </div>
    </Drawer>
  );
}

function ExistingNote({ id }: { id: string }) {
  const [tab, setTab] = useState<"content" | "related">("content");
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const note = useNote(id);
  const related = useRelatedNotes(tab === "related" ? id : undefined);
  const n = note.data;

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await apiSend("PUT", `/api/notes/${encodeURIComponent(id)}`, { content: draft.trim() });
      await invalidateNotes(id);
      setEditing(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!window.confirm("Delete this note? Its relationships go with it. This cannot be undone.")) return;
    setBusy(true);
    try {
      await apiJson(`/api/notes/${encodeURIComponent(id)}`, { method: "DELETE" });
      await invalidateNotes(id);
      closeEntity();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  const title = (
    <>
      <span>Note</span>
      {n?.note_type && <Pill tone={NOTE_TYPE_TONE[n.note_type] ?? "muted"}>{n.note_type}</Pill>}
      {n?.claim_status && <Pill tone={statusTone(n.claim_status)}>{n.claim_status}</Pill>}
      {n?.verification === "failed" && <Pill tone="red">verification failed</Pill>}
    </>
  );

  const actions = n && !editing && (
    <>
      <button type="button" className="ui-icon-btn" title="Edit" onClick={() => { setDraft(n.content); setEditing(true); }}>✎</button>
      <CopyLink />
    </>
  );

  if (!n) {
    const notFound = note.error instanceof ApiError && note.error.status === 404;
    return (
      <Drawer title="Note" onClose={closeEntity}>
        {notFound ? (
          <EmptyState tone="yellow" title="No note with this id"
            checked={<>Looked up <code>{id}</code>. It may have been merged by consolidation or pruned.</>} />
        ) : (
          <LoadState what="note" id={id} error={note.error} onRetry={() => note.refetch()} />
        )}
      </Drawer>
    );
  }

  return (
    <Drawer title={title} actions={actions} onClose={closeEntity} wide={editing}>
      <div className="entity-id">{n.id}</div>
      {n.parent_id && (
        <EmptyState title="This is a chunk of a longer note"
          checked={<>Search matches it and shows the whole note. <EntityLink kind="note" id={n.parent_id}>Open the full note</EntityLink></>} />
      )}
      <div className="ui-stats">
        <Stat label="Created" value={relTime(n.created_at)} title={formatLocal(n.created_at)} />
        <Stat label="Accessed" value={`${n.access_count ?? 0}×`} title={n.last_accessed_at ? `last ${formatLocal(n.last_accessed_at)}` : undefined} />
        <Stat label="Length" value={`${n.content.length.toLocaleString()} ch`} title={n.chunks ? `split into ${n.chunks} chunks for embedding` : undefined} />
      </div>

      {editing ? (
        <>
          <textarea className="ui-input dw-editor" value={draft} onChange={(e) => setDraft(e.target.value)} />
          {error && <div className="tone-text-red">{error}</div>}
          <div className="settings-actions">
            <button type="button" className="btn" disabled={busy || !draft.trim() || draft === n.content} onClick={save}>{busy ? "Saving…" : "Save"}</button>
            <button type="button" className="btn-ghost" onClick={() => setEditing(false)}>Cancel</button>
            <span className="dw-dim">Saving re-embeds it and regenerates its chunks.</span>
          </div>
          <Section title="Danger">
            <button type="button" className="btn danger" disabled={busy} onClick={remove}>Delete note</button>
          </Section>
        </>
      ) : (
        <>
          <Tabs
            tabs={[
              { id: "content", label: "Content" },
              { id: "related", label: related.data ? `Related (${related.data.count >= RELATED_CAP ? `top ${RELATED_CAP}` : related.data.count})` : "Related" },
            ]}
            active={tab}
            onChange={setTab}
          />
          {tab === "content" ? (
            <>
              <div className="entity-content markdown-body">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{n.content}</ReactMarkdown>
              </div>
              {n.note_type === "claim" && <AssertedIn n={n} />}
              <Provenance n={n} />
              {(n.entities?.length ?? 0) > 0 && (
                <Section title="Mentions" aside={`${n.entities!.length}${n.entities!.length >= 30 ? "+" : ""}`}>
                  <div className="dw-chips">
                    {n.entities!.map((e) => (
                      <EntityLink key={e.id} kind="entity" id={e.id} title={e.entity_type ?? undefined}>
                        <Pill tone="muted">{e.name}</Pill>
                      </EntityLink>
                    ))}
                  </div>
                </Section>
              )}
            </>
          ) : (
            <RelatedNotes query={related} />
          )}
        </>
      )}
    </Drawer>
  );
}

/** Where a claim was asserted: the article or video its source note records. */
function AssertedIn({ n }: { n: Note }) {
  const sources = n.asserted_in ?? [];
  return (
    <Section title="Asserted in">
      {sources.length === 0 ? (
        <div className="tone-text-yellow">No source recorded: this claim has no ASSERTED_IN edge to the note it came from.</div>
      ) : (
        <ul className="dw-sources">
          {sources.map((src) => (
            <li key={src.id}>
              <EntityLink kind="note" id={src.id} title="Open the source note">
                {src.title || src.source_context || "source note"}
              </EntityLink>
              {src.url && (
                <a className="dw-ext" href={src.url} target="_blank" rel="noreferrer" title={src.url}>
                  {hostOf(src.url)} ↗
                </a>
              )}
              <div className="dw-dim">
                {[src.outlet, src.source_context, src.created_at && formatLocal(src.created_at)].filter(Boolean).join(" · ")}
              </div>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

function hostOf(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
}

function Provenance({ n }: { n: Note }) {
  const claim = n.claim_status || n.corroboration_tier || n.asserted_by;
  return (
    <Section title="Where it came from">
      <div className="dw-grid">
        <span>Written by</span><span>{n.source_context ?? "—"}</span>
        <span>Provenance</span><span>{n.provenance ?? "—"}</span>
        <span>Embedded</span><span>{n.embedded ? "yes" : n.chunks ? `through ${n.chunks} chunks` : "no — semantic search cannot find it"}</span>
        <span>Topics</span>
        <span>
          {(n.topics?.length ?? 0) > 0
            ? n.topics!.map((t, i) => (
              <EntityLink key={t} kind="topic" id={t} title={i === 0 ? "Main topic" : "Second topic"}>
                <Pill tone={i === 0 ? "accent" : "muted"}>{t}</Pill>
              </EntityLink>
            ))
            : n.topics_assessed === false ? "not classified yet (runs while the brain is idle)" : "none: classified as fitting no topic"}
        </span>
        {claim && (
          <>
            <span>Claim</span>
            <span>{n.claim_status ?? "—"}{n.claim_kind ? ` · ${n.claim_kind}` : ""}</span>
            <span>Asserted by</span><span>{n.asserted_by ?? "—"}</span>
            <span>Corroboration</span>
            <span>
              {n.corroboration_tier ?? "not checked"}
              {(n.corroborating_count ?? 0) > 0 && ` · ${n.corroborating_count} supporting`}
              {(n.contradicting_count ?? 0) > 0 && <span className="tone-text-red"> · {n.contradicting_count} contradicting</span>}
            </span>
            <span>Checked</span><span>{n.verified_at ? formatLocal(n.verified_at) : "never"}</span>
          </>
        )}
        {n.verification && (
          <>
            <span>Verification</span>
            <span className={n.verification === "failed" ? "tone-text-red" : undefined}>{n.verification}{n.verification_reason ? ` — ${n.verification_reason}` : ""}</span>
          </>
        )}
      </div>
    </Section>
  );
}

function RelatedNotes({ query }: { query: ReturnType<typeof useRelatedNotes> }) {
  if (query.isPending) return <div className="loading">Loading related notes…</div>;
  if (query.isError) return <EmptyState tone="red" title="Couldn't load related notes" checked={query.error.message} />;
  const items = query.data.related_notes;
  if (items.length === 0) {
    return (
      <EmptyState title="No related notes"
        checked="This note has no outgoing RELATES_TO (similarity) edges. They are made when a note is embedded; a note stored during a GPU lease gets them in the post-lease backfill." />
    );
  }
  return (
    <div className="related-list">
      {items.length >= RELATED_CAP && <div className="related-cap">The {RELATED_CAP} most similar; the endpoint returns no more than that.</div>}
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
