import { useCallback, useEffect, useRef, useState } from "react";
import { getBrainUrl, getApiKey } from "../../api/config";

// ─── Types ────────────────────────────────────────────────────────────────────

interface ScheduledTask {
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

// ─── API helpers ─────────────────────────────────────────────────────────────

function stUrl(path = "") {
  return `${getBrainUrl()}/scheduled-tasks${path}`;
}

function authHeaders(): Record<string, string> {
  const key = getApiKey();
  return key ? { Authorization: `Bearer ${key}` } : {};
}

async function apiFetch(url: string, init: RequestInit = {}) {
  return fetch(url, {
    ...init,
    headers: { "Content-Type": "application/json", ...authHeaders(), ...(init.headers ?? {}) },
  });
}

function fmtInterval(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${(secs / 3600).toFixed(1)}h`;
  return `${(secs / 86400).toFixed(1)}d`;
}

function fmtDatetime(iso?: string): string {
  if (!iso) return "—";
  try {
    return new Date(iso).toLocaleString(undefined, {
      month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    });
  } catch {
    return iso;
  }
}

// ─── Scheduler config types ───────────────────────────────────────────────────

interface SchedulerConfig {
  interval_secs: number;
  enabled: boolean;
  local_model: string;
  idle_sleep_after_ticks: number;
  sleep_interval_secs: number;
}

// ─── Step builder types ───────────────────────────────────────────────────────

interface ArgPair {
  key: string;
  value: string;
}

interface StepDraft {
  tool_name: string;
  args: ArgPair[];
  provider_hint: string;
  model: string;
  priority: number;
}

const PROVIDERS = ["ollama", "anthropic", "gemini", "vllm"] as const;
const DEFAULT_PROVIDER = "ollama";
const DEFAULT_MODEL = "gemma4:latest";

// Grouped tool options for the dropdown
const TOOL_GROUPS: { group: string; tools: string[] }[] = [
  {
    group: "Knowledge",
    tools: [
      "store_note", "search_notes",
      "consolidate_memories", "prune_old_notes",
      "synthesize_knowledge", "reason",
    ],
  },
  {
    group: "Tasks",
    tools: [
      "create_task", "update_task", "decompose_goal",
      "reflect_on_work", "record_outcome",
    ],
  },
  {
    group: "Agent Jobs",
    tools: [
      "enqueue_jobs", "manage_job",
      "set_worker_config", "dead_letter", "update_job_progress",
    ],
  },
  {
    group: "Working Memory",
    tools: ["push_context", "summarise_session"],
  },
  {
    group: "HTTP / Search",
    tools: ["http_request", "search_web", "define_api_context", "list_api_contexts", "load_api_context"],
  },
  {
    group: "Scheduler",
    tools: [
      "list_scheduled_tasks", "create_scheduled_task", "update_scheduled_task",
      "manage_scheduled_task", "manage_chain",
    ],
  },
  {
    group: "Model",
    tools: ["use_model", "reload_models"],
  },
  {
    group: "Procedures / Dynamic",
    tools: ["manage_dynamic_tool", "execute_procedure", "store_procedure"],
  },
  {
    group: "Query",
    tools: ["neo4j_query", "duckdb_query"],
  },
  {
    group: "Codebase",
    tools: [
      "read_file", "list_files", "search_code", "write_file", "run_tests",
      "git_log", "git_diff", "git_status", "search_issues", "create_issue",
    ],
  },
];

function makeDefaultStep(): StepDraft {
  return {
    tool_name: "search_notes",
    args: [{ key: "query", value: "{{goal}}" }],
    provider_hint: DEFAULT_PROVIDER,
    model: DEFAULT_MODEL,
    priority: 1,
  };
}

function stepDraftsToJson(drafts: StepDraft[]): string {
  const steps = drafts.map((d) => {
    const argsObj: Record<string, string> = {};
    for (const { key, value } of d.args) {
      if (key.trim()) argsObj[key.trim()] = value;
    }
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const step: Record<string, any> = { tool_name: d.tool_name };
    if (Object.keys(argsObj).length > 0) step.arguments = argsObj;
    if (d.provider_hint) step.provider_hint = d.provider_hint;
    if (d.model) step.model = d.model;
    if (d.priority !== 1) step.priority = d.priority;
    return step;
  });
  return JSON.stringify(steps, null, 2);
}

function jsonToStepDrafts(json: string): StepDraft[] | null {
  try {
    const parsed = JSON.parse(json);
    if (!Array.isArray(parsed)) return null;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return parsed.map((s: any) => {
      // Support both old format (tool/args) and new format (tool_name/arguments)
      const toolName: string = s.tool_name ?? s.tool ?? "";
      const rawArgs = s.arguments ?? s.args ?? {};
      const args: ArgPair[] = typeof rawArgs === "object" && rawArgs !== null
        ? Object.entries(rawArgs).map(([k, v]) => ({ key: k, value: String(v) }))
        : [];
      return {
        tool_name: toolName,
        args,
        provider_hint: s.provider_hint ?? DEFAULT_PROVIDER,
        model: s.model ?? DEFAULT_MODEL,
        priority: s.priority ?? 1,
      };
    });
  } catch {
    return null;
  }
}

// ─── ScheduledTasksPanel ─────────────────────────────────────────────────────

export default function ScheduledTasksPanel() {
  const [tasks, setTasks] = useState<ScheduledTask[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editTask, setEditTask] = useState<ScheduledTask | null>(null);
  const [enabledOnly, setEnabledOnly] = useState(false);

  // Scheduler config state
  const [schedulerConfig, setSchedulerConfig] = useState<SchedulerConfig | null>(null);
  const [showSettings, setShowSettings] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const url = enabledOnly ? stUrl("?enabled_only=true") : stUrl();
      const res = await apiFetch(url);
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      const data = await res.json();
      setTasks(data.scheduled_tasks ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [enabledOnly]);

  const fetchSchedulerConfig = useCallback(async () => {
    try {
      const res = await apiFetch(`${getBrainUrl()}/scheduler-config`);
      if (!res.ok) return;
      const data = await res.json();
      setSchedulerConfig(data.config ?? null);
    } catch { /* best-effort */ }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);
  useEffect(() => { fetchSchedulerConfig(); }, [fetchSchedulerConfig]);

  const handleDelete = async (id: string, name: string) => {
    if (!confirm(`Delete scheduled task "${name}"?`)) return;
    try {
      const res = await apiFetch(stUrl(`/${id}`), { method: "DELETE" });
      if (!res.ok && res.status !== 204) throw new Error(`${res.status}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleToggleEnabled = async (task: ScheduledTask) => {
    try {
      const res = await apiFetch(stUrl(`/${task.id}`), {
        method: "PUT",
        body: JSON.stringify({ enabled: !task.enabled }),
      });
      if (!res.ok) throw new Error(`${res.status}`);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleEdit = (task: ScheduledTask) => {
    setEditTask(task);
    setShowForm(true);
  };

  const handleFormClose = () => {
    setShowForm(false);
    setEditTask(null);
  };

  const handleFormSaved = () => {
    handleFormClose();
    refresh();
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", padding: "16px", gap: "12px", overflow: "hidden" }}>
      {/* Header */}
      <div style={{ display: "flex", alignItems: "center", gap: "12px", flexWrap: "wrap" }}>
        <h2 style={{ margin: 0, fontSize: "1.1rem" }}>Scheduled Tasks</h2>

        <label style={{ display: "flex", alignItems: "center", gap: "6px", fontSize: "0.85rem", cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={enabledOnly}
            onChange={(e) => setEnabledOnly(e.target.checked)}
            style={{ accentColor: "var(--accent, #89b4fa)" }}
          />
          Enabled only
        </label>

        <div style={{ marginLeft: "auto", display: "flex", gap: "8px" }}>
          <button className="sidebar-btn" onClick={refresh} disabled={loading} title="Refresh">
            {loading ? "..." : "Refresh"}
          </button>
          <button
            className="sidebar-btn"
            onClick={() => setShowSettings((v) => !v)}
            title="Scheduler settings"
          >
            ⚙ Settings
          </button>
          <button className="sidebar-btn active" onClick={() => { setEditTask(null); setShowForm(true); }}>
            + New Task
          </button>
        </div>
      </div>

      {/* Scheduler settings panel */}
      {showSettings && (
        <SchedulerSettingsPanel
          config={schedulerConfig}
          onSaved={(updated) => { setSchedulerConfig(updated); }}
        />
      )}

      {error && (
        <div style={{ color: "var(--error, #f87171)", fontSize: "0.85rem" }}>{error}</div>
      )}

      {/* Task list */}
      <div style={{ flex: 1, overflowY: "auto", display: "flex", flexDirection: "column", gap: "8px" }}>
        {tasks.length === 0 && !loading && (
          <p style={{ opacity: 0.5, fontSize: "0.9rem" }}>No scheduled tasks found.</p>
        )}
        {tasks.map((task) => (
          <ScheduledTaskRow
            key={task.id}
            task={task}
            onToggleEnabled={() => handleToggleEnabled(task)}
            onEdit={() => handleEdit(task)}
            onDelete={() => handleDelete(task.id, task.name)}
          />
        ))}
      </div>

      {/* Create/Edit form overlay */}
      {showForm && (
        <ScheduledTaskForm
          existing={editTask}
          onSaved={handleFormSaved}
          onCancel={handleFormClose}
        />
      )}
    </div>
  );
}

// ─── ScheduledTaskRow ─────────────────────────────────────────────────────────

function ScheduledTaskRow({
  task,
  onToggleEnabled,
  onEdit,
  onDelete,
}: {
  task: ScheduledTask;
  onToggleEnabled: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const [stepsExpanded, setStepsExpanded] = useState(false);

  let stepCount = 0;
  try {
    const parsed = JSON.parse(task.steps);
    stepCount = Array.isArray(parsed) ? parsed.length : 0;
  } catch { /* ignore */ }

  return (
    <div
      style={{
        background: "var(--surface, #1e1e2e)",
        border: `1px solid ${task.enabled ? "var(--border, #313244)" : "var(--border, #313244)"}`,
        borderLeft: `3px solid ${task.enabled ? "var(--accent, #89b4fa)" : "var(--border, #45475a)"}`,
        borderRadius: "8px",
        padding: "10px 14px",
        opacity: task.enabled ? 1 : 0.6,
      }}
    >
      <div style={{ display: "flex", alignItems: "flex-start", gap: "12px" }}>
        {/* Enable toggle */}
        <input
          type="checkbox"
          checked={task.enabled}
          onChange={onToggleEnabled}
          style={{ marginTop: "3px", cursor: "pointer", accentColor: "var(--accent, #89b4fa)" }}
          title={task.enabled ? "Disable" : "Enable"}
        />

        {/* Content */}
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
            <span style={{ fontWeight: 500, wordBreak: "break-word" }}>{task.name}</span>
            <IntervalBadge secs={task.interval_seconds} />
            {!task.enabled && (
              <span style={{ fontSize: "0.72rem", color: "#a6adc8", border: "1px solid #45475a", borderRadius: "4px", padding: "1px 5px" }}>
                disabled
              </span>
            )}
          </div>

          {task.description && (
            <p style={{ margin: "4px 0 0", fontSize: "0.82rem", opacity: 0.7, wordBreak: "break-word" }}>
              {task.description}
            </p>
          )}

          <div style={{ display: "flex", gap: "14px", marginTop: "6px", fontSize: "0.78rem", opacity: 0.65, flexWrap: "wrap" }}>
            <span>Last run: {fmtDatetime(task.last_run_at)}</span>
            <span>Next run: {fmtDatetime(task.next_run_at)}</span>
            <button
              onClick={() => setStepsExpanded((v) => !v)}
              style={{ background: "none", border: "none", padding: 0, cursor: "pointer", color: "var(--accent, #89b4fa)", fontSize: "0.78rem" }}
            >
              {stepCount} step{stepCount !== 1 ? "s" : ""} {stepsExpanded ? "▲" : "▼"}
            </button>
          </div>

          {stepsExpanded && (
            <pre style={{
              marginTop: "8px", fontSize: "0.75rem", background: "var(--surface2, #313244)",
              borderRadius: "6px", padding: "8px", overflow: "auto", maxHeight: "200px",
              whiteSpace: "pre-wrap", wordBreak: "break-all",
            }}>
              {(() => { try { return JSON.stringify(JSON.parse(task.steps), null, 2); } catch { return task.steps; } })()}
            </pre>
          )}
        </div>

        {/* Actions */}
        <div style={{ display: "flex", gap: "6px", flexShrink: 0 }}>
          <button
            onClick={onEdit}
            style={{ background: "none", border: "1px solid var(--border, #313244)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.78rem", color: "inherit" }}
          >
            Edit
          </button>
          <button
            onClick={onDelete}
            style={{ background: "none", border: "1px solid #f87171", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.78rem", color: "#f87171" }}
          >
            Del
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── SchedulerSettingsPanel ───────────────────────────────────────────────────

function SchedulerSettingsPanel({
  config,
  onSaved,
}: {
  config: SchedulerConfig | null;
  onSaved: (updated: SchedulerConfig) => void;
}) {
  const [localModel, setLocalModel] = useState(config?.local_model ?? DEFAULT_MODEL);
  const [enabled, setEnabled] = useState(config?.enabled ?? true);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (config) {
      setLocalModel(config.local_model);
      setEnabled(config.enabled);
    }
  }, [config]);

  const handleSave = async () => {
    setSaving(true);
    setSaveError(null);
    setSaved(false);
    try {
      const res = await apiFetch(`${getBrainUrl()}/scheduler-config`, {
        method: "PUT",
        body: JSON.stringify({ local_model: localModel, enabled }),
      });
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        throw new Error((data as { error?: string }).error ?? `${res.status}`);
      }
      const data = await res.json();
      onSaved(data.config as SchedulerConfig);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setSaveError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const panelStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)",
    border: "1px solid var(--border, #45475a)",
    borderRadius: "8px",
    padding: "12px 16px",
    display: "flex",
    flexDirection: "column",
    gap: "10px",
    fontSize: "0.85rem",
  };

  const rowStyle: React.CSSProperties = {
    display: "flex",
    alignItems: "center",
    gap: "10px",
    flexWrap: "wrap",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface, #1e1e2e)",
    border: "1px solid var(--border, #45475a)",
    borderRadius: "6px",
    padding: "4px 8px",
    color: "inherit",
    fontSize: "0.85rem",
    flex: 1,
    minWidth: "180px",
  };

  return (
    <div style={panelStyle}>
      <div style={{ fontWeight: 600, opacity: 0.8 }}>Scheduler Settings</div>

      {config === null && (
        <p style={{ opacity: 0.5, margin: 0 }}>Scheduler not yet available.</p>
      )}

      {config !== null && (
        <>
          <div style={rowStyle}>
            <label style={{ display: "flex", alignItems: "center", gap: "8px", cursor: "pointer" }}>
              <input
                type="checkbox"
                checked={enabled}
                onChange={(e) => setEnabled(e.target.checked)}
                style={{ accentColor: "var(--accent, #89b4fa)" }}
              />
              Enabled
            </label>
          </div>

          <div style={rowStyle}>
            <span style={{ whiteSpace: "nowrap", opacity: 0.8 }}>Local model</span>
            <input
              style={inputStyle}
              value={localModel}
              onChange={(e) => setLocalModel(e.target.value)}
              placeholder={`e.g. ${DEFAULT_MODEL}`}
              spellCheck={false}
            />
            <span style={{ opacity: 0.45, fontSize: "0.78rem", whiteSpace: "nowrap" }}>
              used by background tasks
            </span>
          </div>

          {saveError && (
            <p style={{ color: "#f87171", margin: 0, fontSize: "0.8rem" }}>{saveError}</p>
          )}

          <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
            <button
              className="sidebar-btn active"
              onClick={handleSave}
              disabled={saving}
            >
              {saving ? "Saving…" : "Save"}
            </button>
            {saved && <span style={{ color: "#a6e3a1", fontSize: "0.8rem" }}>Saved!</span>}
            <span style={{ marginLeft: "auto", opacity: 0.4, fontSize: "0.75rem" }}>
              interval: {config.interval_secs}s · sleep after {config.idle_sleep_after_ticks} idle ticks
            </span>
          </div>
        </>
      )}
    </div>
  );
}

// ─── Badges ──────────────────────────────────────────────────────────────────

function IntervalBadge({ secs }: { secs: number }) {
  return (
    <span style={{ fontSize: "0.72rem", color: "#cba6f7", border: "1px solid #cba6f7", borderRadius: "4px", padding: "1px 5px" }}>
      every {fmtInterval(secs)}
    </span>
  );
}

// ─── StepCard ─────────────────────────────────────────────────────────────────

function StepCard({
  step,
  index,
  total,
  onChange,
  onRemove,
  onMoveUp,
  onMoveDown,
  inputStyle,
}: {
  step: StepDraft;
  index: number;
  total: number;
  onChange: (updated: StepDraft) => void;
  onRemove: () => void;
  onMoveUp: () => void;
  onMoveDown: () => void;
  inputStyle: React.CSSProperties;
}) {
  const update = (patch: Partial<StepDraft>) => onChange({ ...step, ...patch });

  const setArgKey = (i: number, key: string) => {
    const args = step.args.map((a, idx) => idx === i ? { ...a, key } : a);
    update({ args });
  };

  const setArgValue = (i: number, value: string) => {
    const args = step.args.map((a, idx) => idx === i ? { ...a, value } : a);
    update({ args });
  };

  const removeArg = (i: number) => {
    update({ args: step.args.filter((_, idx) => idx !== i) });
  };

  const addArg = () => {
    update({ args: [...step.args, { key: "", value: "" }] });
  };

  const cardStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)",
    border: "1px solid var(--border, #45475a)",
    borderRadius: "8px",
    padding: "12px",
    display: "flex",
    flexDirection: "column",
    gap: "8px",
  };

  const rowStyle: React.CSSProperties = {
    display: "flex",
    alignItems: "center",
    gap: "8px",
  };

  const smallSelectStyle: React.CSSProperties = {
    ...inputStyle,
    padding: "3px 6px",
    fontSize: "0.8rem",
  };

  return (
    <div style={cardStyle}>
      {/* Step header */}
      <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
        <span style={{ fontSize: "0.75rem", opacity: 0.55, fontWeight: 600, minWidth: "48px" }}>
          Step {index + 1}
        </span>
        <div style={{ display: "flex", gap: "4px", marginLeft: "auto" }}>
          <button
            type="button"
            onClick={onMoveUp}
            disabled={index === 0}
            title="Move up"
            style={{ background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "1px 6px", cursor: index === 0 ? "default" : "pointer", fontSize: "0.72rem", color: "inherit", opacity: index === 0 ? 0.3 : 0.7 }}
          >
            ▲
          </button>
          <button
            type="button"
            onClick={onMoveDown}
            disabled={index === total - 1}
            title="Move down"
            style={{ background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "1px 6px", cursor: index === total - 1 ? "default" : "pointer", fontSize: "0.72rem", color: "inherit", opacity: index === total - 1 ? 0.3 : 0.7 }}
          >
            ▼
          </button>
          <button
            type="button"
            onClick={onRemove}
            title="Remove step"
            style={{ background: "none", border: "1px solid #f87171", borderRadius: "4px", padding: "1px 8px", cursor: "pointer", fontSize: "0.72rem", color: "#f87171" }}
          >
            Remove
          </button>
        </div>
      </div>

      {/* Tool dropdown */}
      <div style={rowStyle}>
        <span style={{ fontSize: "0.82rem", opacity: 0.75, minWidth: "60px" }}>Tool</span>
        <select
          style={{ ...inputStyle, flex: 1 }}
          value={TOOL_GROUPS.flatMap(g => g.tools).includes(step.tool_name) ? step.tool_name : "__custom__"}
          onChange={(e) => {
            if (e.target.value !== "__custom__") update({ tool_name: e.target.value });
          }}
        >
          {TOOL_GROUPS.map((g) => (
            <optgroup key={g.group} label={g.group}>
              {g.tools.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </optgroup>
          ))}
          <optgroup label="Custom">
            <option value="__custom__">— custom (type below) —</option>
          </optgroup>
        </select>
      </div>

      {/* Custom tool name input — shown when tool not in list */}
      {!TOOL_GROUPS.flatMap(g => g.tools).includes(step.tool_name) && (
        <div style={rowStyle}>
          <span style={{ fontSize: "0.82rem", opacity: 0.75, minWidth: "60px" }}>Name</span>
          <input
            style={{ ...inputStyle, flex: 1 }}
            value={step.tool_name}
            onChange={(e) => update({ tool_name: e.target.value })}
            placeholder="custom_tool_name"
            spellCheck={false}
          />
        </div>
      )}

      {/* Arguments */}
      <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
        <span style={{ fontSize: "0.78rem", opacity: 0.6 }}>
          Arguments{" "}
          <span style={{ opacity: 0.5 }}>
            — use <code style={{ fontSize: "0.75rem" }}>{"{{goal}}"}</code>, <code style={{ fontSize: "0.75rem" }}>{"{{task_id}}"}</code>, <code style={{ fontSize: "0.75rem" }}>{"{{date}}"}</code>
          </span>
        </span>
        {step.args.map((arg, i) => (
          <div key={i} style={{ display: "flex", gap: "6px", alignItems: "center" }}>
            <input
              style={{ ...inputStyle, width: "120px", flex: "0 0 120px", fontSize: "0.82rem" }}
              value={arg.key}
              onChange={(e) => setArgKey(i, e.target.value)}
              placeholder="key"
              spellCheck={false}
            />
            <span style={{ opacity: 0.4, fontSize: "0.8rem" }}>=</span>
            <input
              style={{ ...inputStyle, flex: 1, fontSize: "0.82rem" }}
              value={arg.value}
              onChange={(e) => setArgValue(i, e.target.value)}
              placeholder="value"
              spellCheck={false}
            />
            <button
              type="button"
              onClick={() => removeArg(i)}
              style={{ background: "none", border: "none", cursor: "pointer", color: "#f87171", fontSize: "0.9rem", padding: "0 4px", flexShrink: 0 }}
              title="Remove argument"
            >
              ×
            </button>
          </div>
        ))}
        <button
          type="button"
          onClick={addArg}
          style={{ alignSelf: "flex-start", background: "none", border: "1px dashed var(--border, #45475a)", borderRadius: "4px", padding: "2px 10px", cursor: "pointer", fontSize: "0.78rem", color: "var(--accent, #89b4fa)", marginTop: "2px" }}
        >
          + Add argument
        </button>
      </div>

      {/* Provider + Model + Priority row */}
      <div style={{ display: "flex", gap: "8px", flexWrap: "wrap", alignItems: "center" }}>
        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65 }}>Provider</span>
          <select
            style={smallSelectStyle}
            value={step.provider_hint}
            onChange={(e) => update({ provider_hint: e.target.value })}
          >
            {PROVIDERS.map((p) => (
              <option key={p} value={p}>{p}</option>
            ))}
          </select>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65 }}>Model</span>
          <input
            style={{ ...inputStyle, width: "150px", padding: "3px 6px", fontSize: "0.8rem" }}
            value={step.model}
            onChange={(e) => update({ model: e.target.value })}
            placeholder={DEFAULT_MODEL}
            spellCheck={false}
          />
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65 }}>Priority</span>
          <select
            style={smallSelectStyle}
            value={step.priority}
            onChange={(e) => update({ priority: Number(e.target.value) })}
          >
            <option value={0}>0 — low</option>
            <option value={1}>1 — normal</option>
            <option value={2}>2 — high</option>
            <option value={3}>3 — critical</option>
          </select>
        </div>
      </div>
    </div>
  );
}

