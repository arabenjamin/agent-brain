import { useCallback, useEffect, useRef, useState } from "react";
import { getBrainUrl, getApiKey } from "../../api/config";

// ─── Types ────────────────────────────────────────────────────────────────────

interface Todo {
  id: string;
  title: string;
  description?: string;
  status: "pending" | "in_progress" | "done";
  priority: 0 | 1 | 2 | 3;
  tags: string[];
  due_at?: string;
  created_at: string;
  updated_at: string;
}

type StatusFilter = "all" | "pending" | "in_progress" | "done";

const PRIORITY_LABELS: Record<number, string> = {
  0: "Urgent",
  1: "High",
  2: "Normal",
  3: "Low",
};

const STATUS_LABELS: Record<string, string> = {
  pending: "Pending",
  in_progress: "In Progress",
  done: "Done",
};

// ─── API helpers ─────────────────────────────────────────────────────────────

function todosUrl(path = "") {
  return `${getBrainUrl()}/todos${path}`;
}

function authHeaders(): Record<string, string> {
  const key = getApiKey();
  return key ? { Authorization: `Bearer ${key}` } : {};
}

async function apiFetch(url: string, init: RequestInit = {}) {
  const res = await fetch(url, {
    ...init,
    headers: { "Content-Type": "application/json", ...authHeaders(), ...(init.headers ?? {}) },
  });
  return res;
}

// ─── TodoPanel ───────────────────────────────────────────────────────────────

