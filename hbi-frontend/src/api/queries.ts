/**
 * Shared server state.
 *
 * Status endpoints that several views show (queue, scheduler, model catalog)
 * are fetched through one TanStack Query key each, so they're requested once
 * however many views are mounted. Every observer of a key restarts its poll
 * timer whenever the key's data updates (query-core `onQueryUpdate` →
 * `#updateTimers`), so after each fetch the observers realign and the
 * shortest requested interval wins: one request per tick, not one per view.
 */
import { QueryClient, useQuery } from "@tanstack/react-query";
import { apiJson } from "./client";
import { onNotification } from "./mcp";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // A failed poll is retried by the next tick; don't triple every miss.
      retry: 1,
      refetchOnWindowFocus: false,
      staleTime: 2_000,
    },
  },
});

export const keys = {
  queueStatus: ["queue", "status"] as const,
  schedulerStatus: ["scheduler", "status"] as const,
  note: (id: string) => ["note", id] as const,
  noteRelated: (id: string) => ["note", id, "related"] as const,
};

// ── Types (as served; optional fields are not always present) ────────────────

export interface QueueStatus {
  enabled: boolean;
  in_memory_pending: number;
  running_now: number;
  max_concurrent: number;
  by_status: Record<string, number>;
  per_provider?: Record<string, { running: number; max: number }>;
  coordinator?: { alive: boolean; last_heartbeat: string | null };
  deferred_for_gpu_lease?: number;
  poll_interval_secs?: number;
}

export interface SchedulerStatus {
  config: {
    enabled: boolean;
    interval_secs: number;
    sleep_interval_secs: number;
    idle_sleep_after_ticks: number;
    max_tasks_per_run: number;
    error_budget: number;
    local_model?: string;
    session_id: string | null;
  };
  state: {
    is_running: boolean;
    is_sleeping: boolean;
    idle_ticks: number;
    tasks_dispatched: number;
    consecutive_errors: number;
    last_run_at: string | null;
    last_activity_at: string | null;
    last_error: string | null;
  };
}

export interface Note {
  id: string;
  content: string;
  note_type?: string | null;
  access_count?: number;
  created_at?: string;
}

export interface RelatedNote {
  id: string;
  content: string;
  note_type?: string | null;
  similarity: number;
}

// ── Hooks ────────────────────────────────────────────────────────────────────

export function useQueueStatus(pollMs = 5_000) {
  return useQuery({
    queryKey: keys.queueStatus,
    queryFn: () => apiJson<QueueStatus>("/api/queue/status"),
    refetchInterval: pollMs,
  });
}

export function useSchedulerStatus(pollMs = 20_000) {
  return useQuery({
    queryKey: keys.schedulerStatus,
    queryFn: () => apiJson<SchedulerStatus>("/api/scheduler/status"),
    refetchInterval: pollMs,
  });
}

export function useNote(id: string | undefined) {
  return useQuery({
    queryKey: keys.note(id ?? ""),
    queryFn: () => apiJson<Note>(`/api/notes/${encodeURIComponent(id ?? "")}`),
    enabled: !!id,
    // A 404 is an answer, not a transient failure.
    retry: false,
  });
}

export function useRelatedNotes(id: string | undefined) {
  return useQuery({
    queryKey: keys.noteRelated(id ?? ""),
    queryFn: () =>
      apiJson<{ count: number; related_notes: RelatedNote[] }>(
        `/api/notes/${encodeURIComponent(id ?? "")}/related`,
      ),
    enabled: !!id,
    retry: false,
  });
}

/**
 * A finished, failed or dead job changes queue counts and may end a scheduler
 * tick. One subscription for the whole app replaces one per status card.
 * Returns the unsubscribe function.
 */
export function invalidateOnJobPush(): () => void {
  return onNotification((n) => {
    if (n.method === "notifications/agent_job") {
      queryClient.invalidateQueries({ queryKey: keys.queueStatus });
      queryClient.invalidateQueries({ queryKey: keys.schedulerStatus });
    }
  });
}
