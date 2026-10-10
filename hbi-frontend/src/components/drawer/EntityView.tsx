/**
 * An entity: how often it is mentioned, the notes that do, what it appears
 * with — and the names folded into it, each of which can be undone. Merge
 * proposals that involve it are answered here as in the Work inbox, and
 * "Merge into…" handles any pair the proposals don't find.
 */
import { useState } from "react";
import { findEntities, isMergedAway, mergeEntities, unmergeAlias, useMergeCandidates } from "../../api/entities";
import { useEntity, type EntityAlias, type EntityDetail, type EntityHit } from "../../api/search";
import { setEntityHidden } from "../../api/topics";
import { formatLocal, relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import MergeCandidates from "../work/MergeCandidates";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity, openEntity } from "./entities";
import { EntityLink, LoadState, Section } from "./parts";
import RelatedEntities from "./RelatedEntities";
import EntityRelations from "./EntityRelations";
import EntityReference from "./EntityReference";

const METHOD_LABEL: Record<string, string> = {
  seed: "curated seed",
  normalise: "same spelling",
  proposal: "merged by you",
  restored: "from a snapshot",
};

export default function EntityView({ id }: { id: string }) {
  const q = useEntity(id);
  const d = q.data;
  if (isMergedAway(d)) {
    return (
      <Drawer title="Entity" onClose={closeEntity}>
        <EmptyState
          title="Merged into another entity"
          checked={<>
            <code>{id}</code> was “{d.merged_into.alias}”, which is now an alias of{" "}
            {d.merged_into.id ? <EntityLink kind="entity" id={d.merged_into.id}>{d.merged_into.name}</EntityLink> : d.merged_into.name}.
            Undo it from there to bring it back.
          </>}
        />
      </Drawer>
    );
  }
  return (
    <Drawer title={<><span>Entity</span>{d?.entity.entity_type && <Pill tone="orange">{d.entity.entity_type}</Pill>}</>} onClose={closeEntity} wide>
      {!d ? <LoadState what="entity" id={id} error={q.error} onRetry={() => q.refetch()} /> : <Body d={d} />}
    </Drawer>
  );
}

function Body({ d }: { d: EntityDetail }) {
  const id = d.entity.id;
  const cands = useMergeCandidates(20, id);
  return (
    <>
      <h3 className="dw-title">{d.entity.name}</h3>
      <div className="dw-dim">
        mentioned by {d.entity.mentions.toLocaleString()} notes · first seen {formatLocal(d.entity.created_at)} · <code>{id}</code>
        {d.entity.type_source === "seed" && " · name and type from the curated seed"}
        {d.entity.distinct && " · split out by an undo, never folded automatically"}
      </div>
      <div className="settings-actions">
        <a className="btn-ghost" href={hrefTo("knowledge", { view: "list", q: `"${d.entity.name ?? ""}"`, mode: "keyword" })}>
          Search notes for “{d.entity.name}”
        </a>
      </div>

      <EntityReference id={id} name={d.entity.name ?? id} />

      <Aliases aliases={d.aliases ?? []} />

      <InTopics id={id} name={d.entity.name ?? id} topics={d.topics ?? []} />

      {(cands.data?.candidates.length ?? 0) > 0 && (
        <Section title="Maybe the same as" aside={`${cands.data!.total}`}>
          <MergeCandidates items={cands.data!.candidates} />
        </Section>
      )}

      <EntityRelations id={id} name={d.entity.name ?? id} />

      <RelatedEntities id={id} topics={(d.topics ?? []).filter((t) => !t.hidden)} />

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

      <MergeInto self={d} />
    </>
  );
}