export default function TodoPanel() {
  const [todos, setTodos] = useState<Todo[]>([]);
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editTodo, setEditTodo] = useState<Todo | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const url = filter === "all" ? todosUrl() : todosUrl(`?status=${filter}`);
      const res = await apiFetch(url);
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      const data = await res.json();
      setTodos(data.todos ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [filter]);

  useEffect(() => { refresh(); }, [refresh]);

  const handleDelete = async (id: string) => {
    if (!confirm("Delete this todo?")) return;
    try {
      const res = await apiFetch(todosUrl(`/${id}`), { method: "DELETE" });
      if (!res.ok && res.status !== 204) throw new Error(`${res.status}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleToggleDone = async (todo: Todo) => {
    const newStatus = todo.status === "done" ? "pending" : "done";
    try {
      const res = await apiFetch(todosUrl(`/${todo.id}`), {
        method: "PUT",
        body: JSON.stringify({ status: newStatus }),
      });
      if (!res.ok) throw new Error(`${res.status}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleEdit = (todo: Todo) => {
    setEditTodo(todo);
    setShowForm(true);
  };

  const handleFormClose = () => {
    setShowForm(false);
    setEditTodo(null);
  };

  const handleFormSaved = () => {
    handleFormClose();
    refresh();
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", padding: "16px", gap: "12px", overflow: "hidden" }}>
      {/* Header */}
      <div style={{ display: "flex", alignItems: "center", gap: "12px", flexWrap: "wrap" }}>
        <h2 style={{ margin: 0, fontSize: "1.1rem" }}>Todos</h2>

        {/* Status filter */}
        <div style={{ display: "flex", gap: "6px" }}>
          {(["all", "pending", "in_progress", "done"] as StatusFilter[]).map((s) => (
            <button
              key={s}
              onClick={() => setFilter(s)}
              className={`sidebar-btn${filter === s ? " active" : ""}`}
              style={{ padding: "4px 10px", fontSize: "0.8rem" }}
            >
              {s === "all" ? "All" : STATUS_LABELS[s]}
            </button>
          ))}
        </div>

        <div style={{ marginLeft: "auto", display: "flex", gap: "8px" }}>
          <button className="sidebar-btn" onClick={refresh} disabled={loading} title="Refresh">
            {loading ? "..." : "Refresh"}
          </button>
          <button className="sidebar-btn active" onClick={() => { setEditTodo(null); setShowForm(true); }}>
            + Add Todo
          </button>
        </div>
      </div>

      {error && (
        <div style={{ color: "var(--error, #f87171)", fontSize: "0.85rem" }}>{error}</div>
      )}

      {/* Todo list */}
      <div style={{ flex: 1, overflowY: "auto", display: "flex", flexDirection: "column", gap: "8px" }}>
        {todos.length === 0 && !loading && (
          <p style={{ opacity: 0.5, fontSize: "0.9rem" }}>No todos. Add one!</p>
        )}
        {todos.map((todo) => (
          <TodoRow
            key={todo.id}
            todo={todo}
            onToggleDone={() => handleToggleDone(todo)}
            onEdit={() => handleEdit(todo)}
            onDelete={() => handleDelete(todo.id)}
          />
        ))}
      </div>

      {/* Add/Edit form overlay */}
      {showForm && (
        <TodoForm
          existing={editTodo}
          onSaved={handleFormSaved}
          onCancel={handleFormClose}
        />
      )}
    </div>
  );
}

// ─── TodoRow ─────────────────────────────────────────────────────────────────

function TodoRow({
  todo,
  onToggleDone,
  onEdit,
  onDelete,
}: {
  todo: Todo;
  onToggleDone: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const done = todo.status === "done";

  return (
    <div
      style={{
        background: "var(--surface, #1e1e2e)",
        border: "1px solid var(--border, #313244)",
        borderRadius: "8px",
        padding: "10px 14px",
        display: "flex",
        alignItems: "flex-start",
        gap: "12px",
        opacity: done ? 0.6 : 1,
      }}
    >
      {/* Done checkbox */}
      <input
        type="checkbox"
        checked={done}
        onChange={onToggleDone}
        style={{ marginTop: "3px", cursor: "pointer", accentColor: "var(--accent, #89b4fa)" }}
      />

      {/* Content */}
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <span
            style={{
              fontWeight: 500,
              textDecoration: done ? "line-through" : "none",
              wordBreak: "break-word",
            }}
          >
            {todo.title}
          </span>
          <PriorityBadge priority={todo.priority} />
          <StatusBadge status={todo.status} />
        </div>
        {todo.description && (
          <p style={{ margin: "4px 0 0", fontSize: "0.82rem", opacity: 0.7, wordBreak: "break-word" }}>
            {todo.description}
          </p>
        )}
        <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", marginTop: "4px" }}>
          {todo.tags.map((t) => (
            <span key={t} style={{ fontSize: "0.72rem", background: "var(--surface2, #313244)", padding: "1px 6px", borderRadius: "4px" }}>
              {t}
            </span>
          ))}
          {todo.due_at && (
            <span style={{ fontSize: "0.72rem", opacity: 0.6 }}>Due: {todo.due_at.slice(0, 10)}</span>
          )}
        </div>
      </div>

      {/* Actions */}
      <div style={{ display: "flex", gap: "6px", flexShrink: 0 }}>
        <button
          onClick={onEdit}
          style={{ background: "none", border: "1px solid var(--border, #313244)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.78rem", color: "inherit" }}
        >
          Edit
        </button>
        <button
          onClick={onDelete}
          style={{ background: "none", border: "1px solid #f87171", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.78rem", color: "#f87171" }}
        >
          Del
        </button>
      </div>
    </div>
  );
}

// ─── Badges ──────────────────────────────────────────────────────────────────

function PriorityBadge({ priority }: { priority: number }) {
  const colors: Record<number, string> = { 0: "#f38ba8", 1: "#fab387", 2: "#89b4fa", 3: "#a6adc8" };
  const color = colors[priority] ?? "#a6adc8";
  return (
    <span style={{ fontSize: "0.72rem", color, border: `1px solid ${color}`, borderRadius: "4px", padding: "1px 5px" }}>
      {PRIORITY_LABELS[priority] ?? priority}
    </span>
  );
}

function StatusBadge({ status }: { status: string }) {
  const colors: Record<string, string> = { pending: "#a6adc8", in_progress: "#f9e2af", done: "#a6e3a1" };
  const color = colors[status] ?? "#a6adc8";
  return (
    <span style={{ fontSize: "0.72rem", color, border: `1px solid ${color}`, borderRadius: "4px", padding: "1px 5px" }}>
      {STATUS_LABELS[status] ?? status}
    </span>
  );
}

// ─── TodoForm ─────────────────────────────────────────────────────────────────

function TodoForm({
  existing,
  onSaved,
  onCancel,
}: {
  existing: Todo | null;
  onSaved: () => void;
  onCancel: () => void;
}) {
  const [title, setTitle] = useState(existing?.title ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [status, setStatus] = useState<Todo["status"]>(existing?.status ?? "pending");
  const [priority, setPriority] = useState<number>(existing?.priority ?? 2);
  const [tags, setTags] = useState(existing?.tags.join(", ") ?? "");
  const [dueAt, setDueAt] = useState(existing?.due_at?.slice(0, 10) ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => { inputRef.current?.focus(); }, []);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!title.trim()) return;
    setSaving(true);
    setError(null);

    const tagsArr = tags.split(",").map((t) => t.trim()).filter(Boolean);
    const body: Record<string, unknown> = {
      title: title.trim(),
      description: description.trim() || null,
      status,
      priority,
      tags: tagsArr,
      due_at: dueAt || null,
    };

    try {
      const res = existing
        ? await apiFetch(todosUrl(`/${existing.id}`), { method: "PUT", body: JSON.stringify(body) })
        : await apiFetch(todosUrl(), { method: "POST", body: JSON.stringify(body) });

      if (!res.ok) {
        const data = await res.json();
        throw new Error(data.error ?? `${res.status}`);
      }
      onSaved();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const overlayStyle: React.CSSProperties = {
    position: "fixed", inset: 0, background: "rgba(0,0,0,0.6)", display: "flex",
    alignItems: "center", justifyContent: "center", zIndex: 50,
  };

  const cardStyle: React.CSSProperties = {
    background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #313244)",
    borderRadius: "10px", padding: "20px", width: "min(480px, 95vw)",
    display: "flex", flexDirection: "column", gap: "12px",
  };

  const labelStyle: React.CSSProperties = {
    display: "flex", flexDirection: "column", gap: "4px", fontSize: "0.85rem",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)", border: "1px solid var(--border, #45475a)",
    borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.9rem",
  };

  return (
    <div style={overlayStyle} onClick={(e) => e.target === e.currentTarget && onCancel()}>
      <div style={cardStyle}>
        <h3 style={{ margin: 0, fontSize: "1rem" }}>{existing ? "Edit Todo" : "New Todo"}</h3>

        <form onSubmit={handleSubmit} style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <label style={labelStyle}>
            Title *
            <input ref={inputRef} style={inputStyle} value={title} onChange={(e) => setTitle(e.target.value)} required />
          </label>

          <label style={labelStyle}>
            Description
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: "60px" }}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </label>

          <div style={{ display: "flex", gap: "12px" }}>
            <label style={{ ...labelStyle, flex: 1 }}>
              Status
              <select style={inputStyle} value={status} onChange={(e) => setStatus(e.target.value as Todo["status"])}>
                <option value="pending">Pending</option>
                <option value="in_progress">In Progress</option>
                <option value="done">Done</option>
              </select>
            </label>

            <label style={{ ...labelStyle, flex: 1 }}>
              Priority
              <select style={inputStyle} value={priority} onChange={(e) => setPriority(Number(e.target.value))}>
                <option value={0}>Urgent</option>
                <option value={1}>High</option>
                <option value={2}>Normal</option>
                <option value={3}>Low</option>
              </select>
            </label>
          </div>

          <label style={labelStyle}>
            Tags (comma-separated)
            <input style={inputStyle} value={tags} onChange={(e) => setTags(e.target.value)} placeholder="e.g. work, urgent" />
          </label>

          <label style={labelStyle}>
            Due date
            <input type="date" style={inputStyle} value={dueAt} onChange={(e) => setDueAt(e.target.value)} />
          </label>

          {error && <p style={{ color: "#f87171", margin: 0, fontSize: "0.83rem" }}>{error}</p>}

          <div style={{ display: "flex", justifyContent: "flex-end", gap: "8px", marginTop: "4px" }}>
            <button type="button" className="sidebar-btn" onClick={onCancel}>Cancel</button>
            <button type="submit" className="sidebar-btn active" disabled={saving}>
              {saving ? "Saving…" : existing ? "Save" : "Add"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
