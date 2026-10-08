/**
 * The brain stores every timestamp as a UTC `DATETIME`; Neo4j's `toString`
 * renders it ISO-8601, sometimes with nanoseconds and sometimes with a
 * `[Zone/Name]` suffix that `Date` rejects. Anything shown to a person is
 * local time.
 */

/** Parse a brain timestamp; `null` when absent or unparseable. */
export function parseTime(raw: string | null | undefined): Date | null {
  if (!raw) return null;
  const cleaned = raw.trim().replace(/\[[^\]]*\]$/, "");
  const d = new Date(cleaned);
  return Number.isNaN(d.getTime()) ? null : d;
}

/** Local date + time, or "—" when there is no usable timestamp. */
export function formatLocal(raw: string | null | undefined): string {
  const d = parseTime(raw);
  return d ? d.toLocaleString() : "—";
}

/** "just now", "5m ago", "3d ago"; "never" when absent. */
export function relTime(raw: string | null | undefined, now: number = Date.now()): string {
  const d = parseTime(raw);
  if (!d) return "never";
  const secs = Math.floor((now - d.getTime()) / 1000);
  if (secs < 0) return "in the future";
  if (secs < 5) return "just now";
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}
