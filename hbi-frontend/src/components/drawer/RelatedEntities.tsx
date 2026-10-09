/**
 * "Mentioned with": the entities that share notes with this one, strongest
 * first (HBI plan Phase 16), optionally within one topic. Each row opens the
 * notes the two share: the evidence for the pair, since the graph has no
 * entity→entity edges yet.
 */
import { useState } from "react";
import { usePair, useRelated } from "../../api/related";
import { noteTone } from "../../lib/noteTypes";
import { relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { EmptyState, Pill } from "../ui";
import { EntityLink, LoadState, Section } from "./parts";

export function PairNotes({ a, b, topic }: { a: string; b: string; topic?: string }) {
  const q = usePair(a, b, topic);
  if (!q.data) return <LoadState what="shared notes" id={`${a} + ${b}`} error={q.error} onRetry={() => q.refetch()} />;
  const d = q.data;
  return (
    <div className="rel-pair">
      <div className="dw-dim">
        {d.shared} {d.shared === 1 ? "note mentions" : "notes mention"} both {d.a.name} and {d.b.name}
        {d.topic ? ` in ${d.topic}` : ""}{d.returned < d.shared ? `; the newest ${d.returned} shown` : ""}
      </div>
      <ul className="dw-list">
        {d.notes.map((n) => (
          <li key={n.id}>
            <EntityLink kind="note" id={n.id}><Pill tone={noteTone(n.note_type)}>{n.note_type ?? "note"}</Pill></EntityLink>
            <span className="dw-clip">{n.preview}{n.preview_truncated ? "…" : ""}</span>
            <span className="dw-dim">{relTime(n.created_at)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

export default function RelatedEntities({ id, topics }: { id: string; topics: { name: string; label: string | null }[] }) {
  const [topic, setTopic] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const q = useRelated(id, topic || undefined);
  const d = q.data;
  const peak = Math.max(1e-9, ...(d?.related.map((r) => r.strength) ?? [0]));
  return (
    <Section
      title="Mentioned with"
      aside={d ? (d.total > d.returned ? `strongest ${d.returned} of ${d.total}` : `${d.total}`) : undefined}
    >
      <div className="item-actions">
        {topics.length > 0 && (
          <select className="ui-input auto" value={topic} onChange={(e) => { setTopic(e.target.value); setOpen(null); }} title="Count only one topic's notes">
            <option value="">in all notes</option>
            {topics.map((t) => <option key={t.name} value={t.name}>in {t.label ?? t.name}</option>)}
          </select>
        )}
        <a className="btn-ghost" href={hrefTo("knowledge", { view: "map", focus: id, scope: topic || undefined })}>Neighbourhood on the map</a>
      </div>
      {!d ? (
        <LoadState what="related entities" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : d.related.length === 0 ? (
        <EmptyState
          title="Shares no note with another entity"
          checked={topic ? `None of the ${topic} notes that mention it name another entity (URLs and dates are not counted).` : "No note that mentions it names another entity (URLs and dates are not counted)."}
        />
      ) : (
        <>
          <ul className="dw-list rel-list">
            {d.related.map((r) => (
              <li key={r.id} className="rel-row">
                <EntityLink kind="entity" id={r.id} title={`${r.entity_type ?? "entity"} · mentioned by ${r.mentions} notes`}>{r.name}</EntityLink>
                <span className="rel-bar" title={`strength ${r.strength.toFixed(3)}`}><span style={{ width: `${Math.round((100 * r.strength) / peak)}%` }} /></span>
                <button type="button" className="btn-ghost" onClick={() => setOpen(open === r.id ? null : r.id)}
                  title="The notes that mention both">{r.shared} shared {open === r.id ? "▾" : "▸"}</button>
                {open === r.id && <PairNotes a={id} b={r.id} topic={topic || undefined} />}
              </li>
            ))}
          </ul>
          <div className="dw-dim" style={{ fontSize: 11 }}>
            Ranked by strength: {d.method}. A name mentioned everywhere doesn't top every list.
          </div>
        </>
      )}
    </Section>
  );
}
