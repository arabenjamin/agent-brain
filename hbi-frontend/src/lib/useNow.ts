import { useEffect, useState } from "react";

/**
 * The current time, re-read every `intervalMs`. Render must be pure, and a
 * dashboard's "due" / "overdue" labels should move on their own anyway.
 */
export function useNow(intervalMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}
