/**
 * The Work workspace's data: lists for the lanes, lineage for the drawer,
 * and the actions (`clients/work.rs`, `/todos`, `/scheduled-tasks`,
 * `manage_job`). Every mutation invalidates the keys it changes.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import { callToolStrict } from "./mcp";
import { keys as baseKeys, queryClient } from "./queries";

export const workKeys = {
  tasks: (status: string, since: string, limit: number) => ["work", "tasks", status, since, limit] as const,
  jobs: (status: string, since: string, limit: number) => ["work", "jobs", status, since, limit] as const,
  schedules: ["work", "schedules"] as const,
  task: (id: string) => ["work", "task", id] as const,
  job: (id: string) => ["work", "job", id] as const,
  schedule: (id: string) => ["work", "schedule", id] as const,
  todo: (id: string) => ["work", "todo", id] as const,
};

/** Refresh everything Work shows after a mutation. Cheap: lists are small. */
export function invalidateWork(): Promise<void> {
  return Promise.all([
    queryClient.invalidateQueries({ queryKey: ["work"] }),
    queryClient.invalidateQueries({ queryKey: baseKeys.todos }),
    queryClient.invalidateQueries({ queryKey: baseKeys.schedules }),
    queryClient.invalidateQueries({ queryKey: baseKeys.queueStatus }),
  ]).then(() => undefined);
}

// ── Types ────────────────────────────────────────────────────────────────────

export interface TaskRow {
  id: string;
  goal: string;
  status: string;
  context?: string | null;
  parent_id?: string | null;
  created_at: string;
}

export interface JobRow {
  id: string;
  tool_name: string;
  status: string;
  priority: number;
  attempt_count: number;
  max_attempts: number;
  provider_hint?: string | null;
  args?: Record<string, unknown> | null;
  error?: string | null;
  parent_job_id?: string | null;
  created_at: string;
  updated_at: string;
}

export interface NoteRef {
  id: string;
  note_type?: string | null;
  preview: string;
  created_at?: string | null;
  via_job?: string;
  via?: string;
}

export interface TaskRef {
  id: string;
  goal?: string | null;
  status?: string | null;
  via_job?: string;
}

export interface LineageJob {
  id: string;
  tool_name: string;
  status: string;
  attempt_count: number;
  max_attempts: number;
  provider_hint?: string | null;
  parent_job_id?: string | null;
  owner_task_id?: string | null;
  error?: string | null;
  dead_letter_reason?: string | null;
  created_at?: string | null;
  started_at?: string | null;
  completed_at?: string | null;
  duration_ms?: number | null;
  args?: Record<string, unknown> | null;
  result?: string | null;
  result_truncated: boolean;
  result_chars: number;
  notes: NoteRef[];
  tasks?: TaskRef[];
}

export type Origin =
  | { kind: "schedule"; id: string; name: string | null; exists: boolean }
  | { kind: "todo"; id: string; title: string | null }
  | { kind: "task"; id: string; goal: string | null; status: string | null }
  | null;

export interface TaskLineage {
  task: {
    id: string;
    goal: string | null;
    status: string;
    context: string | null;
    success_criteria: string | null;
    created_at: string | null;
    updated_at: string | null;
  };
  origin: Origin;
  subtasks: TaskRef[];
  jobs: LineageJob[];
  /** Why there are no jobs, when there are none. */
  jobs_note: string | null;
  jobs_error: string | null;
  notes: NoteRef[];
  created_tasks: TaskRef[];
}

export interface JobDetail {
  job: LineageJob;
  owner_task: { id: string; goal: string | null; status: string | null; exists: boolean } | null;
  chain: { id: string; tool_name: string; status: string }[];
}

export interface RunRef {
  task_id: string;
  status: string | null;
  created_at: string | null;
  updated_at: string | null;
}

export interface WorkSchedule {
  id: string;
  name: string;
  description?: string | null;
  enabled: boolean;
  interval_seconds: number;
  steps: string | unknown[];
  last_run_at?: string | null;
  next_run_at: string;
  created_at: string;
  updated_at: string;
  managed_by?: string | null;
  /** Tool name per step as the runner reads it ("" for a step it cannot run). */
  step_tools: string[];
  /** Steps stored in the old `tool`/`args` shape, which run as an empty tool and fail. */
  legacy_steps: number;
  last_run?: RunRef | null;
  runs_recorded?: number;
}

export interface ScheduleDetail {
  schedule: WorkSchedule;
  runs: RunRef[];
  runs_total: number;
  runs_shown: number;
}

