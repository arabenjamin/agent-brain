/**
 * Relationships to review (HBI plan Phase 17): what the extractor proposed
 * from claims, newest first, each with the sentence that states it. Accept
 * makes it a relationship; reject keeps it out for good. The Feed's
 * "relationships to review" card opens this (`#/work?view=feed&mode=relations`).
 */
import { useState } from "react";
import { decideRelation, useProposedRelations, type ProposedRelation } from "../../api/relations";
import { hrefTo } from "../../nav";
import { EntityLink } from "../drawer/parts";
import { EmptyState } from "../ui";

const SHOWN = 40;

function Row({ r }: { r: ProposedRelation }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const decide = async (accept: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await decideRelation(r, accept);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <div className="wk-row rel-review">
      <span className="wk-row-main">
        <EntityLink kind="entity" id={r.from}>{r.from_name}</EntityLink> <b>{r.reads}</b>{" "}
        {r.other.id ? <EntityLink kind="entity" id={r.other.id}>{r.other.name}</EntityLink> : r.other.name}
        {r.evidence_text && (
          <div className="wk-dim rel-review-quote">
            “{r.evidence_text.slice(0, 260)}{r.evidence_text.length > 260 ? "…" : ""}”{" "}
            {r.evidence[0] && <EntityLink kind="note" id={r.evidence[0]}>claim</EntityLink>}
            {r.evidence.length > 1 && ` · stated in ${r.evidence.length} claims`}
          </div>
        )}
        {error && <span className="tone-text-red"> · {error}</span>}
      </span>
      <span className="wk-row-meta wk-feed-actions">
        <button type="button" className="btn" disabled={busy} onClick={() => decide(true)}>Accept</button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={() => decide(false)} title="It won't be proposed again">Reject</button>
      </span>
    </div>
  );
}

export default function RelationReview() {
  const q = useProposedRelations(SHOWN);
  const d = q.data;
  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <a className="btn-ghost" href={hrefTo("work", { view: "feed" })}>← Feed</a>
        <span className="wk-dim">{d ? `${d.total} to review, the newest ${d.returned} shown` : "Loading…"}</span>
      </div>
      {q.error && <EmptyState tone="red" title="Couldn't load the proposals" checked={q.error.message} />}
      <div className="wk-hint">
        Relationships a cloud model read in claims. Measured on claims it hadn't seen, about two in three were right, so each waits for
        you: accept it, reject it (it is never proposed again), or open an entity to change its type.
      </div>
      {d && d.total === 0 && <EmptyState title="Nothing to review" checked="No proposed relationship is waiting; the extraction schedule adds more every 30 minutes while claims remain." />}
      {d?.proposed.map((r) => <Row key={`${r.from}|${r.type}|${r.to}`} r={r} />)}
    </div>
  );
}
