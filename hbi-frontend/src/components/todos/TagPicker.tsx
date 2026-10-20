/**
 * Tags as references (`api/refs.ts`): type to search every kind the brain
 * has, pick from the grouped list or from the suggestions the title and
 * description name. A tag that names nothing (a free word from before
 * 2026-10-20) shows greyed and can be removed, not added again.
 */
import { useEffect, useMemo, useState } from "react";
import { REF_KINDS, REF_KIND_LABELS, refKind, useRefSearch, useRefSuggestions, useResolvedRefs, type Ref } from "../../api/refs";
import { openEntity } from "../drawer/entities";

const KIND_MARK: Record<string, string> = {
  workflow: "⟳", agent: "◎", program: "▣", topic: "#", entity: "◆", tool: "⚒", skill: "⚙",
};

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

const chipStyle = (muted: boolean): React.CSSProperties => ({
  display: "inline-flex", alignItems: "center", gap: "4px", padding: "2px 8px", borderRadius: "999px",
  fontSize: "0.8rem", border: "1px solid var(--border, #45475a)",
  background: muted ? "transparent" : "var(--surface2, #313244)", opacity: muted ? 0.6 : 1,
});

/** One tag, its kind marked; opens its drawer when it has one. */
export function TagChip({ r, onRemove }: { r: Ref; onRemove?: () => void }) {
  const muted = r.exists === false;
  const title = r.legacy
    ? "An old free-text tag: it names nothing. Remove it, or keep it as it is; it can't be added again."
    : r.exists === false
      ? "Names nothing that exists any more"
      : `${r.kind}${r.detail ? `: ${r.detail}` : ""}`;
  const mark = r.kind ? KIND_MARK[r.kind] : "·";
  return (
    <span style={chipStyle(muted)} title={title}>
      <span aria-hidden>{mark}</span>
      {r.open ? (
        <button type="button" className="dw-link" onClick={() => openEntity(r.open!.kind, r.open!.id)}>{r.label}</button>
      ) : (
        <span>{r.label}{r.legacy ? " (old tag)" : ""}</span>
      )}
      {onRemove && (
        <button type="button" aria-label={`Remove ${r.label}`} onClick={onRemove}
          style={{ background: "none", border: 0, color: "inherit", cursor: "pointer", padding: 0 }}>×</button>
      )}
    </span>
  );
}

/** A todo's saved tags as chips (the drawer's view). */
export function TagChips({ tags }: { tags: string[] }) {
  const resolved = useResolvedRefs(tags);
  const byRef = new Map((resolved.data?.refs ?? []).map((r) => [r.ref, r]));
  return (
    <span style={{ display: "inline-flex", flexWrap: "wrap", gap: "4px" }}>
      {tags.map((t) => <TagChip key={t} r={byRef.get(t) ?? { ref: t, kind: refKind(t), label: t.split(":").slice(1).join(":") || t }} />)}
    </span>
  );
}

export default function TagPicker({
  tags,
  onChange,
  context,
  inputStyle,
}: {
  tags: string[];
  onChange: (tags: string[]) => void;
  /** The title and description: what suggestions are drawn from. */
  context: string;
  inputStyle: React.CSSProperties;
}) {
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const dq = useDebounced(q.trim().toLowerCase(), 200);
  const search = useRefSearch(dq, open);
  const suggest = useRefSuggestions(useDebounced(context, 600));
  const resolved = useResolvedRefs(tags);

  // Labels for chips: from resolve, then from whatever the user just picked.
  const [picked, setPicked] = useState<Map<string, Ref>>(new Map());
  const label = (t: string): Ref =>
    resolved.data?.refs.find((r) => r.ref === t) ?? picked.get(t) ?? { ref: t, kind: refKind(t), label: t.split(":").slice(1).join(":") || t };

  const hits = useMemo(() => (search.data?.refs ?? []).filter((r) => !tags.includes(r.ref)), [search.data, tags]);
  const suggestions = (suggest.data?.refs ?? []).filter((r) => !tags.includes(r.ref));

  const add = (r: Ref) => {
    if (tags.includes(r.ref)) return;
    setPicked((m) => new Map(m).set(r.ref, r));
    onChange([...tags, r.ref]);
    setQ("");
    setActive(0);
  };

  const onKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") { e.preventDefault(); setOpen(true); setActive((a) => Math.min(a + 1, hits.length - 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setActive((a) => Math.max(a - 1, 0)); }
    else if (e.key === "Enter") { if (open && hits[active]) { e.preventDefault(); add(hits[active]); } }
    else if (e.key === "Escape") { setOpen(false); }
    else if (e.key === "Backspace" && !q && tags.length) { onChange(tags.slice(0, -1)); }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: "6px", position: "relative" }}>
      {tags.length > 0 && (
        <div style={{ display: "flex", flexWrap: "wrap", gap: "4px" }}>
          {tags.map((t) => <TagChip key={t} r={label(t)} onRemove={() => onChange(tags.filter((x) => x !== t))} />)}
        </div>
      )}
      <input
        style={inputStyle}
        value={q}
        placeholder="Search workflows, agents, programs, topics, entities, tools…"
        onChange={(e) => { setQ(e.target.value); setOpen(true); setActive(0); }}
        onFocus={() => setOpen(true)}
        onBlur={() => setTimeout(() => setOpen(false), 150)}
        onKeyDown={onKey}
        role="combobox"
        aria-expanded={open}
        aria-autocomplete="list"
      />
      {open && (hits.length > 0 || search.data?.note) && (
        <div role="listbox" style={{
          position: "absolute", top: "100%", left: 0, right: 0, zIndex: 60, maxHeight: "260px", overflowY: "auto",
          background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #45475a)", borderRadius: "6px", padding: "4px",
        }}>
          {REF_KINDS.map((k) => {
            const group = hits.filter((r) => r.kind === k);
            if (!group.length) return null;
            return (
              <div key={k}>
                <div className="dw-dim" style={{ fontSize: "0.75rem", padding: "4px 6px 2px" }}>{REF_KIND_LABELS[k]}</div>
                {group.map((r) => {
                  const i = hits.indexOf(r);
                  return (
                    <div key={r.ref} role="option" aria-selected={i === active}
                      onMouseDown={(e) => { e.preventDefault(); add(r); }}
                      onMouseEnter={() => setActive(i)}
                      style={{ padding: "4px 8px", borderRadius: "4px", cursor: "pointer", background: i === active ? "var(--surface2, #313244)" : undefined }}>
                      <span aria-hidden>{KIND_MARK[k]} </span>{r.label}
                      {r.label !== r.name && <span className="dw-dim"> · {r.name}</span>}
                      {r.detail && <div className="dw-dim" style={{ fontSize: "0.75rem" }}>{r.detail}</div>}
                    </div>
                  );
                })}
              </div>
            );
          })}
          {search.data?.note && dq.length < 2 && <div className="dw-dim" style={{ fontSize: "0.75rem", padding: "4px 6px" }}>{search.data.note}</div>}
        </div>
      )}
      {suggestions.length > 0 && (
        <div style={{ display: "flex", flexWrap: "wrap", gap: "4px", alignItems: "center", fontSize: "0.8rem" }}>
          <span className="dw-dim">Suggested:</span>
          {suggestions.map((r) => (
            <button key={r.ref} type="button" onClick={() => add(r)} title={`${r.kind}${r.detail ? `: ${r.detail}` : ""}`}
              style={{ ...chipStyle(false), cursor: "pointer", color: "inherit", borderStyle: "dashed", background: "transparent" }}>
              + {r.kind ? KIND_MARK[r.kind] : ""} {r.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
