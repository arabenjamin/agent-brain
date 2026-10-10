/**
 * The one-line Wikidata description beside an entity's name in review lists
 * (HBI plan Phase 18), so an unfamiliar name can be decided in place.
 * Nothing renders for an entity with no reference link.
 */
import type { WikiHint as Hint } from "../../api/wiki";

export default function WikiHint({ h }: { h: Hint | undefined }) {
  if (!h?.description) return null;
  const d = h.description.length > 80 ? `${h.description.slice(0, 80)}…` : h.description;
  return (
    <span className="wk-dim" title={`${h.label ?? ""}: ${h.description}${h.status === "proposed" ? " (suggested link, not yet confirmed)" : ""}`}>
      {" "}({d}{h.status === "proposed" ? "?" : ""})
    </span>
  );
}
