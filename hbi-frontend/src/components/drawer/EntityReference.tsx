/**
 * What an entity is, from Wikipedia (HBI plan Phase 18): the linked Wikidata
 * item with the article's first paragraph, so an unfamiliar name can be
 * decided without leaving the drawer. A link the model suggested waits for the
 * user: confirm it, rule it out, choose another candidate, or paste a URL.
 */
import { useState } from "react";
import { decideReference, lookUp, setReferenceUrl, useEntityReference, type WikiItem } from "../../api/wiki";
import { formatLocal } from "../../lib/time";
import { Pill } from "../ui";
import { Section } from "./parts";

function ItemLine({ it }: { it: WikiItem }) {
  return (
    <>
      <strong>{it.label}</strong>
      {it.description && <span className="dw-dim"> — {it.description}</span>}
      {it.kinds.length > 0 && <span className="dw-dim"> [{it.kinds.join(", ")}]</span>}
    </>
  );
}

function Links({ it }: { it: WikiItem }) {
  return (
    <span className="dw-dim">
      {it.wiki_url ? <a href={it.wiki_url} target="_blank" rel="noreferrer">Wikipedia ↗</a> : "no English Wikipedia article"}
      {" · "}
      <a href={it.wikidata_url} target="_blank" rel="noreferrer">Wikidata {it.qid} ↗</a>
    </span>
  );
}

export default function EntityReference({ id, name }: { id: string; name: string }) {
  const q = useEntityReference(id);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [url, setUrl] = useState("");
  const [showCands, setShowCands] = useState(false);
  const act = (f: () => Promise<unknown>) => async () => {
    setBusy(true);
    setError(null);
    try {
      await f();
      setUrl("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const r = q.data;
  if (q.error) {
    return <Section title="Reference"><div className="tone-text-red">Couldn't load the reference link: {q.error.message}</div></Section>;
  }
  if (!r) return null;
  const link = r.link;
  const status = link?.status === "confirmed"
    ? <Pill tone="green">confirmed{link.method === "manual" ? " by you" : ""}</Pill>
    : link ? <Pill tone="yellow" title={link.model ? `Suggested by ${link.model}` : undefined}>suggested</Pill> : null;
  return (
    <Section title="Reference" aside={status}>
      {r.pending && <div className="dw-note">Looking “{name}” up on Wikidata…</div>}
      {link ? (
        <div>
          <div><ItemLine it={link} /></div>
          {r.summary && <p className="dw-note" style={{ marginTop: 6 }}>{r.summary}</p>}
          {r.summary_error && <div className="dw-dim">Couldn't fetch the article's summary: {r.summary_error}</div>}
          {link.aliases.length > 0 && <div className="dw-dim">Also known as: {link.aliases.join(", ")}</div>}
          <div><Links it={link} /></div>
          {link.status === "proposed" && link.why && (
            <div className="dw-dim">
              Why the model picked it: {link.why}
              {link.query && ` (found by searching “${link.query}”)`}
            </div>
          )}
          <div className="settings-actions">
            {link.status === "proposed" && (
              <button type="button" className="btn" disabled={busy} onClick={act(() => decideReference(id, link.qid, "confirm"))}>
                Confirm
              </button>
            )}
            <button type="button" className="btn-ghost" disabled={busy} title="Never offer this item for this entity again"
              onClick={act(() => decideReference(id, link.qid, "reject"))}>Not this</button>
          </div>
        </div>
      ) : !r.pending && (
        <div className="dw-note">
          {r.result === "none" && r.checked_at
            ? `No Wikipedia article found (checked ${formatLocal(r.checked_at)}). Checked again after 90 days.`
            : r.public
              ? "Not looked up yet. Background lookups cover entities named in claims, most-mentioned first."
              : "Not looked up: only your private notes mention it, so the brain looks it up only when you ask."}
        </div>
      )}

      {r.candidates.length > 0 && (
        <div>
          <button type="button" className="btn-ghost" onClick={() => setShowCands(!showCands)}>
            {showCands ? "Hide" : "Choose another"} ({r.candidates.length} the lookup saw)
          </button>
          {showCands && (
            <ul className="dw-list">
              {r.candidates.map((c) => (
                <li key={c.qid}>
                  <span><ItemLine it={c} /> <Links it={c} /></span>
                  <button type="button" className="btn-ghost" disabled={busy} onClick={act(() => decideReference(id, c.qid, "confirm"))}>
                    This one
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      {r.rejected.length > 0 && (
        <div className="dw-dim">Ruled out by you: {r.rejected.map((x) => x.label ?? x.qid).join(", ")}</div>
      )}

      <form className="settings-actions" onSubmit={(e) => { e.preventDefault(); if (url.trim()) void act(() => setReferenceUrl(id, url.trim()))(); }}>
        <input className="ui-input" value={url} onChange={(e) => setUrl(e.target.value)}
          placeholder="Paste a Wikipedia or Wikidata URL" />
        <button type="submit" className="btn-ghost" disabled={busy || !url.trim() || !r.contact_set}>Set</button>
        <button type="button" className="btn-ghost" disabled={busy || r.pending || !r.contact_set}
          title={r.contact_set
            ? `Search Wikidata for “${name}” now${r.public ? "" : " (sends its name to Wikimedia, and its notes to the no-log cloud model)"}`
            : "WIKIMEDIA_CONTACT is not set, so no lookups run"}
          onClick={act(() => lookUp(id))}>
          {r.pending ? "Looking up…" : link || r.result ? "Look up again" : "Look up"}
        </button>
      </form>
      {!r.contact_set && <div className="dw-dim">Lookups are off: WIKIMEDIA_CONTACT is not set.</div>}
      {error && <div className="tone-text-red">{error}</div>}
    </Section>
  );
}
