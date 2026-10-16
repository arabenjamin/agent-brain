/**
 * Todos: shape, ordering and the overdue rule shared by the Work lanes,
 * the drawer and Home. Split out of the former TodoPanel (HBI Phase 2).
 */

export interface Todo {
  id: string;
  title: string;
  description?: string | null;
  status: "pending" | "in_progress" | "awaiting_approval" | "awaiting_review" | "done";
  priority: 0 | 1 | 2 | 3;
  tags: string[];
  due_at?: string | null;
  created_at: string;
  updated_at: string;
  /** `user`, `agent:<profile>`, `agent:auto`; absent = the user's own. */
  assignee?: string | null;
  /** Who asked: the user, a schedule, a code proposal, or the brain (`notify_user` needing action). */
  origin?: { kind: "user" | "schedule" | "proposal" | "agent" | string; ref?: string | null } | null;
  prompt?: string | null;
  /** The item's chat thread. */
  session_id?: string | null;
  snoozed_until?: string | null;
  dispatch_count?: number;
  result_note_id?: string | null;
  /** The model its agent runs on; absent = the agent's own. */
  model?: string | null;
  /** The user's comment when they sent a result back. */
  review_comment?: string | null;
  /** Open and the user's to act on now. */
  for_user?: boolean;
  /** What the user must do or decide, and what waits on it. */
  ask?: string | null;
  /** The Task whose run made the item. */
  source_task?: string | null;
  /** Who made it, in words; absent = the user, in the HBI. */
  source?: string | null;
}

/** Who an item is for, in words. */
export function assigneeLabel(a?: string | null): string {
  if (!a || a === "user") return "you";
  if (a === "agent:auto") return "an agent (auto)";
  return a.startsWith("agent:") ? a.slice(6) : a;
}

/** Where an item came from, in words. */
export const ORIGIN_LABELS: Record<string, string> = {
  user: "you",
  agent: "the brain asked",
  proposal: "code proposal",
  schedule: "a schedule",
  deep_research: "deep research",
};

export type SortBy = "priority" | "due" | "created" | "updated";

export const SORT_LABELS: Record<SortBy, string> = {
  priority: "Priority",
  due: "Due date",
  created: "Newest",
  updated: "Recently updated",
};

export const PRIORITY_LABELS: Record<number, string> = { 0: "Urgent", 1: "High", 2: "Normal", 3: "Low" };

export const STATUS_LABELS: Record<string, string> = {
  pending: "Pending",
  in_progress: "In progress",
  awaiting_approval: "Awaiting approval",
  awaiting_review: "Awaiting review",
  done: "Done",
};

export function todosUrl(path = ""): string {
  return `/todos${path}`;
}

/** The local calendar date as YYYY-MM-DD. */
export function localDate(now: number): string {
  const d = new Date(now);
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}

/**
 * A due date is a calendar date (the form sends YYYY-MM-DD; it is stored as
 * that date at 00:00 UTC). A todo is overdue once that date is before
 * today's *local* date — comparing instants instead marked a todo due today
 * overdue from the evening before in any timezone west of UTC.
 */
export function isTodoOverdue(todo: { due_at?: string | null; status: string }, now: number): boolean {
  if (!todo.due_at || todo.status === "done") return false;
  return todo.due_at.slice(0, 10) < localDate(now);
}

export function compareTodos(a: Todo, b: Todo, sortBy: SortBy): number {
  switch (sortBy) {
    case "priority":
      // Lower number = more urgent; tie-break on due date (missing dates last).
      if (a.priority !== b.priority) return a.priority - b.priority;
      return (a.due_at ?? "9999") < (b.due_at ?? "9999") ? -1 : 1;
    case "due":
      if (!a.due_at && !b.due_at) return 0;
      if (!a.due_at) return 1;
      if (!b.due_at) return -1;
      return a.due_at < b.due_at ? -1 : 1;
    case "created":
      return a.created_at > b.created_at ? -1 : 1;
    case "updated":
      return a.updated_at > b.updated_at ? -1 : 1;
  }
}
