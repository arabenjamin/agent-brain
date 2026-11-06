import type { Tone } from "../components/ui";
import type { ClaimStatus, TodoStatus } from "./lists.generated";

/** A todo's status; every one is named (the brain's `todo-statuses` list). */
const TODO_TONES = {
  pending: "accent",
  in_progress: "yellow",
  awaiting_approval: "muted",
  awaiting_review: "muted",
  done: "green",
} satisfies Record<TodoStatus, Tone>;

/** A claim's standing; every one is named (the brain's `claim-statuses` list). */
const CLAIM_TONES = {
  unverified: "muted",
  corroborated: "green",
  disputed: "purple",
  refuted: "red",
  // A contradiction waiting on the owner's review: not yet a refutation.
  contested: "yellow",
} satisfies Record<ClaimStatus, Tone>;

/**
 * One colour per state across tasks, jobs, todos and runs. A job that will
 * never run again is failed, dead or dead_letter; all three read as red.
 */
const TONES: Record<string, Tone> = {
  ...TODO_TONES,
  ...CLAIM_TONES,
  completed: "green",
  running: "yellow",
  queued: "accent",
  created: "accent",
  parked: "muted",
  cancelled: "muted",
  blocked: "purple",
  failed: "red",
  dead: "red",
  dead_letter: "red",
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

/** "every 5 min", "every 2 h", "every 7 d"; "by name" for a workflow with no schedule. */
export function fmtEvery(secs: number | null | undefined): string {
  if (secs == null) return "by name";
  if (secs < 60) return `every ${secs} s`;
  if (secs < 3600) return `every ${Math.round(secs / 60)} min`;
  if (secs < 86400) return `every ${+(secs / 3600).toFixed(1)} h`;
  return `every ${+(secs / 86400).toFixed(1)} d`;
}
