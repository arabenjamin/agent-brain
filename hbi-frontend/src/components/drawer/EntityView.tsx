/** An entity: how often it is mentioned, the notes that do, and what it appears with. */
import { useEntity } from "../../api/search";
import { formatLocal, relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity } from "./entities";
import { EntityLink, LoadState, Section } from "./parts";

export default function EntityView({ id }: { id: string }) {
  const q = useEntity(id);
  const d = q.data;
  return (
    <Drawer title={<><span>Entity</span>{d?.entity.entity_type && <Pill tone="orange">{d.entity.entity_type}</Pill>}</>} onClose={closeEntity} wide>
      {!d ? (
        <LoadState what="entity" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{d.entity.name}</h3>
          <div className="dw-dim">
            mentioned by {d.entity.mentions.toLocaleString()} notes · first seen {formatLocal(d.entity.created_at)} · <code>{d.entity.id}</code>
          </div>
          <div className="settings-actions">
            <a className="btn-ghost" href={hrefTo("knowledge", { view: "list", q: `"${d.entity.name ?? ""}"`, mode: "keyword" })}>
              Search notes for “{d.entity.name}”
            </a>
          </div>

          <Section title="Mentioned with" aside={d.co_mentioned.length ? "by shared notes" : undefined}>
            {d.co_mentioned.length === 0 ? (
              <div className="dw-note">No other entity shares a note with this one (URLs and dates are not counted).</div>
            ) : (
              <div className="dw-chips">
                {d.co_mentioned.map((c) => (
                  <EntityLink key={c.id} kind="entity" id={c.id} title={`${c.entity_type ?? ""} · ${c.shared} shared notes`}>
                    <Pill tone="muted">{c.name} {c.shared}</Pill>
                  </EntityLink>
                ))}
              </div>
            )}
          </Section>

          <Section title="Notes" aside={d.entity.mentions > d.notes_shown ? `newest ${d.notes_shown} of ${d.entity.mentions}` : `${d.notes_shown}`}>
            {d.notes.length === 0 ? (
              <EmptyState title="No notes mention it" checked="It has no MENTIONS edges left (the notes may have been pruned or merged)." />
            ) : (
              <ul className="dw-list">
                {d.notes.map((n) => (
                  <li key={n.id}>
                    <EntityLink kind="note" id={n.id}><Pill tone="accent">{n.note_type ?? "note"}</Pill></EntityLink>
                    <span className="dw-clip">{n.preview}</span>
                    <span className="dw-dim">{relTime(n.created_at)}</span>
                  </li>
                ))}
              </ul>
            )}
          </Section>
        </>
      )}
    </Drawer>
  );
}