// ─── ScheduledTaskForm ────────────────────────────────────────────────────────

function ScheduledTaskForm({
  existing,
  onSaved,
  onCancel,
}: {
  existing: ScheduledTask | null;
  onSaved: () => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [enabled, setEnabled] = useState(existing?.enabled ?? true);
  const [intervalUnit, setIntervalUnit] = useState<"seconds" | "minutes" | "hours" | "days">("days");
  const [intervalValue, setIntervalValue] = useState<number>(1);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showRaw, setShowRaw] = useState(false);
  const [rawJson, setRawJson] = useState("");
  const [rawJsonError, setRawJsonError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Step builder state
  const [steps, setSteps] = useState<StepDraft[]>(() => {
    if (existing) {
      const parsed = jsonToStepDrafts(existing.steps);
      if (parsed && parsed.length > 0) return parsed;
    }
    return [makeDefaultStep()];
  });

  // Sync interval_seconds ↔ unit/value
  useEffect(() => {
    const secs = existing?.interval_seconds ?? 86400;
    if (secs % 86400 === 0) { setIntervalUnit("days"); setIntervalValue(secs / 86400); }
    else if (secs % 3600 === 0) { setIntervalUnit("hours"); setIntervalValue(secs / 3600); }
    else if (secs % 60 === 0) { setIntervalUnit("minutes"); setIntervalValue(secs / 60); }
    else { setIntervalUnit("seconds"); setIntervalValue(secs); }
  }, [existing?.interval_seconds]);

  // Sync raw JSON when toggling to raw view
  useEffect(() => {
    if (showRaw) {
      setRawJson(stepDraftsToJson(steps));
      setRawJsonError(null);
    }
  }, [showRaw]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => { inputRef.current?.focus(); }, []);

  const computedIntervalSeconds = () => {
    const multipliers = { seconds: 1, minutes: 60, hours: 3600, days: 86400 };
    return Math.max(1, Math.round(intervalValue * multipliers[intervalUnit]));
  };

  const applyRawJson = () => {
    const parsed = jsonToStepDrafts(rawJson);
    if (!parsed) {
      setRawJsonError("Invalid JSON or not an array of steps");
      return false;
    }
    setSteps(parsed);
    setRawJsonError(null);
    return true;
  };

  const handleToggleRaw = () => {
    if (showRaw) {
      // Switching back to builder — try to apply raw JSON
      if (!applyRawJson()) return;
      setShowRaw(false);
    } else {
      setShowRaw(true);
    }
  };

  const updateStep = (i: number, updated: StepDraft) => {
    setSteps(steps.map((s, idx) => idx === i ? updated : s));
  };

  const removeStep = (i: number) => {
    setSteps(steps.filter((_, idx) => idx !== i));
  };

  const addStep = () => {
    setSteps([...steps, makeDefaultStep()]);
  };

  const moveStep = (i: number, dir: -1 | 1) => {
    const j = i + dir;
    if (j < 0 || j >= steps.length) return;
    const next = [...steps];
    [next[i], next[j]] = [next[j], next[i]];
    setSteps(next);
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;

    // If raw editor is open, try to apply it first
    if (showRaw && !applyRawJson()) return;

    const stepsJson = stepDraftsToJson(steps);

    setSaving(true);
    setError(null);

    const body: Record<string, unknown> = {
      name: name.trim(),
      description: description.trim() || null,
      enabled,
      interval_seconds: computedIntervalSeconds(),
      steps: stepsJson,
    };

    try {
      const res = existing
        ? await apiFetch(stUrl(`/${existing.id}`), { method: "PUT", body: JSON.stringify(body) })
        : await apiFetch(stUrl(), { method: "POST", body: JSON.stringify(body) });

      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        throw new Error((data as { error?: string }).error ?? `${res.status}`);
      }
      onSaved();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const overlayStyle: React.CSSProperties = {
    position: "fixed", inset: 0, background: "rgba(0,0,0,0.6)", display: "flex",
    alignItems: "center", justifyContent: "center", zIndex: 50,
  };

  const cardStyle: React.CSSProperties = {
    background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #313244)",
    borderRadius: "10px", padding: "20px", width: "min(680px, 96vw)", maxHeight: "92vh",
    display: "flex", flexDirection: "column", gap: "12px", overflowY: "auto",
  };

  const labelStyle: React.CSSProperties = {
    display: "flex", flexDirection: "column", gap: "4px", fontSize: "0.85rem",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)", border: "1px solid var(--border, #45475a)",
    borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.9rem",
  };

  return (
    <div style={overlayStyle} onClick={(e) => e.target === e.currentTarget && onCancel()}>
      <div style={cardStyle}>
        <h3 style={{ margin: 0, fontSize: "1rem" }}>
          {existing ? `Edit: ${existing.name}` : "New Scheduled Task"}
        </h3>

        <form onSubmit={handleSubmit} style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <label style={labelStyle}>
            Name *
            <input
              ref={inputRef}
              style={inputStyle}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Daily news briefing"
              required
            />
          </label>

          <label style={labelStyle}>
            Description
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: "56px" }}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="What this task does…"
            />
          </label>

          {/* Interval */}
          <div style={labelStyle}>
            <span>Interval *</span>
            <div style={{ display: "flex", gap: "8px" }}>
              <input
                type="number"
                min={1}
                style={{ ...inputStyle, width: "80px" }}
                value={intervalValue}
                onChange={(e) => setIntervalValue(Math.max(1, Number(e.target.value)))}
              />
              <select
                style={{ ...inputStyle, flex: 1 }}
                value={intervalUnit}
                onChange={(e) => setIntervalUnit(e.target.value as typeof intervalUnit)}
              >
                <option value="seconds">seconds</option>
                <option value="minutes">minutes</option>
                <option value="hours">hours</option>
                <option value="days">days</option>
              </select>
              <span style={{ fontSize: "0.8rem", opacity: 0.6, alignSelf: "center", whiteSpace: "nowrap" }}>
                = {computedIntervalSeconds()}s
              </span>
            </div>
          </div>

          {/* Enabled */}
          <label style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "0.85rem", cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
              style={{ accentColor: "var(--accent, #89b4fa)" }}
            />
            Enabled
          </label>

          {/* Steps header */}
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span style={{ fontSize: "0.85rem", fontWeight: 600 }}>
              Steps ({steps.length})
            </span>
            <button
              type="button"
              onClick={handleToggleRaw}
              style={{ marginLeft: "auto", background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.75rem", color: showRaw ? "var(--accent, #89b4fa)" : "inherit" }}
            >
              {showRaw ? "← Builder" : "Raw JSON"}
            </button>
          </div>

          {/* Raw JSON editor */}
          {showRaw && (
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              <textarea
                style={{ ...inputStyle, fontFamily: "monospace", resize: "vertical", minHeight: "180px", fontSize: "0.82rem" }}
                value={rawJson}
                onChange={(e) => { setRawJson(e.target.value); setRawJsonError(null); }}
                spellCheck={false}
              />
              {rawJsonError && (
                <span style={{ color: "#f87171", fontSize: "0.78rem" }}>{rawJsonError}</span>
              )}
              <span style={{ fontSize: "0.75rem", opacity: 0.5 }}>
                Fields: <code style={{ fontSize: "0.73rem" }}>tool_name</code>, <code style={{ fontSize: "0.73rem" }}>arguments</code>, <code style={{ fontSize: "0.73rem" }}>provider_hint</code>, <code style={{ fontSize: "0.73rem" }}>model</code>, <code style={{ fontSize: "0.73rem" }}>priority</code>, <code style={{ fontSize: "0.73rem" }}>max_attempts</code>
              </span>
            </div>
          )}

          {/* Step cards */}
          {!showRaw && (
            <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              {steps.length === 0 && (
                <p style={{ opacity: 0.45, fontSize: "0.85rem", margin: 0 }}>No steps yet. Add one below.</p>
              )}
              {steps.map((step, i) => (
                <StepCard
                  key={i}
                  step={step}
                  index={i}
                  total={steps.length}
                  onChange={(updated) => updateStep(i, updated)}
                  onRemove={() => removeStep(i)}
                  onMoveUp={() => moveStep(i, -1)}
                  onMoveDown={() => moveStep(i, 1)}
                  inputStyle={inputStyle}
                />
              ))}
              <button
                type="button"
                onClick={addStep}
                style={{ alignSelf: "flex-start", background: "none", border: "1px dashed var(--accent, #89b4fa)", borderRadius: "6px", padding: "6px 16px", cursor: "pointer", fontSize: "0.85rem", color: "var(--accent, #89b4fa)" }}
              >
                + Add Step
              </button>
            </div>
          )}

          {error && <p style={{ color: "#f87171", margin: 0, fontSize: "0.83rem" }}>{error}</p>}

          <div style={{ display: "flex", justifyContent: "flex-end", gap: "8px", marginTop: "4px" }}>
            <button type="button" className="sidebar-btn" onClick={onCancel}>Cancel</button>
            <button type="submit" className="sidebar-btn active" disabled={saving}>
              {saving ? "Saving…" : existing ? "Save" : "Create"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
