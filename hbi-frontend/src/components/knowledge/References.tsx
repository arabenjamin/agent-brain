/**
 * Reference-link coverage (HBI plan Phase 18): of the entities named in claims
 * (the public ones background lookups cover), how many are linked to a
 * Wikidata item, have none, or are still waiting, and which ones.
 * `#/knowledge?view=references&bucket=confirmed|proposed|none|waiting`.
 */
import { useWikiCoverage } from "../../api/wiki";
import { relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { EntityLink } from "../drawer/parts";
import { EmptyState, Pill } from "../ui";

const BUCKETS = [
  { id: "proposed", label: "suggested", hint: "The model's pick, waiting for you in the entity's drawer" },
  { id: "confirmed", label: "confirmed", hint: "Confirmed or set by you" },
  { id: "none", label: "no article", hint: "Looked up; nothing fit. Checked again after 90 days" },
  { id: "waiting", label: "waiting", hint: "Not looked up yet; the hourly entity-linking schedule gets to them, most-mentioned first" },
] as const;

export default function References({ bucket }: { bucket?: string }) {
  const b = BUCKETS.some((x) => x.id === bucket) ? bucket! : "proposed";
  const q = useWikiCoverage(b);
  const d = q.data;
  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <span className="wk-dim">
          {d ? `${d.public_entities.toLocaleString()} entities named in claims` : "Loading…"}
          {d && d.linked_on_demand > 0 && ` · ${d.linked_on_demand} more linked on demand`}
        </span>
        {BUCKETS.map((x) => (
          <a key={x.id} className={x.id === b ? "btn" : "btn-ghost"} title={x.hint} href={hrefTo("knowledge", { view: "references", bucket: x.id })}>
            {d ? d.buckets[x.id].toLocaleString() : "…"} {x.label}
          </a>
        ))}
      </div>
      {d && !d.contact_set && (
        <EmptyState tone="yellow" title="Lookups are off" checked="WIKIMEDIA_CONTACT is not set. Wikimedia asks automated clients for a contact, so the brain looks nothing up until it is." />
      )}
      {q.error && <EmptyState tone="red" title="Couldn't load reference coverage" checked={q.error.message} />}
      <div className="wk-hint">{BUCKETS.find((x) => x.id === b)!.hint}. Most-mentioned first; open one to see or decide its link.</div>
      {d?.entities && d.entities.length === 0 && <EmptyState title={`No entities are ${BUCKETS.find((x) => x.id === b)!.label}`} checked={`Counts: ${BUCKETS.map((x) => `${d.buckets[x.id]} ${x.label}`).join(", ")}.`} />}
      {d?.entities?.map((e) => (
        <div key={e.id} className="wk-row">
          <span className="wk-row-main">
            <EntityLink kind="entity" id={e.id}><strong>{e.name}</strong></EntityLink>{" "}
            <Pill tone="orange">{e.entity_type}</Pill>
            {e.label && <span className="wk-dim"> → {e.label}{e.description ? ` — ${e.description}` : ""}</span>}
          </span>
          <span className="wk-row-meta wk-dim">
            {e.claims} {e.claims === 1 ? "claim" : "claims"}{e.checked_at ? ` · checked ${relTime(e.checked_at)}` : ""}
          </span>
        </div>
      ))}
      {d?.entities && d.entities.length >= 100 && <div className="wk-dim">The 100 most-mentioned shown.</div>}
    </div>
  );
}
