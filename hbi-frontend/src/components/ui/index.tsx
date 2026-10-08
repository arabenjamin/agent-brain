/**
 * The shared component kit. Styling lives in `styles/ui.css` and reads the
 * tokens in `styles/main.css` `:root` — no inline palettes.
 */
import { useEffect, type ReactNode } from "react";

export type Tone = "accent" | "green" | "red" | "yellow" | "purple" | "cyan" | "orange" | "muted";

// ── Pill ─────────────────────────────────────────────────────────────────────

export function Pill({ children, tone = "muted", title }: { children: ReactNode; tone?: Tone; title?: string }) {
  return <span className={`ui-pill tone-${tone}`} title={title}>{children}</span>;
}

// ── Card ─────────────────────────────────────────────────────────────────────

export function Card({
  title,
  actions,
  children,
}: {
  title?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="ui-card">
      {(title || actions) && (
        <header className="ui-card-header">
          <span className="ui-card-title">{title}</span>
          {actions && <span className="ui-card-actions">{actions}</span>}
        </header>
      )}
      <div className="ui-card-body">{children}</div>
    </section>
  );
}

// ── Stat ─────────────────────────────────────────────────────────────────────

/**
 * A labelled number. Give it `href` (or `onClick`) whenever the number has
 * rows behind it: a count the user can't drill into is one they can't check.
 */
export function Stat({
  label,
  value,
  tone,
  href,
  onClick,
  title,
}: {
  label: string;
  value: ReactNode;
  tone?: Tone;
  href?: string;
  onClick?: () => void;
  title?: string;
}) {
  const body = (
    <>
      <span className="ui-stat-label">{label}</span>
      <span className={`ui-stat-value${tone ? ` tone-text-${tone}` : ""}`}>{value}</span>
    </>
  );
  if (href) return <a className="ui-stat ui-stat-link" href={href} title={title}>{body}</a>;
  return onClick ? (
    <button type="button" className="ui-stat ui-stat-link" onClick={onClick} title={title}>{body}</button>
  ) : (
    <div className="ui-stat" title={title}>{body}</div>
  );
}

// ── Tabs ─────────────────────────────────────────────────────────────────────

export function Tabs<T extends string>({
  tabs,
  active,
  onChange,
}: {
  tabs: { id: T; label: ReactNode }[];
  active: T;
  onChange: (id: T) => void;
}) {
  return (
    <div className="ui-tabs" role="tablist">
      {tabs.map((t) => (
        <button
          key={t.id}
          type="button"
          role="tab"
          aria-selected={t.id === active}
          className={`ui-tab${t.id === active ? " active" : ""}`}
          onClick={() => onChange(t.id)}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}

// ── EmptyState ───────────────────────────────────────────────────────────────

/**
 * Nothing to show. `checked` says what was looked at, so an empty view is
 * never mistaken for a broken one ("0 notes match 'x' across 23,078").
 */
export function EmptyState({
  title,
  checked,
  action,
  tone = "muted",
}: {
  title: ReactNode;
  checked?: ReactNode;
  action?: ReactNode;
  tone?: Tone;
}) {
  return (
    <div className={`ui-empty tone-border-${tone}`}>
      <div className="ui-empty-title">{title}</div>
      {checked && <div className="ui-empty-checked">{checked}</div>}
      {action && <div className="ui-empty-action">{action}</div>}
    </div>
  );
}

// ── Drawer ───────────────────────────────────────────────────────────────────

/** Right-hand inspection panel. Escape closes it. */
export function Drawer({
  title,
  actions,
  onClose,
  wide = false,
  children,
}: {
  title: ReactNode;
  actions?: ReactNode;
  onClose: () => void;
  wide?: boolean;
  children: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <aside className={`ui-drawer${wide ? " wide" : ""}`} aria-label="Details">
      <header className="ui-drawer-header">
        <span className="ui-drawer-title">{title}</span>
        <span className="ui-drawer-actions">
          {actions}
          <button type="button" className="ui-icon-btn" onClick={onClose} title="Close (Esc)" aria-label="Close">×</button>
        </span>
      </header>
      <div className="ui-drawer-body">{children}</div>
    </aside>
  );
}

// ── DrillBanner ──────────────────────────────────────────────────────────────

/**
 * Marks a view that is showing a filtered subset (a drill-down from a count),
 * with a way back to the whole list. `capped` says when the rows hit the
 * endpoint's limit, so a short list is never mistaken for the full answer.
 */
export function DrillBanner({
  children,
  rows,
  cap,
  onClear,
}: {
  children: ReactNode;
  rows?: number;
  cap?: number;
  onClear: () => void;
}) {
  return (
    <div className="drill-banner" role="status">
      <span>Filtered: {children}</span>
      {rows !== undefined && (
        <span className={cap !== undefined && rows >= cap ? "tone-text-yellow" : "tone-text-muted"}>
          {cap !== undefined && rows >= cap ? `first ${cap} rows (the endpoint's limit)` : `${rows} rows`}
        </span>
      )}
      <button type="button" className="ui-icon-btn" onClick={onClear}>Show all ✕</button>
    </div>
  );
}
