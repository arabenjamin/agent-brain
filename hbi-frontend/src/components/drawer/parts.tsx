/**
 * Building blocks shared by the drawer's views: a status pill, a titled
 * section, a link that opens another entity in the drawer, and the
 * loading / not-found / error states.
 */
import type { ReactNode } from "react";
import { ApiError } from "../../api/client";
import { statusTone } from "../../lib/status";
import { EmptyState, Pill } from "../ui";
import { openEntity, type DrawerKind } from "./entities";

export function StatusPill({ status }: { status: string | null | undefined }) {
  return <Pill tone={statusTone(status)}>{(status ?? "unknown").replaceAll("_", " ")}</Pill>;
}

export function Section({ title, aside, children }: { title: ReactNode; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="dw-section">
      <header className="dw-section-head">
        <span>{title}</span>
        {aside && <span className="dw-section-aside">{aside}</span>}
      </header>
      {children}
    </section>
  );
}

/** A button that opens another entity in the drawer (the URL changes, so Back returns). */
export function EntityLink({ kind, id, children, title }: { kind: DrawerKind; id: string; children: ReactNode; title?: string }) {
  return (
    <button type="button" className="dw-link" onClick={() => openEntity(kind, id)} title={title ?? `Open ${kind}`}>
      {children}
    </button>
  );
}

/** Loading, or the reason there is nothing to show. */
export function LoadState({
  what,
  id,
  error,
  onRetry,
}: {
  what: string;
  id: string;
  error: Error | null;
  onRetry?: () => void;
}) {
  if (!error) return <div className="loading">Loading {what}…</div>;
  if (error instanceof ApiError && error.status === 404) {
    return (
      <EmptyState
        tone="yellow"
        title={`No ${what} with this id`}
        checked={<>Looked up <code>{id}</code>. It may have been deleted, or (for a job) pruned after its retention window.</>}
      />
    );
  }
  return (
    <EmptyState
      tone="red"
      title={`Couldn't load the ${what}`}
      checked={error.message}
      action={onRetry && <button type="button" className="btn" onClick={onRetry}>Retry</button>}
    />
  );
}

/** A long tool result, marked when the server cut it. */
export function ResultText({ text, truncated, total }: { text: string; truncated: boolean; total: number }) {
  return (
    <>
      <pre className="dw-pre">{text}</pre>
      {truncated && (
        <div className="dw-note tone-text-yellow">
          Showing {text.length.toLocaleString()} of {total.toLocaleString()} characters.
        </div>
      )}
    </>
  );
}
