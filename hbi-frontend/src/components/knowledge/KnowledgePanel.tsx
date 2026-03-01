import { useCallback, useEffect, useState } from "react";
import { callTool } from "../../api/mcp";

interface Note {
  id: string;
  content: string;
  note_type?: string;
  access_count?: number;
  created_at?: string;
  similarity?: number;
}

const NOTE_TYPES = ["semantic", "episodic", "reflection", "consolidated"];

export default function KnowledgePanel() {
  const [query, setQuery]               = useState("");
  const [notes, setNotes]               = useState<Note[]>([]);
  const [selected, setSelected]         = useState<Note | null>(null);
  const [related, setRelated]           = useState<Note[]>([]);
  const [loading, setLoading]           = useState(false);
  const [loadingRelated, setLoadingRelated] = useState(false);
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

  const loadInitial = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const json = await callTool("review_due_notes", { limit: 20 });
      const data = JSON.parse(json);
      setNotes(data.notes ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { loadInitial(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const search = useCallback(async (q: string) => {
    if (!q.trim()) { loadInitial(); return; }
    setLoading(true);
    setError(null);
    setSelected(null);
    setRelated([]);
    setEditing(false);
    setConfirmDelete(false);
    try {
      const json = await callTool("search_notes", { query: q, limit: 30 });
      const data = JSON.parse(json);
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
    setLoadingRelated(true);
    try {
      const json = await callTool("find_related_notes", { note_id: note.id });
      const data = JSON.parse(json);
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
      const json = await callTool("store_note", {
        content:   newContent.trim(),
        note_type: newNoteType,
      });
      const data = JSON.parse(json);
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
      await callTool("update_note", { id: selected.id, content: editContent.trim() });
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
      await callTool("delete_note", { id: selected.id });
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
            <span style={{ fontSize: 10, color: "var(--text-muted)" }}>{selected.id.slice(0, 12)}…</span>
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
          <span style={{ fontSize: 10, color: "var(--text-muted)", marginLeft: 8 }}>
            {selected.id.slice(0, 12)}…
          </span>
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

        <div className="note-full-content scroll">{selected.content}</div>

        {selected.access_count != null && (
          <div style={{ fontSize: 10, color: "var(--text-muted)", padding: "4px 0" }}>
            Accessed {selected.access_count}×
          </div>
        )}

        {(loadingRelated || related.length > 0) && (
          <div className="related-section">
            <div className="related-section-title">
              Related ({loadingRelated ? "…" : related.length})
            </div>
            {related.map((r, i) => (
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
          placeholder="Search notes… (Enter)"
          value={query}
          onChange={e => setQuery(e.target.value)}
          onKeyDown={handleKey}
        />
        <button className="btn" onClick={() => search(query)} disabled={loading}>
          Search
        </button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div className="knowledge-layout">
        {/* Left: results */}
        <div className="knowledge-left">
          <div style={{ padding: "8px 12px", fontSize: 10, color: "var(--text-muted)", borderBottom: "1px solid var(--border)" }}>
            {loading ? "Searching…" : `${notes.length} notes`}
          </div>
          <div className="note-list">
            {notes.length === 0 && !loading && (
              <div className="empty-state" style={{ flex: 1, paddingTop: 40 }}>
                <span className="icon">📝</span>
                <span>No notes found</span>
              </div>
            )}
            {notes.map(n => (
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
                </div>
                <div className="note-preview">{n.content}</div>
                {n.access_count != null && (
                  <div className="note-access">accessed {n.access_count}×</div>
                )}
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
