/**
 * Entity merge proposals: "is `vance` the same as `jd vance`?" Each answer is
 * one click — merge (either way round), different, or related — and a merge
 * can be undone from the entity's drawer. Used by the Work inbox and by an
 * entity's drawer (narrowed to that entity).
 */
import { useState } from "react";
import { decideEntities, mergeEntities, REASON_LABEL, type MergeCandidate } from "../../api/entities";
import { EntityLink } from "../drawer/parts";

function Side({ s }: { s: MergeCandidate["from"] }) {
  return (
    <EntityLink kind="entity" id={s.id} title={`${s.entity_type} · ${s.mentions} notes`}>
      <strong>{s.name}</strong> <span className="wk-dim">{s.mentions}</span>
    </EntityLink>
  );
}

function CandidateRow({ c }: { c: MergeCandidate }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const act = (f: () => Promise<unknown>) => async () => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <div className="wk-row wk-todo em-row">
      <span className="wk-row-main">
        <Side s={c.from} /> <span className="wk-dim">{REASON_LABEL[c.reason]}</span> <Side s={c.into} />
        <span className="wk-dim">
          {" "}· {c.shared_notes} shared {c.shared_notes === 1 ? "note" : "notes"}
          {c.rivals > 0 && ` · ${c.rivals} other ${c.rivals === 1 ? "name" : "names"} it could be`}
        </span>
        {error && <span className="tone-text-red"> {error}</span>}
      </span>
      <span className="wk-row-meta">
        <button type="button" className="btn" disabled={busy} title={`Fold “${c.from.name}” into “${c.into.name}”`}
          onClick={act(() => mergeEntities(c.from.id, c.into.id))}>Merge</button>
        <button type="button" className="btn-ghost" disabled={busy} title={`Fold “${c.into.name}” into “${c.from.name}” instead`}
          onClick={act(() => mergeEntities(c.into.id, c.from.id))}>⇄</button>
        <button type="button" className="btn-ghost" disabled={busy} title="Different things: don't ask again"
          onClick={act(() => decideEntities(c.from.id, c.into.id, "different"))}>Different</button>
        <button type="button" className="btn-ghost" disabled={busy} title="Related but not the same: link them, don't merge"
          onClick={act(() => decideEntities(c.from.id, c.into.id, "related"))}>Related</button>
      </span>
    </div>
  );
}

export default function MergeCandidates({ items }: { items: MergeCandidate[] }) {
  return (
    <>
      {items.map((c) => <CandidateRow key={`${c.from.id}-${c.into.id}`} c={c} />)}
    </>
  );
}