function Aliases({ aliases }: { aliases: EntityAlias[] }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ tone: "red" | "muted"; text: string } | null>(null);
  // Kept mounted so an undo's outcome stays visible after its alias is gone.
  if (aliases.length === 0) return msg ? <div className={msg.tone === "red" ? "tone-text-red" : "dw-note"}>{msg.text}</div> : null;
  const undo = async (name: string) => {
    if (!window.confirm(`Split “${name}” back out as its own entity, with the mentions that came through it?`)) return;
    setBusy(name);
    setMsg(null);
    try {
      const r = await unmergeAlias(name);
      setMsg({
        tone: "muted",
        text: `“${name}” is its own entity again (${r.unmerged.mentions_moved} mentions moved back).` +
          (r.still_in_seed ? " It is still listed in entities/aliases.yaml; remove it there too to keep the file honest." : ""),
      });
    } catch (e) {
      setMsg({ tone: "red", text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(null);
    }
  };
  return (
    <Section title="Also known as" aside={`${aliases.length}`}>
      <ul className="dw-list">
        {aliases.map((a) => (
          <li key={a.name}>
            <strong>{a.name}</strong>
            <span className="dw-dim">
              {METHOD_LABEL[a.method ?? ""] ?? a.method ?? "alias"}
              {a.case_sensitive && " · capitals only"}
              {" · "}{a.mentions} {a.mentions === 1 ? "mention" : "mentions"} through it
              {a.merged_at && ` · merged ${relTime(a.merged_at)}`}
            </span>
            {a.name && (
              <button type="button" className="btn-ghost" disabled={busy !== null} onClick={() => undo(a.name!)}
                title="Bring this name back as its own entity">
                {busy === a.name ? "Undoing…" : "Undo"}
              </button>
            )}
          </li>
        ))}
      </ul>
      {msg && <div className={msg.tone === "red" ? "tone-text-red" : "dw-note"}>{msg.text}</div>}
    </Section>
  );
}

/** Fold this entity into one a person names (for pairs the proposals miss). */
function MergeInto({ self }: { self: EntityDetail }) {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<EntityHit[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const find = async () => {
    setError(null);
    try {
      setHits((await findEntities(q.trim())).filter((h) => h.id !== self.entity.id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const merge = async (into: EntityHit) => {
    if (!window.confirm(`Merge “${self.entity.name}” into “${into.name}”? Its ${self.entity.mentions} mentions move; “${self.entity.name}” stays as an alias you can undo.`)) return;
    setBusy(true);
    try {
      await mergeEntities(self.entity.id, into.id);
      openEntity("entity", into.id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <Section title="Merge into…">
      <form className="settings-actions" onSubmit={(e) => { e.preventDefault(); if (q.trim()) void find(); }}>
        <input className="ui-input" value={q} onChange={(e) => setQ(e.target.value)} placeholder="Name of the entity this is the same as" />
        <button type="submit" className="btn-ghost" disabled={!q.trim()}>Find</button>
      </form>
      {error && <div className="tone-text-red">{error}</div>}
      {hits && hits.length === 0 && <div className="dw-note">No entity name or alias matches “{q.trim()}”.</div>}
      {hits && hits.length > 0 && (
        <ul className="dw-list">
          {hits.map((h) => (
            <li key={h.id}>
              <EntityLink kind="entity" id={h.id}><strong>{h.name}</strong></EntityLink>
              <span className="dw-dim">{h.entity_type ?? ""} · {h.mentions} notes</span>
              <button type="button" className="btn" disabled={busy} onClick={() => merge(h)}>Merge into this</button>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

/**
 * The topics this entity appears in through their notes, and the ones the user
 * keeps it out of (HBI plan Phase 15). Hiding changes no note: the entity just
 * stops showing under that topic on the map and in its drawer.
 */
function InTopics({ id, name, topics }: { id: string; name: string; topics: NonNullable<EntityDetail["topics"]> }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const toggle = async (topic: string, hidden: boolean) => {
    setBusy(topic);
    setError(null);
    try {
      await setEntityHidden(id, topic, hidden);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <Section title="In topics" aside="through the notes that mention it">
      {topics.length === 0 ? (
        <div className="dw-note">None of the notes that mention it are in a topic yet.</div>
      ) : (
        <ul className="dw-list">
          {topics.map((t) => (
            <li key={t.name}>
              <EntityLink kind="topic" id={t.name}>{t.label ?? t.name}</EntityLink>
              <span className="dw-dim">{t.notes} {t.notes === 1 ? "note" : "notes"}</span>
              {t.hidden ? (
                <>
                  <Pill tone="yellow">kept out by you</Pill>
                  <button type="button" className="btn-ghost" disabled={busy === t.name} onClick={() => toggle(t.name, false)}>Let back in</button>
                </>
              ) : (
                <button type="button" className="btn-ghost" disabled={busy === t.name}
                  title={`Stop showing ${name} under ${t.label ?? t.name}; its notes are unchanged`}
                  onClick={() => toggle(t.name, true)}>Keep out</button>
              )}
            </li>
          ))}
        </ul>
      )}
      {error && <div className="tone-text-red" style={{ fontSize: 12 }}>{error}</div>}
    </Section>
  );
}
