/**
 * Schedule and chain editing: data shapes and the step-draft round trip.
 * Split out of the former 1900-line ScheduledTasksPanel (HBI Phase 2).
 */

// ─── Types ────────────────────────────────────────────────────────────────────

export interface ScheduledTask {
  id: string;
  name: string;
  description?: string;
  enabled: boolean;
  interval_seconds: number;
  steps: string; // JSON string of ChainStep[]
  last_run_at?: string;
  next_run_at: string;
  created_at: string;
  updated_at: string;
}

export interface SchedulerChain {
  id: string;
  name: string;
  pattern: string;
  patterns: string[];
  priority: number;
  description: string;
  no_evaluator: boolean;
  evaluation_rubric?: string | null;
  step_count: number;
  steps: string; // JSON string
  updated_at?: string;
}

// ─── API helpers ─────────────────────────────────────────────────────────────

export function stUrl(path = "") {
  return `/scheduled-tasks${path}`;
}

export function fmtInterval(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${(secs / 3600).toFixed(1)}h`;
  return `${(secs / 86400).toFixed(1)}d`;
}

export function fmtDatetime(iso?: string): string {
  if (!iso) return "—";
  try {
    return new Date(iso).toLocaleString(undefined, {
      month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    });
  } catch {
    return iso;
  }
}

// ─── Step builder types ───────────────────────────────────────────────────────

export interface ArgPair {
  key: string;
  value: string;
}

export interface StepDraft {
  tool_name: string;
  args: ArgPair[];
  provider_hint: string;
  priority: number;
}

/**
 * `provider_hint` values offered in the step editor. "ollama" routes to the
 * local backend, anything else to that cloud provider. A step that already
 * carries another value keeps it (see StepCard).
 */
export const PROVIDERS = ["ollama", "ollama-cloud", "anthropic", "gemini"] as const;
export const DEFAULT_PROVIDER = "ollama";

// Grouped tool options for the dropdown — keep in sync with live tools/list
export const TOOL_GROUPS: { group: string; tools: string[] }[] = [
  {
    group: "Knowledge",
    tools: [
      "store_note", "search_notes", "list_notes", "recall_recent",
      "consolidate_memories", "prune_old_notes",
      "synthesize_knowledge", "reason",
    ],
  },
  {
    group: "Tasks",
    tools: [
      "create_task", "update_task", "list_tasks", "decompose_goal",
      "reflect_on_work", "record_outcome",
    ],
  },
  {
    group: "Agent Jobs",
    tools: [
      "enqueue_jobs", "manage_job", "manage_chain",
      "get_job_result", "dead_letter",
      "update_job_progress", "set_worker_config",
    ],
  },
  {
    group: "Working Memory",
    tools: ["push_context", "summarise_session", "list_sessions", "notify_user"],
  },
  {
    group: "HTTP / Search",
    tools: ["http_request", "search_web", "get_search_usage", "ingest_api", "define_api_context"],
  },
  {
    group: "Sub-Agents",
    tools: ["run_agent", "select_agent"],
  },
  {
    group: "Proposals",
    tools: ["write_proposal", "read_proposal", "list_proposals", "dismiss_proposal"],
  },
  {
    group: "Scheduler",
    tools: ["manage_scheduled_task", "scheduler_control", "run_scheduler_tick"],
  },
  {
    group: "Model",
    tools: ["use_model", "reload_models"],
  },
  {
    group: "Procedures / Dynamic",
    tools: ["manage_dynamic_tool", "execute_procedure", "store_procedure", "search_procedures"],
  },
  {
    group: "Query",
    tools: ["neo4j_query"],
  },
  {
    group: "Codebase",
    tools: [
      "read_codebase_file", "list_codebase_files", "search_codebase",
      "get_git_log", "get_git_diff", "get_file_tree", "analyze_own_structure",
    ],
  },
  {
    group: "Context / Resources",
    tools: ["context", "resource"],
  },
  {
    group: "WebSocket",
    tools: ["ws_connect", "ws_send", "ws_receive", "ws_close"],
  },
];

export type ToolGroup = { group: string; tools: string[] };

export function makeDefaultStep(): StepDraft {
  return {
    tool_name: "search_notes",
    args: [{ key: "query", value: "{{goal}}" }],
    provider_hint: DEFAULT_PROVIDER,
    priority: 1,
  };
}

// Arg values are edited as text but must round-trip their real JSON type when
// saved: `query_params` is an object, `pick` an array, `timeout_ms` a number.
// The generic key/value editor once wrote every value back as a string, so an
// object became the literal "[object Object]" and an array a comma-joined
// string — which the strict `http_request` arg parser rejected, silently
// breaking the step on every run. Parse each value as JSON; a bare string
// (plain text, or a `{{template}}` var) is not valid JSON and is kept verbatim.
// To force a numeric- or boolean-looking value to STAY a string, quote it
// ("12345") — JSON.parse then yields the string.
export function parseArgValue(value: string): unknown {
  if (value.trim() === "") return value;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

export function stepDraftsToJson(drafts: StepDraft[]): string {
  const steps = drafts.map((d) => {
    const argsObj: Record<string, unknown> = {};
    for (const { key, value } of d.args) {
      if (key.trim()) argsObj[key.trim()] = parseArgValue(value);
    }
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const step: Record<string, any> = { tool_name: d.tool_name };
    if (Object.keys(argsObj).length > 0) step.arguments = argsObj;
    if (d.provider_hint) step.provider_hint = d.provider_hint;
    if (d.priority !== 1) step.priority = d.priority;
    return step;
  });
  return JSON.stringify(steps, null, 2);
}

export function jsonToStepDrafts(json: string): StepDraft[] | null {
  try {
    const parsed = JSON.parse(json);
    if (!Array.isArray(parsed)) return null;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return parsed.map((s: any) => {
      // Support both old format (tool/args) and new format (tool_name/arguments)
      const toolName: string = s.tool_name ?? s.tool ?? "";
      const rawArgs = s.arguments ?? s.args ?? {};
      // Preserve non-string arg types (object/array/number/bool) as their JSON
      // text so they stay editable and round-trip back through parseArgValue;
      // String(v) here is what turned an object into "[object Object]".
      const args: ArgPair[] = typeof rawArgs === "object" && rawArgs !== null
        ? Object.entries(rawArgs).map(([k, v]) => ({
            key: k,
            value: typeof v === "string" ? v : JSON.stringify(v),
          }))
        : [];
      return {
        tool_name: toolName,
        args,
        provider_hint: s.provider_hint ?? DEFAULT_PROVIDER,
        priority: s.priority ?? 1,
      };
    });
  } catch {
    return null;
  }
}

