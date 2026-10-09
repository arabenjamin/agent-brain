import { useEffect, useRef, useState } from "react";
import { apiRequest } from "../../api/client";
import AgentAssist from "../ui/AgentAssist";
import { todosUrl, type Todo } from "./model";

export default function TodoForm({
  existing,
  onSaved,
  onCancel,
  embedded = false,
}: {
  existing: Todo | null;
  /** Called with the saved todo's id (a new todo's id comes from the response). */
  onSaved: (id: string | null) => void;
  onCancel: () => void;
  /** Render in place (inside the drawer) instead of as a modal overlay. */
  embedded?: boolean;
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

  // The agent sees only this task, so it carries the todo as it stands.
  const buildAssistTask = (prompt: string, action?: string) => {
    const todo =
      `Todo title: ${title.trim() || "(none yet)"}\n` +
      `Current description:\n${description.trim() || "(none)"}`;
    const ask =
      action === "describe"
        ? "Write a clear description for this todo: what is to be done, what done looks like, and anything the brain already knows that bears on it."
        : action === "breakdown"
          ? "Break this todo into a short ordered checklist of concrete steps, as markdown lines starting with \"- [ ] \"."
          : prompt;
    return `${ask}\n\n${todo}\n\nReply with only the text to put in the todo's description: no preamble, no sign-off.`;
  };

  const applyAssist = (body: string, action?: string) => {
    if (!body.trim()) return "The agent's answer was empty";
    setDescription((d) => (action === "breakdown" && d.trim() ? `${d.trim()}\n\n${body.trim()}` : body.trim()));
    return null;
  };

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
        ? await apiRequest(todosUrl(`/${existing.id}`), { method: "PUT", body: JSON.stringify(body) })
        : await apiRequest(todosUrl(), { method: "POST", body: JSON.stringify(body) });

      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error((data as { error?: string }).error ?? `${res.status}`);
      const saved = data as { id?: string; todo?: { id?: string } };
      onSaved(existing?.id ?? saved.id ?? saved.todo?.id ?? null);
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

  const embeddedStyle: React.CSSProperties = { display: "flex", flexDirection: "column", gap: "12px" };

  const labelStyle: React.CSSProperties = {
    display: "flex", flexDirection: "column", gap: "4px", fontSize: "0.85rem",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)", border: "1px solid var(--border, #45475a)",
    borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.9rem",
  };

  return (
    <div style={embedded ? undefined : overlayStyle} onClick={embedded ? undefined : (e) => e.target === e.currentTarget && onCancel()}>
      <div style={embedded ? embeddedStyle : cardStyle}>
        {!embedded && <h3 style={{ margin: 0, fontSize: "1rem" }}>{existing ? "Edit Todo" : "New Todo"}</h3>}

        <form onSubmit={handleSubmit} style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <label style={labelStyle}>
            Title *
            <input ref={inputRef} style={inputStyle} value={title} onChange={(e) => setTitle(e.target.value)} required />
          </label>

          <label style={labelStyle}>
            Description
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: embedded ? "140px" : "60px" }}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </label>

          <AgentAssist
            title="Ask an agent to help with this todo"
            defaultAgent="knowledge-worker"
            placeholder="What should the agent do with this todo? Or use a preset below."
            buildTask={buildAssistTask}
            onResult={(body, _run, action) => applyAssist(body, action)}
            runLabel="Ask"
            actions={[
              { key: "describe", label: "Draft a description" },
              { key: "breakdown", label: "Break it down" },
            ]}
            note="“Break it down” adds to the description; the rest replace it"
          />

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
