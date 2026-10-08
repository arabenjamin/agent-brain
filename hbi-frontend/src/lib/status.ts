import type { Tone } from "../components/ui";

/**
 * One colour per state across tasks, jobs, todos and runs. A job that will
 * never run again is failed, dead or dead_letter; all three read as red.
 */
const TONES: Record<string, Tone> = {
  completed: "green",
  done: "green",
  corroborated: "green",
  running: "yellow",
  in_progress: "yellow",
  queued: "accent",
  created: "accent",
  pending: "accent",
  parked: "muted",
  cancelled: "muted",
  blocked: "purple",
  failed: "red",
  dead: "red",
  dead_letter: "red",
  refuted: "red",
};

export function statusTone(status: string | null | undefined): Tone {
  return (status && TONES[status]) || "muted";
}

/** Job states after which the job will not run again unless retried. */
export const TERMINAL_FAILED = ["failed", "dead", "dead_letter"];
/** Job states that can still run. */
export const LIVE_JOB = ["running", "queued", "parked"];

/** "1.2 s", "3 m 20 s"; "—" when absent. */
export function fmtDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)} s`;
  const m = Math.floor(s / 60);
  return `${m} m ${Math.round(s - m * 60)} s`;
}

/** "every 5 min", "every 2 h", "every 7 d". */
export function fmtEvery(secs: number): string {
  if (secs < 60) return `every ${secs} s`;
  if (secs < 3600) return `every ${Math.round(secs / 60)} min`;
  if (secs < 86400) return `every ${+(secs / 3600).toFixed(1)} h`;
  return `every ${+(secs / 86400).toFixed(1)} d`;
}
