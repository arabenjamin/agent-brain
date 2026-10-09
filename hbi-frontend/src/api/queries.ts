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
import type { CatalogModel, ProviderInfo } from "../lib/models";
import { QueryClient, useQuery } from "@tanstack/react-query";
import { apiJson, query } from "./client";
import { callTool } from "./mcp";
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
  activity: (hours: number) => ["activity", hours] as const,
  schedules: ["schedules"] as const,
  todos: ["todos"] as const,
  proposals: ["proposals"] as const,
  modelUsage: (hours: number) => ["models", "usage", hours] as const,
  activeModels: ["models", "active"] as const,
  llmBudget: ["llm", "budget"] as const,
  profiles: ["profiles"] as const,
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
  last_accessed_at?: string | null;
  /** Which process wrote it (`daily_news`, `chat_session`, …). */
  source_context?: string | null;
  provenance?: string | null;
  claim_status?: string | null;
  claim_kind?: string | null;
  corroboration_tier?: string | null;
  asserted_by?: string | null;
  verified_at?: string | null;
  verification?: string | null;
  verification_reason?: string | null;
  corroborating_count?: number | null;
  contradicting_count?: number | null;
  /** Has its own vector (long notes are embedded through their chunks). */
  embedded?: boolean;
  /** This note is a chunk of that one. */
  parent_id?: string | null;
  /** How many chunks this note was split into. */
  chunks?: number;
  entities?: { id: string; name: string | null; entity_type: string | null }[];
  /** Topics, main first. */
  topics?: string[];
  /** False while the note waits to be classified for the current topics. */
  topics_assessed?: boolean;
  /** The user set its topics; the classifier leaves it alone. */
  topics_locked?: boolean;
  /** A claim's source notes, with the article or video each records. */
  asserted_in?: SourceRef[];
  /** A claim that could not be made to stand alone, and why. */
  ambiguous?: boolean;
  ambiguous_reason?: string | null;
  /** A rewritten claim's original text, and when it was rewritten. */
  content_v1?: string | null;
  rewritten_at?: string | null;
  /** The named things a claim is about. */
  about?: string[];
}

export interface SourceRef {
  id: string;
  note_type?: string | null;
  source_context?: string | null;
  created_at?: string | null;
  title?: string | null;
  url?: string | null;
  /** The site or channel. */
  outlet?: string | null;
}

/** The models the brain is using now: `active_*` serves background cloud work, `chat_*` the chat panel. */
export interface ActiveModels {
  active_provider?: string;
  active_model?: string;
  chat_provider?: string;
  chat_model?: string;
  /** Every catalog model, each with the routing policy's verdict per purpose and data class. */
  catalog_models?: CatalogModel[];
  /** The provider registry: billing, data policy, whether a key is set. */
  providers?: ProviderInfo[];
}

export interface RelatedNote {
  id: string;
  content: string;
  note_type?: string | null;
  similarity: number;
}

/** Counts plus the breakdown behind them; `null` when that section's query failed. */
export interface Breakdown {
  total: number;
  by: Record<string, number>;
}

export interface Activity {
  window_hours: number;
  /** Start of the window, UTC RFC 3339 — pass it as `since` to list the rows. */
  since: string;
  notes: Breakdown | null;
  tasks: Breakdown | null;
  /** Jobs whose state changed in the window. */
  jobs: Breakdown | null;
  /** Claims whose evidence was gathered in the window, by resulting status. */
  claims_checked: Breakdown | null;
  media_ingested: number | null;
  schedule_runs: number | null;
  /** Sections that failed, by name. Never shown as zero. */
  errors: string[];
}

export interface ScheduledTask {
  id: string;
  name: string;
  description?: string;
  enabled: boolean;
  interval_seconds: number;
  last_run_at: string | null;
  next_run_at: string | null;
  managed_by?: string;
  paused_reason?: string | null;
}

export interface Todo {
  id: string;
  title: string;
  description?: string;
  status: string;
  priority?: number;
  due_at?: string | null;
  tags?: string[];
}

export interface Proposal {
  filename: string;
  title: string;
  severity: "low" | "medium" | "high" | string;
  task_id: string;
  created: string;
  applied: boolean;
}

export interface ModelUsage {
  model: string;
  total_calls: number;
  successes: number;
  failures: number;
  rate_limited: number;
  total_tokens_in: number;
  total_tokens_out: number;
  total_cost: number;
}

