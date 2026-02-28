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

export default function KnowledgePanel() {
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<Note[]>([]);
  const [selected, setSelected] = useState<Note | null>(null);
  const [related, setRelated] = useState<Note[]>([]);
  const [loading, setLoading] = useState(false);
  const [loadingRelated, setLoadingRelated] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Initial load: fetch notes due for review.
  useEffect(() => {
    loadInitial();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

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

  const search = useCallback(async (q: string) => {
    if (!q.trim()) {
      loadInitial();
      return;
    }
    setLoading(true);
    setError(null);
    setSelected(null);
    setRelated([]);
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

  const noteType = (n: Note) => n.note_type ?? "semantic";

  return (
    <div className="panel">
      <div className="panel-header">
        🔍 Knowledge
        {notes.length > 0 && <span className="badge">{notes.length}</span>}
      </div>

      <div className="input-row">
        <input
          placeholder="Search notes… (Enter)"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
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
          <div
            style={{
              padding: "8px 12px",
              fontSize: 10,
              color: "var(--text-muted)",
              borderBottom: "1px solid var(--border)",
            }}
          >
            {loading ? "Searching…" : `${notes.length} notes`}
          </div>
          <div className="note-list">
            {notes.length === 0 && !loading && (
              <div className="empty-state" style={{ flex: 1, paddingTop: 40 }}>
                <span className="icon">📝</span>
                <span>No notes found</span>
              </div>
            )}
            {notes.map((n) => (
              <div
                key={n.id}
                className={`note-card${selected?.id === n.id ? " selected" : ""}`}
                onClick={() => selectNote(n)}
              >
                <div className="note-card-header">
                  <span className={`note-type-badge ${noteType(n)}`}>
                    {noteType(n)}
                  </span>
                  {n.similarity != null && (
                    <span style={{ fontSize: 10, color: "var(--text-muted)" }}>
                      {(n.similarity * 100).toFixed(0)}%
                    </span>
                  )}
                </div>
                <div className="note-preview">{n.content}</div>
                {n.access_count != null && (
                  <div className="note-access">
                    accessed {n.access_count}×
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>

        {/* Right: detail */}
        <div className="knowledge-right">
          {selected ? (
            <div className="note-detail">
              <div style={{ marginBottom: 12 }}>
                <span className={`note-type-badge ${noteType(selected)}`}>
                  {noteType(selected)}
                </span>
                <span style={{ fontSize: 10, color: "var(--text-muted)", marginLeft: 8 }}>
                  {selected.id.slice(0, 12)}…
                </span>
              </div>
              <div className="note-full-content">{selected.content}</div>

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
          ) : (
            <div className="empty-state" style={{ flex: 1 }}>
              <span className="icon">←</span>
              <span>Select a note to read it</span>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
