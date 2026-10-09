/**
 * An entity's typed relationships (HBI plan Phase 17): the ones you set, the
 * ones you accepted, and the extracted proposals waiting for you, each read
 * from this entity's side ("works for", "employs") with the notes that state
 * it. Add one by hand, change its type, or remove it; removing an extracted
 * one keeps it as rejected so it is never proposed again.
 */
import { useState } from "react";
import { findEntities } from "../../api/entities";
import { decideRelation, relabelRelation, removeRelation, setRelation, useEntityRelations, useRelationTypes, type Relation, type RelType } from "../../api/relations";
import type { EntityHit } from "../../api/search";
import { Pill } from "../ui";
import { EntityLink, LoadState, Section } from "./parts";

function useAct() {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const act = async (f: () => Promise<unknown>, done: string) => {
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
  const note = msg && <span className={msg.ok ? "tone-text-green" : "tone-text-red"} style={{ fontSize: 11 }}>{msg.text}</span>;
  return { busy, act, note };
}

export function RelationRow({ r, types }: { r: Relation; types: RelType[] }) {
  const { busy, act, note } = useAct();
  const proposed = r.status === "proposed";
  return (
    <li className="rel-typed">
      <span>
        <b>{r.reads}</b>{" "}
        {r.other.id ? <EntityLink kind="entity" id={r.other.id}>{r.other.name}</EntityLink> : r.other.name}
      </span>
      {r.method === "manual" ? <Pill tone="cyan">set by you</Pill>
        : proposed ? <Pill tone="yellow" title={r.model ? `extracted by ${r.model}` : undefined}>proposed</Pill>
        : <Pill tone="muted" title={r.model ? `extracted by ${r.model}` : undefined}>accepted</Pill>}
      {r.evidence.slice(0, 3).map((id, i) => (
        <EntityLink key={id} kind="note" id={id} title="The note that states it">{i === 0 ? "evidence" : `+${i}`}</EntityLink>
      ))}
      <span className="item-actions">
        {proposed && (
          <>
            <button type="button" className="btn" disabled={busy} onClick={() => act(() => decideRelation(r, true), "Accepted.")}>Accept</button>
            <button type="button" className="btn-ghost" disabled={busy} onClick={() => act(() => decideRelation(r, false), "Rejected; it won't be proposed again.")}>Reject</button>
          </>
        )}
        <select className="ui-input auto" disabled={busy} value="" title="Change its type (yours from then on)"
          onChange={(e) => e.target.value && act(() => relabelRelation(r, e.target.value), "Type changed.")}>
          <option value="">type…</option>
          {types.filter((t) => t.name !== r.type).map((t) => <option key={t.name} value={t.name}>{t.label}</option>)}
        </select>
        {!proposed && (
          <button type="button" className="btn-ghost" disabled={busy}
            title={r.method === "llm" ? "Kept as rejected, so it is not proposed again" : "Delete it"}
            onClick={() => act(() => removeRelation(r), "Removed.")}>Remove</button>
        )}
        {note}
      </span>
    </li>
  );
}

/** Add a relationship by hand: a type, a direction, and the other entity. */
export function AddRelation({ self, selfName, other, noteId }: { self: string; selfName: string; other?: { id: string; name: string }; noteId?: string }) {
  const types = useRelationTypes();
  const [type, setType] = useState("");
  const [outgoing, setOutgoing] = useState(true);
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<EntityHit[] | null>(null);
  const [target, setTarget] = useState<{ id: string; name: string } | null>(other ?? null);
  const { busy, act, note } = useAct();
  const t = types.data?.types.find((x) => x.name === type);
  const find = async () => setHits((await findEntities(q.trim())).filter((h) => h.id !== self));
  const reads = t && target ? (outgoing ? `${selfName} ${t.label} ${target.name}` : `${target.name} ${t.label} ${selfName}`) : null;
  return (
    <div className="item-hand">
      <div className="item-actions">
        <select className="ui-input auto" value={type} onChange={(e) => setType(e.target.value)}>
          <option value="">relationship…</option>
          {(types.data?.types ?? []).map((x) => <option key={x.name} value={x.name} title={x.definition}>{x.label}</option>)}
        </select>
        {t && !t.symmetric && (
          <button type="button" className="btn-ghost" onClick={() => setOutgoing((o) => !o)} title="Swap the direction">⇄ swap</button>
        )}
      </div>
      {!other && (
        <form className="item-actions" onSubmit={(e) => { e.preventDefault(); if (q.trim()) void find(); }}>
          <input className="ui-input" value={q} onChange={(e) => setQ(e.target.value)} placeholder="The other entity's name" />
          <button type="submit" className="btn-ghost" disabled={!q.trim()}>Find</button>
        </form>
      )}
      {!other && hits && (hits.length === 0
        ? <div className="dw-note">No entity name or alias matches “{q.trim()}”.</div>
        : (
          <div className="dw-chips">
            {hits.slice(0, 8).map((h) => (
              <button key={h.id} type="button" className={target?.id === h.id ? "btn" : "btn-ghost"} onClick={() => setTarget({ id: h.id, name: h.name })}>
                {h.name} <span className="dw-dim">{h.entity_type ?? ""}</span>
              </button>
            ))}
          </div>
        ))}
      {t && <div className="dw-dim" style={{ fontSize: 11 }}>{t.definition}</div>}
      <div className="item-actions">
        <button type="button" className="btn" disabled={busy || !t || !target}
          onClick={() => target && t && act(
            () => (outgoing ? setRelation(self, target.id, t.name, noteId) : setRelation(target.id, self, t.name, noteId)),
            "Added.",
          )}>{reads ? `Add: ${reads}` : "Add"}</button>
        {note}
      </div>
    </div>
  );
}

export default function EntityRelations({ id, name }: { id: string; name: string }) {
  const q = useEntityRelations(id);
  const types = useRelationTypes();
  const [adding, setAdding] = useState(false);
  const d = q.data;
  const proposed = d?.relations.filter((r) => r.status === "proposed").length ?? 0;
  return (
    <Section title="Relationships" aside={d ? `${d.relations.length}${proposed ? ` · ${proposed} to review` : ""}${d.rejected ? ` · ${d.rejected} rejected` : ""}` : undefined}>
      {!d ? <LoadState what="relationships" id={id} error={q.error} onRetry={() => q.refetch()} /> : (
        <>
          {d.relations.length === 0 && (
            <div className="dw-note">
              No typed relationship yet{d.rejected ? ` (${d.rejected} rejected by you)` : ""}. Extracted ones arrive from claims as proposals; "Mentioned with" below shows the entities it shares notes with.
            </div>
          )}
          <ul className="dw-list">{d.relations.map((r) => <RelationRow key={`${r.from}|${r.type}|${r.to}`} r={r} types={types.data?.types ?? []} />)}</ul>
        </>
      )}
      <button type="button" className="btn-ghost" onClick={() => setAdding((a) => !a)}>{adding ? "Cancel" : "+ Relationship"}</button>
      {adding && <AddRelation self={id} selfName={name} />}
    </Section>
  );
}
