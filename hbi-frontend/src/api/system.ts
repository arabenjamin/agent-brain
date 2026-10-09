/**
 * System (`clients/system.rs`, `/api/logs`): one health call for the whole
 * machine, the registry of what the brain is made of, and the log buffer.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, query } from "./client";

export type ComponentStatus = "up" | "not_configured" | "degraded" | "down";

export interface ProbedComponent {
  name: string;
  label: string;
  kind: string;
  required: boolean;
  purpose: string;
  status: ComponentStatus;
  latency_ms: number | null;
  endpoint: string | null;
  detail: string;
}

export interface HealthIssue {
  severity: "critical" | "warn" | "info";
  area: string;
  message: string;
}

/* eslint-disable @typescript-eslint/no-explicit-any -- these blocks are the services' own JSON, passed through */
export interface SystemHealth {
  overall: "ok" | "degraded" | "down";
  checked_at: string;
  components: ProbedComponent[];
  components_declared: number;
  issues: HealthIssue[];
  scheduler: any;
  queue: any;
  gpu_lease: any;
  budget: any;
  version: { sha?: string; branch?: string; dirty?: boolean; subject?: string; deployed_at?: string } | null;
}

export interface Registry {
  counts: Record<string, number>;
  skills: { name: string; tools: { name: string; description: string }[] }[];
  dynamic_tools: { name: string; description: string | null }[];
  agents: {
    name: string; description: string; tool_count: number; model_preference: string | null;
    provider_hint: string | null; spawnable: boolean; specialty: string | null; data_class: string | null;
  }[];
  chains: { id: string; name: string; pattern: string | null; description: string | null; steps: number; no_evaluator: boolean }[];
  mcp_servers: {
    name: string; endpoint: string | null; description: string | null; enabled: boolean;
    auth_scheme: string | null; auth_configured: boolean | null; auth_env_var: string | null;
  }[];
  providers: any;
  routing_policy: any;
  components: { name: string; label: string; kind: string; required: boolean; purpose: string }[];
}
/* eslint-enable @typescript-eslint/no-explicit-any */

export function useSystemHealth(pollMs = 15_000) {
  return useQuery({
    queryKey: ["system", "health"],
    queryFn: () => apiJson<SystemHealth>("/api/system/health"),
    refetchInterval: pollMs,
  });
}

export function useRegistry() {
  return useQuery({
    queryKey: ["system", "registry"],
    queryFn: () => apiJson<Registry>("/api/system/registry"),
    staleTime: 60_000,
  });
}

export interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

export interface LogPage {
  count: number;
  entries: LogEntry[];
  /** Lines in the buffer matching the filter (the page may hold fewer). */
  matched: number;
  buffered: number;
  capacity: number;
  /** The oldest line the buffer still holds. */
  oldest: string | null;
}

export interface LogQuery {
  level: string;
  /** Comma list of target prefixes; `-prefix` drops one. */
  target?: string;
  /** Comma list of targets shown only at WARN or above. */
  quiet?: string;
  q?: string;
  limit: number;
}

/**
 * Modules whose INFO lines are the brain's routine activity (a tick, a job,
 * a tool call) rather than the system: "hide routine activity" quiets them.
 * Their warnings and errors always show.
 */
export const ACTIVITY_TARGETS = [
  "agent_brain::services::queue",
  "agent_brain::services::scheduler",
  "agent_brain::services::agent_runner",
  "agent_brain::services::knowledge",
  "agent_brain::skills",
  "agent_brain::clients::chat",
  "agent_brain_repository::task",
  "agent_brain_repository::agent_job",
];

export function useLogs(q: LogQuery, live: boolean) {
  return useQuery({
    queryKey: ["system", "logs", q],
    queryFn: () => apiJson<LogPage>(`/api/logs${query({ ...q })}`),
    refetchInterval: live ? 5_000 : false,
    placeholderData: (prev) => prev,
  });
}

// ── Architecture (`clients/architecture.rs`, `services/architecture.rs`) ────

export interface ArchNode {
  /** `kind:name`, what a layout.yaml selector matches. */
  id: string;
  kind: "skill" | "routes" | "component" | "provider" | "mcp" | "collection" | "static";
  name: string;
  label: string;
  count: number;
  /** The rows behind `count`: tools, routes, schedule names, … */
  items: string[];
  detail: string;
  /* eslint-disable-next-line @typescript-eslint/no-explicit-any -- per-kind extras */
  meta: Record<string, any>;
}

export interface ArchGroup {
  id: string;
  label: string;
  column: number;
  blurb: string;
  nodes: ArchNode[];
}

export interface ArchChange { added: string[]; removed: string[] }
export type ArchDiff = Record<string, ArchChange>;

export interface ArchSnapshot {
  id: string;
  taken_at: string;
  trigger: string;
  version: string | null;
  counts: Record<string, number>;
  diff: ArchDiff;
  summary: string;
  previous_id: string | null;
}

export interface Architecture {
  generated_at: string;
  counts: Record<string, number>;
  groups: ArchGroup[];
  unplaced: ArchNode[];
  stale: string[];
  edges: { from: string; to: string; label: string }[];
  topology: {
    networks: { name: string; internal: boolean; external: boolean; note: string }[];
    containers: { name: string; networks: string[]; ports: string[]; component: string | null; note: string }[];
    host: { name: string; component: string | null; note: string }[];
  };
  layout_loaded: boolean;
  /** Live against the newest snapshot: never out of date. */
  since_snapshot: { snapshot_id: string; taken_at: string; version: string | null; summary: string; diff: ArchDiff } | null;
  history: ArchSnapshot[];
}

export function useArchitecture() {
  return useQuery({
    queryKey: ["system", "architecture"],
    queryFn: () => apiJson<Architecture>("/api/architecture"),
    staleTime: 60_000,
  });
}

/** Display names for the diff and count categories. */
export const ARCH_CATEGORY: Record<string, string> = {
  skills: "skills", tools: "tools", routes: "routes", agents: "agents", chains: "chains",
  schedules: "schedules", mcp_servers: "MCP servers", dynamic_tools: "dynamic tools",
  providers: "providers", components: "services",
};

/** `+3 tools, −1 schedule` — counts only, for a one-line summary. */
export function diffCounts(d: ArchDiff): string {
  const parts: string[] = [];
  for (const [cat, label] of Object.entries(ARCH_CATEGORY)) {
    const c = d[cat];
    if (!c) continue;
    if (c.added.length) parts.push(`+${c.added.length} ${label}`);
    if (c.removed.length) parts.push(`−${c.removed.length} ${label}`);
  }
  return parts.join(", ");
}
