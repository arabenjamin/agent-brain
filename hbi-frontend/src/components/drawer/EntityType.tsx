/**
 * What an entity is (HBI plan Phase 19): its coarse type, the fine kind its
 * reference link gives (country, municipality…), and the question the brain
 * asks when the reference disagrees with the filed type. Setting a type here
 * is final: mentions and links never change it until "Let the brain decide".
 */
import { useState } from "react";
import type { EntityDetail } from "../../api/search";
import { answerTypeQuestion, setEntityType, unlockEntityType, useEntityTypes } from "../../api/types";
import { Pill } from "../ui";
import { Section } from "./parts";

export default function EntityType({ e }: { e: EntityDetail["entity"] }) {
  const vocab = useEntityTypes(1);
  const [editing, setEditing] = useState(false);
  const [type, setType] = useState(e.entity_type ?? "");
  const [kind, setKind] = useState(e.kind_source === "manual" ? e.kind ?? "" : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const act = (f: () => Promise<unknown>) => async () => {
    setBusy(true);
    setError(null);
    try {
      await f();
      setEditing(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  const q = e.type_question;
  const source = e.type_source === "manual"
    ? <Pill tone="green">set by you</Pill>
    : e.type_source === "seed" ? <Pill>curated seed</Pill> : <Pill>from its mentions</Pill>;
  const kinds = (vocab.data?.kinds ?? []).filter((k) => !type || k.entity_type === type);
  return (
    <Section title="Type" aside={source}>
      <div>
        <strong>{e.entity_type ?? "unknown"}</strong>
        {e.kind && <> · <strong>{e.kind}</strong></>}
        {e.kind && (
          <span className="dw-dim">
            {e.kind_source === "manual" ? " (kind set by you)" : " (kind from its reference)"}
          </span>
        )}
      </div>

      {q && (
        <div className="dw-note">
          Its reference, <strong>{q.label ?? q.qid}</strong>{q.description && <> — {q.description}</>}, says it is
          {" "}a <strong>{q.kind}</strong> ({q.kind_type}), but it is filed as <strong>{e.entity_type}</strong>.
          <div className="settings-actions">
            <button type="button" className="btn" disabled={busy} onClick={act(() => answerTypeQuestion(e.id, "reference"))}>
              Use {q.kind_type} · {q.kind}
            </button>
            <button type="button" className="btn-ghost" disabled={busy}
              title="Keep the filed type; this reference won't ask again"
              onClick={act(() => answerTypeQuestion(e.id, "keep"))}>
              Keep {e.entity_type}
            </button>
          </div>
          <div className="dw-dim">If the reference itself is wrong, use “Not this” under Reference instead.</div>
        </div>
      )}

      {editing ? (
        <form className="settings-actions" onSubmit={(ev) => { ev.preventDefault(); if (type) void act(() => setEntityType(e.id, type, kind))(); }}>
          <select className="ui-input" value={type} onChange={(ev) => setType(ev.target.value)} aria-label="Type">
            {!vocab.data?.entity_types.includes(type) && <option value={type}>{type || "choose a type"}</option>}
            {(vocab.data?.entity_types ?? []).map((t) => <option key={t} value={t}>{t}</option>)}
          </select>
          <input className="ui-input" list={`kinds-${e.id}`} value={kind} onChange={(ev) => setKind(ev.target.value)}
            placeholder="kind (optional), e.g. municipality" aria-label="Kind" />
          <datalist id={`kinds-${e.id}`}>
            {kinds.map((k) => <option key={k.name} value={k.name} />)}
          </datalist>
          <button type="submit" className="btn" disabled={busy || !type}>Save</button>
          <button type="button" className="btn-ghost" onClick={() => setEditing(false)}>Cancel</button>
        </form>
      ) : (
        <div className="settings-actions">
          {e.type_source !== "seed" && (
            <button type="button" className="btn-ghost" onClick={() => setEditing(true)}>Change type…</button>
          )}
          {e.type_source === "manual" && (
            <button type="button" className="btn-ghost" disabled={busy}
              title="Drop your type and kind: the type goes back to the vote of its mentions, the kind to its reference"
              onClick={act(() => unlockEntityType(e.id))}>Let the brain decide</button>
          )}
        </div>
      )}
      {error && <div className="tone-text-red">{error}</div>}
    </Section>
  );
}