export interface BudgetProvider {
  name: string;
  kind: string;
  billing: "local" | "free" | "paid" | string;
  data_policy: string;
  enabled: boolean;
  key_configured: boolean;
  api_key_env: string | null;
  private_ok_for: string[];
  limits: Record<string, number>;
  spend: { daily_usd?: number; monthly_usd?: number } | null;
  month_spend_usd?: number;
  today?: { requests: number; spend_usd: number; tokens: number };
  /** Per-model counters and cooldowns (a model can be benched alone). */
  models?: {
    model: string;
    cooling_down: boolean;
    cooldown_until: string | null;
    cooldown_reason: string | null;
    today?: { requests: number; tokens: number };
  }[];
  terms?: string | null;
  [k: string]: unknown;
}

export interface LlmBudget {
  generated_at: string;
  day_resets_at: string;
  gpu_lease: { held: boolean; [k: string]: unknown };
  providers: BudgetProvider[];
}

export interface ProfileInfo {
  name: string;
  description?: string;
  tools?: string[];
  /** A model the profile pins, if any (`/api/contexts`). */
  model_preference?: string;
  /** Whether `run_agent` / `delegate` may run it. */
  spawnable?: boolean;
  /** `public` lets its work reach providers that may train on prompts; absent = private. */
  data_class?: string | null;
  /** Why an item assigned to it waits for approval; null = it runs on its own. */
  asks_first?: string | null;
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

export function useActivity(hours = 24, pollMs = 60_000) {
  return useQuery({
    queryKey: keys.activity(hours),
    queryFn: () => apiJson<Activity>(`/api/activity${query({ hours })}`),
    refetchInterval: pollMs,
  });
}

export function useSchedules(pollMs = 60_000) {
  return useQuery({
    queryKey: keys.schedules,
    queryFn: async () =>
      (await apiJson<{ scheduled_tasks?: ScheduledTask[] }>("/scheduled-tasks")).scheduled_tasks ?? [],
    refetchInterval: pollMs,
  });
}

export function useTodos(pollMs = 60_000) {
  return useQuery({
    queryKey: keys.todos,
    queryFn: async () => (await apiJson<{ todos?: Todo[] }>("/todos")).todos ?? [],
    refetchInterval: pollMs,
  });
}

/** Code proposals awaiting review (`list_proposals`; applied ones excluded). */
export function useProposals(pollMs = 120_000) {
  return useQuery({
    queryKey: keys.proposals,
    queryFn: async () => {
      const raw = await callTool("list_proposals", { include_applied: false });
      return ((JSON.parse(raw) as { proposals?: Proposal[] }).proposals ?? []);
    },
    refetchInterval: pollMs,
  });
}

export function useModelUsage(hours = 24, pollMs = 60_000) {
  return useQuery({
    queryKey: keys.modelUsage(hours),
    queryFn: async () =>
      (await apiJson<{ models?: ModelUsage[] }>(`/api/models/usage${query({ hours })}`)).models ?? [],
    refetchInterval: pollMs,
  });
}

export function useLlmBudget(pollMs = 60_000) {
  return useQuery({
    queryKey: keys.llmBudget,
    queryFn: () => apiJson<LlmBudget>("/api/llm/budget"),
    refetchInterval: pollMs,
  });
}

/** Context profiles (agents). Chat's picker and Settings → Agents share it. */
interface ProfileList {
  profiles: ProfileInfo[];
  /** The front door a chat gets when it names none (CHAT_DEFAULT_PROFILE); null = picked per message. */
  defaultProfile: string | null;
}

function useProfileList<T>(select: (d: ProfileList) => T) {
  return useQuery({
    queryKey: keys.profiles,
    queryFn: async (): Promise<ProfileList> => {
      const r = await apiJson<{ profiles?: ProfileInfo[]; default_profile?: string }>("/api/contexts");
      return {
        profiles: [...(r.profiles ?? [])].sort((a, b) => a.name.localeCompare(b.name)),
        defaultProfile: r.default_profile ?? null,
      };
    },
    select,
  });
}

export function useProfiles() {
  return useProfileList((d) => d.profiles);
}

/** The deployment's front door: what a chat that names no profile runs under. */
export function useDefaultProfile() {
  return useProfileList((d) => d.defaultProfile);
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

export function useActiveModels() {
  return useQuery({
    queryKey: keys.activeModels,
    queryFn: () => apiJson<ActiveModels>("/api/models"),
    staleTime: 60_000,
  });
}
