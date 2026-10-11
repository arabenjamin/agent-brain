/**
 * Workflow editing: data shapes and the step-draft round trip.
 * Split out of the former 1900-line ScheduledTasksPanel (HBI Phase 2).
 */

// ─── Types ────────────────────────────────────────────────────────────────────

export interface ScheduledTask {
  id: string;
  name: string;
  description?: string;
  enabled: boolean;
  /** null = no schedule; the workflow runs only when started by name. */
  interval_seconds: number | null;
  steps: string; // JSON string of ChainStep[]
  last_run_at?: string;
  next_run_at?: string | null;
  created_at: string;
  updated_at: string;
  topic?: string | null;
  kind?: string | null;
}

// ─── API helpers ─────────────────────────────────────────────────────────────

export function stUrl(path = "") {
  return `/workflows${path}`;
}

// ─── Step builder types ───────────────────────────────────────────────────────

export interface ArgPair {
  key: string;
  value: string;
}

export interface StepDraft {
  tool_name: string;
  args: ArgPair[];
  /** "" when the step has none: it then runs on cloud. */
  provider_hint: string;
  /**
   * A catalog model the step must run on; "" = none (the hint decides). When
   * set, the brain derives `provider_hint` from it and refuses a contradicting
   * one, so the editor writes the derived hint.
   */
  model: string;
  priority: number;
  /**
   * Every other field the step carries (`data_class`, `max_attempts`,
   * `description`, `is_evaluator`, `context_profile`, …), kept as it was so a
   * save writes it back. The editor once dropped them: saving a schedule
   * turned `data_class: public` work private and reset every `max_attempts`.
   */
  extra: Record<string, unknown>;
}

/** The fields the step editor edits itself; everything else rides in `extra`. */
const EDITED_FIELDS = new Set(["tool_name", "tool", "arguments", "args", "provider_hint", "model", "priority"]);

export const DEFAULT_PROVIDER = "ollama";

/**
 * What a step's `provider_hint` actually does (`services/queue.rs`): only
 * "ollama" changes routing, to the local model. Every other value, or none,
 * runs on the brain's active cloud model; the value only chooses which
 * concurrency slot the job waits for. A step that pins a `model` runs on it
 * instead, and its hint is derived from it.
 */
export function providerLabel(hint: string, activeModel?: string): string {
  const cloud = activeModel ? `the brain's active model (${activeModel})` : "the brain's active model";
  if (hint === "ollama") return "Local model";
  if (hint === "") return `Cloud: ${cloud}`;
  return `${hint}: also cloud, on ${cloud}`;
}

/** The hints offered for a new choice: local, or cloud. A step's own other value is kept. */
export const PROVIDER_CHOICES = ["ollama", ""] as const;

// Grouped tool options for the dropdown — keep in sync with live tools/list
export const TOOL_GROUPS: { group: string; tools: string[] }[] = [
  {
    group: "Knowledge",
    tools: [
      "store_note", "search_notes", "recall_recent",
      "consolidate_memories", "prune_old_notes",
      "synthesize_knowledge", "reason",
    ],
  },
  {
    group: "Tasks",
    tools: [
      "create_task", "update_task", "list_tasks", "reflect_on_work",
    ],
  },
  {
    group: "Agent Jobs",
    tools: [
      "enqueue_jobs", "manage_job", "dead_letter",
    ],
  },
  {
    group: "Working Memory",
    tools: ["push_context", "notify_user"],
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
    tools: ["write_proposal"],
  },
  {
    group: "Scheduler",
    tools: ["manage_workflow", "scheduler_control"],
  },
  {
    group: "Plugins",
    tools: ["manage_plugin"],
  },
  {
    group: "Query",
    tools: ["neo4j_query"],
  },
  {
    group: "Codebase",
    tools: [
      "read_codebase_file", "search_codebase", "get_git_log", "analyze_own_structure",
    ],
  },
];

export type ToolGroup = { group: string; tools: string[] };

export function makeDefaultStep(): StepDraft {
  return {
    tool_name: "search_notes",
    args: [{ key: "query", value: "{{goal}}" }],
    provider_hint: DEFAULT_PROVIDER,
    model: "",
    priority: 1,
    extra: {},
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
    const step: Record<string, any> = { ...d.extra, tool_name: d.tool_name };
    if (Object.keys(argsObj).length > 0) step.arguments = argsObj;
    if (d.provider_hint) step.provider_hint = d.provider_hint;
    if (d.model.trim()) step.model = d.model.trim();
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
      const extra = Object.fromEntries(Object.entries(s ?? {}).filter(([k]) => !EDITED_FIELDS.has(k)));
      return {
        tool_name: toolName,
        args,
        provider_hint: typeof s.provider_hint === "string" ? s.provider_hint : "",
        model: typeof s.model === "string" ? s.model : "",
        priority: s.priority ?? 1,
        extra,
      };
    });
  } catch {
    return null;
  }
}

