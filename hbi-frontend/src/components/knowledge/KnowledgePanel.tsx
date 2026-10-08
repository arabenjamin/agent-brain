import { useCallback, useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { apiJson, query as apiQuery } from "../../api/client";
import { formatLocal } from "../../lib/time";
import { goTo } from "../../nav";
import { DrillBanner } from "../ui";
import { asEntityId } from "../../lib/ids";
import { openEntity } from "../drawer/entities";

interface Note {
  id: string;
  content: string;
  note_type?: string;
  access_count?: number;
  created_at?: string;
  similarity?: number;
}

function fmtDate(s?: string): string {
  if (!s) return "";
  const d = new Date(s);
  if (isNaN(d.getTime())) return s.slice(0, 10);
  return d.toLocaleString(undefined, { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

const NOTE_TYPES = ["semantic", "episodic", "reflection", "consolidated"];

/** A drill-down from Home or Ctrl-K: a server-side filtered list, or a search. */
export interface KnowledgeDrill {
  /** UTC RFC 3339: notes created since. */
  since?: string;
  type?: string;
  /** UTC RFC 3339: claims whose evidence was gathered since. */
  verifiedSince?: string;
  q?: string;
}

const DRILL_LIMIT = 500;

export default function KnowledgePanel({ drill }: { drill?: KnowledgeDrill }) {
  const drillSince = drill?.since;
  const drillType = drill?.type;
  const drillVerified = drill?.verifiedSince;
  const drillQ = drill?.q;
  const filtered = !!(drillSince || drillType || drillVerified);
  const [query, setQuery]               = useState(drillQ ?? "");
  const [filterType, setFilterType]     = useState(drillType ?? "");
  const [notes, setNotes]               = useState<Note[]>([]);
  const [selected, setSelected]         = useState<Note | null>(null);
  const [related, setRelated]           = useState<Note[]>([]);
  const [loading, setLoading]           = useState(false);
  const [loadingRelated, setLoadingRelated] = useState(false);
  const [relatedOpen, setRelatedOpen]       = useState(false);
  const [error, setError]               = useState<string | null>(null);

  // Create state
  const [composing, setComposing]       = useState(false);
  const [newContent, setNewContent]     = useState("");
  const [newNoteType, setNewNoteType]   = useState("semantic");
  const [saving, setSaving]             = useState(false);

  // Edit state
  const [editing, setEditing]           = useState(false);
  const [editContent, setEditContent]   = useState("");

  // Delete confirm
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting]         = useState(false);

  // Every type the list could show: the common ones, whatever is loaded, and
  // a drilled type (the old fixed list hid claim, source_record and 7 more).
  const typeOptions = Array.from(new Set([
    ...NOTE_TYPES,
    ...notes.map(n => n.note_type).filter((t): t is string => !!t),
    ...(drillType ? [drillType] : []),
  ])).sort();

  const loadInitial = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await apiJson<{ notes?: Note[] }>(filtered
        ? `/api/notes${apiQuery({ since: drillSince, note_type: drillType, verified_since: drillVerified, limit: DRILL_LIMIT })}`
        : "/api/notes?limit=50");
      setNotes(data.notes ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [filtered, drillSince, drillType, drillVerified]);

  // A drill with `q` runs that search; otherwise load the (filtered) list.
  useEffect(() => {
    if (drillQ) search(drillQ);
    else loadInitial();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const search = useCallback(async (q: string) => {
    if (!q.trim()) { loadInitial(); return; }
    // A pasted note id is a lookup, not a phrase: substring search on an id
    // matches nothing, so open the note itself.
    const id = asEntityId(q);
    if (id) { openEntity("note", id); return; }
    setLoading(true);
    setError(null);
    setSelected(null);
    setRelated([]);
    setEditing(false);
    setConfirmDelete(false);
    try {
      const data = await apiJson<{ notes?: Note[] }>(`/api/notes?q=${encodeURIComponent(q)}&limit=30`);
      setNotes(data.notes ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [loadInitial]);

  const selectNote = useCallback(async (note: Note) => {
    setSelected(note);
    setEditing(false);
    setEditContent("");
    setConfirmDelete(false);
    setComposing(false);
    setRelated([]);
    setRelatedOpen(false);
    setLoadingRelated(true);
    try {
      const data = await apiJson<{ related_notes?: Note[] }>(`/api/notes/${note.id}/related`);
      setRelated(data.related_notes ?? []);
    } catch {
      // related notes are optional
    } finally {
      setLoadingRelated(false);
    }
  }, []);

  const handleKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") search(query);
  };

  // ── Create ──────────────────────────────────────────────────────────────────

  const openCompose = () => {
    setComposing(true);
    setSelected(null);
    setEditing(false);
    setConfirmDelete(false);
    setNewContent("");
    setNewNoteType("semantic");
  };

  const createNote = useCallback(async () => {
    if (!newContent.trim()) return;
    setSaving(true);
    setError(null);
    try {
      const data = await apiJson<{ note_id?: string }>("/api/notes", {
        method: "POST",
        body: JSON.stringify({ content: newContent.trim(), note_type: newNoteType }),
      });
      const newNote: Note = {
        id:        data.note_id ?? "",
        content:   newContent.trim(),
        note_type: newNoteType,
      };
      setNotes(prev => [newNote, ...prev]);
      setComposing(false);
      setNewContent("");
      selectNote(newNote);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }, [newContent, newNoteType, selectNote]);

  // ── Edit ────────────────────────────────────────────────────────────────────

  const startEdit = () => {
    if (!selected) return;
    setEditContent(selected.content);
    setEditing(true);
    setConfirmDelete(false);
  };

  const cancelEdit = () => {
    setEditing(false);
    setEditContent("");
  };

  const saveEdit = useCallback(async () => {
    if (!selected || !editContent.trim()) return;
    setSaving(true);
    setError(null);
    try {
      await apiJson<unknown>(`/api/notes/${selected.id}`, {
        method: "PUT",
        body: JSON.stringify({ content: editContent.trim() }),
      });
      const updated = { ...selected, content: editContent.trim() };
      setSelected(updated);
      setNotes(prev => prev.map(n => n.id === selected.id ? updated : n));
      setEditing(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }, [selected, editContent]);

  // ── Delete ──────────────────────────────────────────────────────────────────

  const doDelete = useCallback(async () => {
    if (!selected) return;
    setDeleting(true);
    setError(null);
    try {
      await apiJson<unknown>(`/api/notes/${selected.id}`, { method: "DELETE" });
      setNotes(prev => prev.filter(n => n.id !== selected.id));
      setSelected(null);
      setConfirmDelete(false);
      setEditing(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setDeleting(false);
    }
  }, [selected]);

  const noteType = (n: Note) => n.note_type ?? "semantic";

  // ── Render ──────────────────────────────────────────────────────────────────

  const renderRight = () => {
    // Compose new note
    if (composing) {
      return (
        <div className="note-detail">
          <div className="note-edit-header">
            <strong>New Note</strong>
            <select
              className="note-type-select"
              value={newNoteType}
              onChange={e => setNewNoteType(e.target.value)}
            >
              {NOTE_TYPES.map(t => <option key={t} value={t}>{t}</option>)}
            </select>
          </div>
          <textarea
            className="note-edit-area"
            autoFocus
            placeholder="Write your note…"
            value={newContent}
            onChange={e => setNewContent(e.target.value)}
          />
          <div className="note-actions">
            <button className="btn" onClick={createNote} disabled={saving || !newContent.trim()}>
              {saving ? "Saving…" : "Save Note"}
            </button>
            <button className="btn-ghost" onClick={() => setComposing(false)}>Cancel</button>
          </div>
        </div>
      );
    }

    // No note selected
    if (!selected) {
      return (
        <div className="empty-state" style={{ flex: 1 }}>
          <span className="icon">←</span>
          <span>Select a note to read it</span>
        </div>
      );
    }

    // Selected note — edit mode
    if (editing) {
      return (
        <div className="note-detail">
          <div className="note-edit-header">
            <span className={`note-type-badge ${noteType(selected)}`}>{noteType(selected)}</span>
            <span style={{ fontSize: 9, color: "var(--text-muted)", fontFamily: "monospace", userSelect: "all" }}>{selected.id}</span>
          </div>
          <textarea
            className="note-edit-area"
            autoFocus
            value={editContent}
            onChange={e => setEditContent(e.target.value)}
          />
          <div className="note-actions">
            <button className="btn" onClick={saveEdit} disabled={saving || !editContent.trim()}>
              {saving ? "Saving…" : "Save Changes"}
            </button>
            <button className="btn-ghost" onClick={cancelEdit}>Cancel</button>
          </div>
        </div>
      );
    }

    // Selected note — read mode
    return (
      <div className="note-detail">
        <div className="note-detail-topbar">
          <span className={`note-type-badge ${noteType(selected)}`}>{noteType(selected)}</span>
          <div style={{ marginLeft: 8, display: "flex", flexDirection: "column", gap: 1 }}>
            <span style={{ fontSize: 9, color: "var(--text-muted)", fontFamily: "monospace", userSelect: "all" }}>
              {selected.id}
            </span>
            {selected.created_at && (
              <span style={{ fontSize: 9, color: "var(--text-muted)" }}>
                {fmtDate(selected.created_at)}
              </span>
            )}
          </div>
          <div style={{ marginLeft: "auto", display: "flex", gap: 6 }}>
            <button className="btn-ghost" onClick={startEdit} title="Edit note">✎ Edit</button>
            {!confirmDelete
              ? <button className="btn-ghost danger" onClick={() => setConfirmDelete(true)} title="Delete note">🗑 Delete</button>
              : (
                <span style={{ display: "flex", gap: 4, alignItems: "center" }}>
                  <span style={{ fontSize: 11, color: "var(--red)" }}>Delete?</span>
                  <button className="btn danger" onClick={doDelete} disabled={deleting} style={{ padding: "2px 8px", fontSize: 11 }}>
                    {deleting ? "…" : "Yes"}
                  </button>
                  <button className="btn-ghost" onClick={() => setConfirmDelete(false)} style={{ padding: "2px 8px", fontSize: 11 }}>No</button>
                </span>
              )
            }
          </div>
        </div>

        <div className="note-full-content scroll markdown-body">
          <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
            {selected.content}
          </ReactMarkdown>
        </div>

        {selected.access_count != null && (
          <div style={{ fontSize: 10, color: "var(--text-muted)", padding: "4px 0" }}>
            Accessed {selected.access_count}×
          </div>
        )}

        {(loadingRelated || related.length > 0) && (
          <div className="related-section">
            <button
              className="related-section-title"
              onClick={() => setRelatedOpen(v => !v)}
              style={{ background: "none", border: "none", cursor: "pointer", width: "100%", textAlign: "left", color: "inherit" }}
            >
              Related ({loadingRelated ? "…" : related.length}) {relatedOpen ? "▲" : "▼"}
            </button>
            {relatedOpen && related.map((r, i) => (
              <div
                key={i}
                className="related-note-item"
                onClick={() => selectNote(r)}
                title={r.content}
              >
                {r.content.slice(0, 140)}
                {r.similarity != null && (
                  <span style={{ color: "var(--text-muted)", marginLeft: 4 }}>
                    [{(r.similarity * 100).toFixed(0)}%]
                  </span>
                )}
              </div>
            ))}
          </div>
        )}
      </div>
    );
  };

  return (
    <div className="panel">
      <div className="panel-header">
        🔍 Knowledge
        {notes.length > 0 && <span className="badge">{notes.length}</span>}
        <button
          className="btn"
          style={{ marginLeft: "auto", padding: "3px 10px", fontSize: 11 }}
          onClick={openCompose}
        >
          + New Note
        </button>
      </div>

      <div className="input-row">
        <input
          placeholder="Search notes, or paste a note id… (Enter)"
          value={query}
          onChange={e => setQuery(e.target.value)}
          onKeyDown={handleKey}
        />
        <select
          className="note-type-select"
          value={filterType}
          onChange={e => setFilterType(e.target.value)}
          style={{ flex: "0 0 auto" }}
        >
          <option value="">All types</option>
          {typeOptions.map(t => <option key={t} value={t}>{t}</option>)}
        </select>
        <button className="btn" onClick={() => search(query)} disabled={loading}>
          Search
        </button>
      </div>

      {filtered && (
        <DrillBanner rows={notes.length} cap={DRILL_LIMIT} onClear={() => goTo("knowledge", { view: "notes" })}>
          {drillType ? <strong>{drillType}</strong> : "all"} notes
          {drillSince && <> created since {formatLocal(drillSince)}</>}
          {drillVerified && <> checked since {formatLocal(drillVerified)}</>}
        </DrillBanner>
      )}

      {error && <div className="error-msg">{error}</div>}

      <div className="knowledge-layout">
        {/* Left: results */}
        <div className="knowledge-left">
          <div style={{ padding: "8px 12px", fontSize: 10, color: "var(--text-muted)", borderBottom: "1px solid var(--border)" }}>
            {loading ? "Searching…" : `${notes.filter(n => !filterType || n.note_type === filterType).length} notes`}
          </div>
          <div className="note-list">
            {notes.filter(n => !filterType || n.note_type === filterType).length === 0 && !loading && (
              <div className="empty-state" style={{ flex: 1, paddingTop: 40 }}>
                <span className="icon">📝</span>
                <span>
                  {filterType
                    ? `No ${filterType} notes among the ${notes.length} loaded`
                    : "No notes found"}
                </span>
              </div>
            )}
            {notes.filter(n => !filterType || n.note_type === filterType).map(n => (
              <div
                key={n.id}
                className={`note-card${selected?.id === n.id ? " selected" : ""}`}
                onClick={() => selectNote(n)}
              >
                <div className="note-card-header">
                  <span className={`note-type-badge ${noteType(n)}`}>{noteType(n)}</span>
                  {n.similarity != null && (
                    <span style={{ fontSize: 10, color: "var(--text-muted)" }}>
                      {(n.similarity * 100).toFixed(0)}%
                    </span>
                  )}
                  {n.created_at && (
                    <span style={{ fontSize: 10, color: "var(--text-muted)", marginLeft: "auto" }}>
                      {fmtDate(n.created_at)}
                    </span>
                  )}
                </div>
                <div className="note-preview">{n.content}</div>
                <div style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 2 }}>
                  {n.access_count != null && (
                    <div className="note-access">accessed {n.access_count}×</div>
                  )}
                  <span style={{ fontSize: 9, color: "var(--text-muted)", fontFamily: "monospace", userSelect: "all" }}>
                    {n.id}
                  </span>
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* Right: detail / edit / compose */}
        <div className="knowledge-right">
          {renderRight()}
        </div>
      </div>
    </div>
  );
}
