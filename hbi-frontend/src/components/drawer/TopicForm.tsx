/**
 * Create or edit a topic (HBI plan §10.2). The description is the
 * classifier's definition, so a changed one re-classifies every note while the
 * brain is idle; the name is the topic's identity and is set once.
 */
import { useState, type FormEvent } from "react";
import { createTopic, MAX_DESCRIPTION, TOPIC_NAME, updateTopic, type MemberRule, type TopicDef } from "../../api/topics";

export default function TopicForm({ existing, onSaved, onCancel }: {
  existing: TopicDef | null;
  onSaved: (name: string, note: string) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [label, setLabel] = useState(existing?.label ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [lists, setLists] = useState((existing?.source_lists ?? []).join(", "));
  const [assign, setAssign] = useState<"classifier" | "rules">(existing?.assign ?? "classifier");
  const [members, setMembers] = useState<MemberRule[]>(existing?.members ?? []);
  const [reference, setReference] = useState(existing?.reference ?? "");
  const setRule = (i: number, k: keyof MemberRule, v: string) =>
    setMembers((m) => m.map((r, j) => (j === i ? { ...r, [k]: v || undefined } : r)));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const nameOk = TOPIC_NAME.test(name);
  const definitionChanged = !existing || existing.description.trim() !== description.trim()
    || (existing.assign ?? "classifier") !== assign || JSON.stringify(existing.members ?? []) !== JSON.stringify(members);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const source_lists = lists.split(",").map((s) => s.trim()).filter(Boolean);
    const rules = assign === "rules" ? members.filter((r) => r.note_type || r.source_context || r.content_contains) : [];
    try {
      const r = existing
        ? await updateTopic(existing.name, { label, description, source_lists, assign, members: rules, reference: reference.trim() || null })
        : await createTopic({ name, label, description, source_lists, assign, members: rules, reference: reference.trim() || null });
      onSaved(r.topic.name, r.note);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="item-hand" onSubmit={submit}>
      <label>
        <span className="wk-dim">Name (its identity: lowercase, digits and hyphens; can't change later)</span>
        <input className="ui-input" value={name} disabled={!!existing} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="e.g. space-launch" />
        {!existing && name && !nameOk && <span className="tone-text-red" style={{ fontSize: 11 }}>2–40 lowercase letters, digits and hyphens</span>}
      </label>
      <label>
        <span className="wk-dim">Label (what the HBI shows)</span>
        <input className="ui-input" value={label} maxLength={60} onChange={(e) => setLabel(e.target.value)} />
      </label>
      <label>
        <span className="wk-dim">
          Definition: the local model reads only the name and this to decide which notes belong. Say what does not belong too.
        </span>
        <textarea className="ui-input" rows={5} value={description} onChange={(e) => setDescription(e.target.value)} />
        <span className={description.trim().length > MAX_DESCRIPTION ? "tone-text-red" : "wk-dim"} style={{ fontSize: 11 }}>
          {description.trim().length} / {MAX_DESCRIPTION}
          {existing && definitionChanged && " · saving re-classifies every note while the brain is idle (about 4 hours for the whole graph)"}
        </span>
      </label>
      <label>
        <span className="wk-dim">Reference (optional): a page about the topic, such as a Wikipedia article. Changing it classifies nothing again.</span>
        <input className="ui-input" type="url" value={reference} onChange={(e) => setReference(e.target.value)} placeholder="https://en.wikipedia.org/wiki/…" />
      </label>
      <label>
        <span className="wk-dim">Who puts notes in it</span>
        <select className="ui-input auto" value={assign} onChange={(e) => setAssign(e.target.value as "classifier" | "rules")}>
          <option value="classifier">The classifier, from the definition</option>
          <option value="rules">Only the rules below, and you (left out of the classifier's prompt)</option>
        </select>
        <span className="wk-dim" style={{ fontSize: 11 }}>
          Use rules for a topic about you or one of your programs: the model can't tell whose career or which agency a note is about.
        </span>
      </label>
      {assign === "rules" && (
        <div className="topic-rules">
          <span className="wk-dim">A note joins when every field a rule gives matches (text: case-insensitive, first 1,500 characters).</span>
          {members.map((r, i) => (
            <div key={i} className="topic-rule">
              <input className="ui-input" placeholder="source_context" value={r.source_context ?? ""} onChange={(e) => setRule(i, "source_context", e.target.value)} />
              <input className="ui-input" placeholder="note_type" value={r.note_type ?? ""} onChange={(e) => setRule(i, "note_type", e.target.value)} />
              <input className="ui-input" placeholder="text contains" value={r.content_contains ?? ""} onChange={(e) => setRule(i, "content_contains", e.target.value)} />
              <button type="button" className="btn-ghost" onClick={() => setMembers((m) => m.filter((_, j) => j !== i))}>×</button>
            </div>
          ))}
          <button type="button" className="btn-ghost" onClick={() => setMembers((m) => [...m, {}])}>+ rule</button>
        </div>
      )}
      <label>
        <span className="wk-dim">Source lists its searches use (comma-separated, optional)</span>
        <input className="ui-input" value={lists} onChange={(e) => setLists(e.target.value)} />
      </label>
      {error && <div className="tone-text-red" style={{ fontSize: 12 }}>{error}</div>}
      <div className="item-actions">
        <button type="submit" className="btn" disabled={busy || !label.trim() || !description.trim() || (!existing && !nameOk)}>
          {busy ? "Saving…" : existing ? "Save" : "Create topic"}
        </button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={onCancel}>Cancel</button>
      </div>
    </form>
  );
}
