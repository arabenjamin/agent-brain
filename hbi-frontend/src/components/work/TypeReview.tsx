/**
 * Entity types to check (HBI plan Phase 19): entities whose reference link
 * says they are a different type than the one they are filed under ("Filed
 * as organisation; its reference says municipality"). The brain never
 * retypes on its own: a quarter of the measured disagreements came from
 * wrong links. The Feed's "entity types to check" card opens this
 * (`#/work?view=feed&mode=types`).
 */
import { useState } from "react";
import { answerTypeQuestion, useEntityTypes, type TypeQuestion } from "../../api/types";
import { hrefTo } from "../../nav";
import { EntityLink } from "../drawer/parts";
import { EmptyState, Pill } from "../ui";

const SHOWN = 100;

function Row({ q }: { q: TypeQuestion }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const answer = async (a: "keep" | "reference") => {
    setBusy(true);
    setError(null);
    try {
      await answerTypeQuestion(q.id, a);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };
  return (
    <div className="wk-row rel-review">
      <span className="wk-row-main">
        <EntityLink kind="entity" id={q.id}>{q.name}</EntityLink>{" "}
        <span className="wk-dim">filed as</span> <b>{q.entity_type}</b>{" "}
        <span className="wk-dim">· its reference says</span> <b>{q.kind}</b> <span className="wk-dim">({q.kind_type})</span>
        <div className="wk-dim rel-review-quote">
          {q.label ?? q.qid}{q.description && ` — ${q.description}`}{" "}
          {q.link_status === "proposed" ? <Pill tone="yellow">suggested link</Pill> : <Pill tone="green">confirmed link</Pill>}
          {` · ${q.mentions} notes`}
        </div>
        {error && <span className="tone-text-red"> · {error}</span>}
      </span>
      <span className="wk-row-meta wk-feed-actions">
        <button type="button" className="btn" disabled={busy} onClick={() => answer("reference")}>Use {q.kind_type}</button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={() => answer("keep")}
          title="Keep the filed type; this reference won't ask again">Keep {q.entity_type}</button>
      </span>
    </div>
  );
}

export default function TypeReview() {
  const q = useEntityTypes(SHOWN);
  const d = q.data;
  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <a className="btn-ghost" href={hrefTo("work", { view: "feed" })}>← Feed</a>
        <span className="wk-dim">{d ? `${d.total} to check, ${d.questions.length} shown, most-mentioned first` : "Loading…"}</span>
      </div>
      {q.error && <EmptyState tone="red" title="Couldn't load the type questions" checked={q.error.message} />}
      <div className="wk-hint">
        Each entity's reference link says it is a different type than the one it is filed under. Use the reference's type, keep the
        filed one (this link won't ask again), or open the entity: if the reference itself is wrong, rule it out with “Not this”, or set
        a type and kind by hand. Nothing changes until you answer.
      </div>
      {d && d.total === 0 && (
        <EmptyState title="Nothing to check" checked="No reference link disagrees with its entity's type. Links are added hourly by the entity-linking schedule." />
      )}
      {d?.questions.map((x) => <Row key={x.id} q={x} />)}
    </div>
  );
}