export interface TodoDetail {
  todo: {
    id: string;
    title: string;
    description?: string | null;
    status: "pending" | "in_progress" | "done";
    priority: 0 | 1 | 2 | 3;
    tags: string[];
    due_at?: string | null;
    created_at: string;
    updated_at: string;
  };
  tasks: TaskRef[];
}

// ── Lists ────────────────────────────────────────────────────────────────────

export function useTaskList(status = "", since = "", limit = 100, pollMs = 10_000) {
  return useQuery({
    queryKey: workKeys.tasks(status, since, limit),
    queryFn: async () =>
      (await apiJson<{ tasks?: TaskRow[] }>(`/api/tasks${query({ status, since, limit })}`)).tasks ?? [],
    refetchInterval: pollMs,
  });
}

export function useJobList(status = "", since = "", limit = 100, pollMs = 5_000) {
  return useQuery({
    queryKey: workKeys.jobs(status, since, limit),
    queryFn: async () =>
      (await apiJson<{ jobs?: JobRow[] }>(`/api/jobs${query({ status, since, limit })}`)).jobs ?? [],
    refetchInterval: pollMs,
  });
}

export function useWorkSchedules(pollMs = 30_000) {
  return useQuery({
    queryKey: workKeys.schedules,
    queryFn: () => apiJson<{ schedules: WorkSchedule[]; runs_error: string | null }>("/api/work/schedules"),
    refetchInterval: pollMs,
  });
}

// ── Detail (drawer) ──────────────────────────────────────────────────────────

const detail = { retry: false, refetchInterval: 5_000 } as const;

export function useTaskLineage(id: string) {
  return useQuery({ queryKey: workKeys.task(id), queryFn: () => apiJson<TaskLineage>(`/api/work/tasks/${encodeURIComponent(id)}`), ...detail });
}

export function useJobDetail(id: string) {
  return useQuery({ queryKey: workKeys.job(id), queryFn: () => apiJson<JobDetail>(`/api/work/jobs/${encodeURIComponent(id)}`), ...detail });
}

export function useScheduleDetail(id: string) {
  return useQuery({
    queryKey: workKeys.schedule(id),
    queryFn: () => apiJson<ScheduleDetail>(`/api/work/schedules/${encodeURIComponent(id)}`),
    retry: false,
    refetchInterval: 15_000,
    enabled: id !== "new",
  });
}

export function useTodoDetail(id: string) {
  return useQuery({
    queryKey: workKeys.todo(id),
    queryFn: () => apiJson<TodoDetail>(`/api/work/todos/${encodeURIComponent(id)}`),
    retry: false,
    enabled: id !== "new",
  });
}

// ── Actions ──────────────────────────────────────────────────────────────────

async function after<T>(p: Promise<T>): Promise<T> {
  const v = await p;
  await invalidateWork();
  return v;
}

export function cancelJob(id: string) {
  return after(callToolStrict("manage_job", { action: "cancel", job_id: id }));
}

/**
 * Re-queue a job that will not run again. `manage_job retry` takes failed,
 * dead and cancelled jobs; a dead-lettered job is retried through the
 * `dead_letter` tool instead.
 */
export function retryJob(id: string, status: string) {
  return status === "dead_letter"
    ? after(callToolStrict("dead_letter", { action: "retry", job_id: id }))
    : after(callToolStrict("manage_job", { action: "retry", job_id: id }));
}

/** Dispatch one run now; resolves to the run-record task id. */
export function runScheduleNow(id: string) {
  return after(apiSend<{ task_id: string }>("POST", `/api/work/schedules/${encodeURIComponent(id)}/run`));
}

/** Targeted: sets only `enabled`, so a YAML-owned schedule stays YAML-owned. */
export function setScheduleEnabled(id: string, enabled: boolean) {
  return after(apiSend("PUT", `/scheduled-tasks/${encodeURIComponent(id)}`, { enabled }));
}

export function deleteSchedule(id: string) {
  return after(apiJson(`/scheduled-tasks/${encodeURIComponent(id)}`, { method: "DELETE" }));
}

export function setTodoStatus(id: string, status: string) {
  return after(apiSend("PUT", `/todos/${encodeURIComponent(id)}`, { status }));
}

export function deleteTodo(id: string) {
  return after(apiJson(`/todos/${encodeURIComponent(id)}`, { method: "DELETE" }));
}

/** Hand a todo to the brain as a task; resolves to the new task id. */
export function todoToTask(id: string) {
  return after(apiSend<{ task_id: string; warning?: string }>("POST", `/api/work/todos/${encodeURIComponent(id)}/task`));
}
