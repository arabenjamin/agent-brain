import { useEffect, useMemo, useRef, useState } from "react";
import { paletteResults, staticCommands, type Command } from "./commands";

/** Ctrl/Cmd-K: jump to any workspace, view, setting or note. */
export default function CommandPalette({ onClose }: { onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const all = useMemo(() => staticCommands(), []);

  const results: Command[] = useMemo(
    () => paletteResults(all, query),
    [all, query],
  );

  useEffect(() => { inputRef.current?.focus(); }, []);

  const run = (c: Command | undefined) => {
    if (!c) return;
    onClose();
    c.run();
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") { e.preventDefault(); onClose(); }
    else if (e.key === "ArrowDown") { e.preventDefault(); setIndex((i) => Math.min(i + 1, results.length - 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setIndex((i) => Math.max(i - 1, 0)); }
    else if (e.key === "Enter") { e.preventDefault(); run(results[index]); }
  };

  return (
    <div className="palette-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="palette" role="dialog" aria-label="Command palette">
        <input
          ref={inputRef}
          className="ui-input"
          placeholder="Go to a view or setting, paste a note id, or search notes…"
          value={query}
          onChange={(e) => { setQuery(e.target.value); setIndex(0); }}
          onKeyDown={onKey}
          spellCheck={false}
        />
        {results.length === 0 ? (
          <div className="palette-empty">Nothing matches “{query}”.</div>
        ) : (
          <ul className="palette-list" role="listbox">
            {results.map((c, i) => (
              <li
                key={c.id}
                role="option"
                aria-selected={i === index}
                className={`palette-item${i === index ? " active" : ""}`}
                onMouseEnter={() => setIndex(i)}
                onMouseDown={(e) => { e.preventDefault(); run(c); }}
              >
                <span>{c.label}</span>
                <span className="palette-group">{c.group}</span>
              </li>
            ))}
          </ul>
        )}
        <div className="palette-hint">↑↓ to move · Enter to open · Esc to close</div>
      </div>
    </div>
  );
}
