/**
 * A note's topics, editable (HBI plan Phase 15). Any change sets them by hand
 * and locks them: the classifier leaves the note alone until "Let the
 * classifier decide". The first topic is the main one; at most two.
 */
import { useState } from "react";
import type { Note } from "../../api/queries";
import { setNoteTopics, unlockNoteTopics, useTopics } from "../../api/topics";
import { Pill } from "../ui";
import { EntityLink } from "./parts";

export default function NoteTopics({ n }: { n: Note }) {
  const all = useTopics();
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const current = n.topics ?? [];
  const labels = new Map((all.data?.topics ?? []).map((t) => [t.name, t.label]));

  if (n.parent_id) return <span className="dw-dim">a chunk: its topics are its parent note's</span>;

  const run = async (f: () => Promise<unknown>, done: string) => {
    setBusy(true);
    setMsg(null);
    try {
      await f();
      setMsg({ ok: true, text: done });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };
  const set = (topics: string[]) => run(() => setNoteTopics(n.id, topics), "Set by you; the classifier leaves this note alone.");
  const addable = (all.data?.topics ?? []).filter((t) => !current.includes(t.name));

  return (
    <span className="note-topics">
      {current.length === 0 && (
        <span className="dw-dim">
          {n.topics_locked ? "none (set by you)" : n.topics_assessed === false ? "not classified yet (runs while the brain is idle)" : "none: classified as fitting no topic"}
        </span>
      )}
      {current.map((t, i) => (
        <span key={t} className="note-topic">
          <EntityLink kind="topic" id={t} title={i === 0 ? "Main topic" : "Second topic"}>
            <Pill tone={i === 0 ? "accent" : "muted"}>{labels.get(t) ?? t}</Pill>
          </EntityLink>
          {i === 1 && (
            <button type="button" className="btn-ghost note-topic-x" disabled={busy} title="Make this the main topic"
              onClick={() => set([t, current[0]])}>↑ main</button>
          )}
          <button type="button" className="btn-ghost note-topic-x" disabled={busy} title={`Remove ${labels.get(t) ?? t} from this note`}
            onClick={() => set(current.filter((x) => x !== t))}>×</button>
        </span>
      ))}
      {current.length < 2 && addable.length > 0 && (
        <select className="ui-input auto" disabled={busy} value="" title="Add a topic (the first is the main one)"
          onChange={(e) => e.target.value && set([...current, e.target.value])}>
          <option value="">+ topic</option>
          {addable.map((t) => <option key={t.name} value={t.name}>{t.label}</option>)}
        </select>
      )}
      {n.topics_locked ? (
        <>
          <Pill tone="cyan">set by you</Pill>
          <button type="button" className="btn-ghost" disabled={busy}
            onClick={() => run(() => unlockNoteTopics(n.id), "The classifier decides again while the brain is idle.")}>Let the classifier decide</button>
        </>
      ) : current.length > 0 && (
        <button type="button" className="btn-ghost" disabled={busy} title="Keep these topics: the classifier will not change them"
          onClick={() => set(current)}>Lock</button>
      )}
      {msg && <span className={msg.ok ? "tone-text-green" : "tone-text-red"} style={{ fontSize: 11 }}>{msg.text}</span>}
    </span>
  );
}
