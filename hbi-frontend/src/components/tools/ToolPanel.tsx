import { useCallback, useEffect, useState } from "react";
import { getMcpClient } from "../../api/mcp";

// ── Types ─────────────────────────────────────────────────────────────────────

interface Tool {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
}

// ── Skill definitions ─────────────────────────────────────────────────────────

interface SkillDef {
  label:  string;
  icon:   string;
  color:  string;
  desc:   string;
  tools:  string[];
}

const SKILLS: SkillDef[] = [
  {
    label: "Knowledge",
    icon:  "🧠",
    color: "var(--green)",
    desc:  "Notes, hybrid RAG search, memory consolidation, spaced-repetition scheduling, entity extraction, and LLM reasoning over the graph.",
    tools: [
      "store_note", "search_notes", "list_notes", "get_note", "update_note",
      "delete_note", "find_related_notes", "search_by_entity",
      "prune_old_notes", "consolidate_memories", "review_due_notes",
      "reason", "audit_action", "explain_reasoning", "ask_clarification",
      "export_graph_visualization",
    ],
  },
  {
    label: "Tasks",
    icon:  "🎯",
    color: "var(--cyan)",
    desc:  "Goal tracking, LLM-powered decomposition into subtasks, DEPENDS_ON edge wiring, outcome recording, and work reflection.",
    tools: [
      "create_task", "list_tasks", "update_task",
      "decompose_goal", "reflect_on_work", "record_outcome",
    ],
  },
  {
    label: "Agent Queue",
    icon:  "⚙",
    color: "var(--purple)",
    desc:  "Durable priority job queue (0–3), per-provider semaphores (Ollama×3, Anthropic×2, Gemini×5), job chaining with parked/unparked state.",
    tools: [
      "enqueue_jobs", "queue_status",
      "get_job_result", "cancel_job", "retry_job",
      "set_worker_config", "drain_queue",
    ],
  },
  {
    label: "Scheduler",
    icon:  "⏱",
    color: "var(--purple)",
    desc:  "Autonomous 5-min Tokio tick loop. Perception scan auto-creates tasks on failures or overdue notes. Idle sleep mode with bedtime chain.",
    tools: [
      "start_scheduler", "stop_scheduler", "get_scheduler_status",
      "configure_scheduler", "run_scheduler_tick",
    ],
  },
  {
    label: "API",
    icon:  "🔌",
    color: "#fb923c",
    desc:  "OpenAPI ingestion, HTTP execution with automatic credential injection, LLM self-healing on 4xx/5xx, and spec diff/export.",
    tools: [
      "ingest_openapi", "graph_query_endpoint", "execute_http_request",
      "get_api_context", "list_loaded_apis", "clear_api_context",
      "discover_openapi", "build_openapi_from_docs", "build_openapi_from_repo",
      "export_openapi", "diff_api_spec",
      "configure_api_credential", "list_api_credentials", "delete_api_credential",
    ],
  },
  {
    label: "Admin",
    icon:  "🛠",
    color: "var(--cyan)",
    desc:  "Graph maintenance, gzip snapshots, MERGE-safe restore, integrity checks, duplicate purging, and self-structure analysis.",
    tools: [
      "snapshot_knowledge", "restore_knowledge", "list_snapshots",
      "verify_knowledge_integrity", "analyze_own_structure",
      "delete_api", "purge_duplicate_endpoints", "purge_orphaned_schemas",
      "reset_graph", "backfill_endpoint_embeddings",
    ],
  },
  {
    label: "Models",
    icon:  "🤖",
    color: "var(--purple)",
    desc:  "LLM provider/model registry, runtime switching via use_model, capability-based selection, and YAML catalog reload. Usage analytics via the generic duckdb_query tool on the model_usage table.",
    tools: [
      "list_models", "use_model",
      "select_model", "reload_models",
    ],
  },
  {
    label: "Context",
    icon:  "📋",
    color: "var(--cyan)",
    desc:  "YAML context profiles with tool allowlists and system prompts. Boot/init protocols run on startup. Auto-assigns profiles to goals.",
    tools: [
      "list_context_profiles", "get_context_profile",
      "auto_assign_context", "build_agent_context",
    ],
  },
  {
    label: "Working Memory",
    icon:  "📝",
    color: "var(--accent)",
    desc:  "Per-session scratchpad for multi-step tasks. Entries have roles (observation/plan/result/error). LLM summarise to long-term memory.",
    tools: [
      "push_context", "get_context",
      "summarise_session", "list_sessions",
    ],
  },
  {
    label: "Dynamic Tools",
    icon:  "⚡",
    color: "var(--yellow)",
    desc:  "Define new MCP tools at runtime backed by stored procedures. Hot-registered instantly without restart. Template substitution supported.",
    tools: [
      "define_tool", "execute_procedure",
      "list_dynamic_tools", "remove_dynamic_tool",
    ],
  },
  {
    label: "Procedures",
    icon:  "📜",
    color: "var(--accent)",
    desc:  "Named multi-step workflow storage. Steps support {{input.field}}, {{context.steps.N}} substitution and per-step on_failure handling.",
    tools: ["store_procedure", "search_procedures"],
  },
  {
    label: "Search",
    icon:  "🔍",
    color: "#fb923c",
    desc:  "Web search via SerpApi, Brave, or Google Custom Search. Results can be stored as notes for long-term retention.",
    tools: ["search_web"],
  },
  {
    label: "Sleep",
    icon:  "💤",
    color: "var(--red)",
    desc:  "Offline learning from DuckDB telemetry. Exports training data as JSONL and surfaces knowledge gaps for targeted improvement.",
    tools: ["digest_experiences", "analyze_gaps"],
  },
];

// Build name→skill index
const TOOL_TO_SKILL = new Map<string, string>();
for (const s of SKILLS) {
  for (const t of s.tools) TOOL_TO_SKILL.set(t, s.label);
}

function skillFor(name: string): string {
  return TOOL_TO_SKILL.get(name) ?? "Dynamic";
}

// ── Schema view ───────────────────────────────────────────────────────────────

function SchemaView({ schema }: { schema: Record<string, unknown> }) {
  const props = schema.properties as Record<string, { type?: string; description?: string }> | undefined;
  const required = (schema.required as string[]) ?? [];

  if (!props || Object.keys(props).length === 0) {
    return <span className="tool-schema-empty">No parameters</span>;
  }

  return (
    <table className="tool-schema-table">
      <thead>
        <tr><th>Param</th><th>Type</th><th>Description</th></tr>
      </thead>
      <tbody>
        {Object.entries(props).map(([param, def]) => (
          <tr key={param}>
            <td>
              <code className={required.includes(param) ? "required" : "optional"}>{param}</code>
              {required.includes(param) && <span className="req-star">*</span>}
            </td>
            <td><span className="type-tag">{def.type ?? "any"}</span></td>
            <td className="schema-desc">{def.description ?? ""}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

// ── Tool card ─────────────────────────────────────────────────────────────────

function ToolCard({ tool, skillColor, showSkillBadge }: {
  tool: Tool;
  skillColor: string;
  showSkillBadge?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const hasSchema =
    !!tool.inputSchema?.properties &&
    Object.keys(tool.inputSchema.properties as object).length > 0;
  const skill = showSkillBadge ? skillFor(tool.name) : null;
  const def = skill ? SKILLS.find(s => s.label === skill) : null;

  return (
    <div
      className={`tool-card${open ? " tool-card-open" : ""}`}
      style={{ borderLeftColor: open ? skillColor : undefined, borderLeftWidth: open ? 2 : undefined }}
    >
      <div className="tool-card-header" onClick={() => setOpen(v => !v)}>
        <span className="tool-name" style={{ color: skillColor }}>{tool.name}</span>
        {def && (
          <span style={{
            fontSize: 9, fontWeight: 700, padding: "1px 6px", borderRadius: 8,
            background: `${def.color}22`, color: def.color,
            border: `1px solid ${def.color}44`, flexShrink: 0,
            textTransform: "uppercase", letterSpacing: "0.05em",
          }}>{def.label}</span>
        )}
        {hasSchema && <span className="tool-toggle">{open ? "▲" : "▼"}</span>}
      </div>
      {tool.description && <div className="tool-desc">{tool.description}</div>}
      {open && tool.inputSchema && (
        <div className="tool-schema">
          <SchemaView schema={tool.inputSchema} />
        </div>
      )}
    </div>
  );
}

// ── Skill nav item ────────────────────────────────────────────────────────────

function SkillNavItem({ skill, count, active, onClick }: {
  skill: SkillDef; count: number; active: boolean; onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      style={{
        display: "flex", alignItems: "center", gap: 9,
        width: "100%", padding: "8px 14px",
        background: active ? `color-mix(in srgb, ${skill.color} 12%, transparent)` : "none",
        border: "none",
        borderRight: active ? `2px solid ${skill.color}` : "2px solid transparent",
        cursor: "pointer", textAlign: "left", fontFamily: "var(--font)",
        transition: "background 0.12s",
      }}
    >
      <span style={{ fontSize: 13, flexShrink: 0 }}>{skill.icon}</span>
      <span style={{
        flex: 1, fontSize: 11.5, fontWeight: active ? 700 : 400,
        color: active ? skill.color : "var(--text-dim)",
        overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
      }}>{skill.label}</span>
      <span style={{
        fontSize: 10, fontWeight: 600,
        color: active ? skill.color : "var(--text-muted)",
        background: active ? `color-mix(in srgb, ${skill.color} 18%, transparent)` : "var(--bg-card)",
        border: `1px solid ${active ? skill.color + "55" : "var(--border)"}`,
        padding: "1px 6px", borderRadius: 8, flexShrink: 0,
      }}>{count}</span>
    </button>
  );
}

// ── Main panel ────────────────────────────────────────────────────────────────

export default function ToolPanel() {
  const [tools,    setTools]    = useState<Tool[]>([]);
  const [loading,  setLoading]  = useState(false);
  const [error,    setError]    = useState<string | null>(null);
  const [filter,   setFilter]   = useState("");
  const [selected, setSelected] = useState<string>(SKILLS[0].label);

  const fetchTools = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const client = await getMcpClient();
      const result = await client.listTools();
      setTools(result.tools as Tool[]);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchTools(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Build map of skill label → tools from live tool list
  const skillToolMap = new Map<string, Tool[]>();
  for (const s of SKILLS) skillToolMap.set(s.label, []);
  skillToolMap.set("Dynamic", []);
  for (const t of tools) {
    const sk = skillFor(t.name);
    const list = skillToolMap.get(sk) ?? [];
    list.push(t);
    skillToolMap.set(sk, list);
  }

  // Search mode
  const q = filter.trim().toLowerCase();
  const isSearching = q.length > 0;
  const searchResults = isSearching
    ? tools.filter(t =>
        t.name.toLowerCase().includes(q) ||
        (t.description ?? "").toLowerCase().includes(q)
      )
    : [];

  const activeSkillDef = SKILLS.find(s => s.label === selected)
    ?? { label: "Dynamic", icon: "⚡", color: "var(--text-muted)", desc: "Runtime-defined tools.", tools: [] };
  const activeColor = activeSkillDef.color;
  const activeTools = skillToolMap.get(selected) ?? [];

  // Dynamic tools (unknown skill) if any
  const dynamicTools = skillToolMap.get("Dynamic") ?? [];

  const allSkills = [
    ...SKILLS.filter(s => (skillToolMap.get(s.label)?.length ?? 0) > 0),
    ...(dynamicTools.length > 0
      ? [{ label: "Dynamic", icon: "⚡", color: "var(--text-muted)", desc: "Runtime-defined tools not yet in the static registry.", tools: [] }]
      : []),
  ];

  return (
    <div className="panel">

      {/* Header */}
      <div className="panel-header">
        🔧 Tool Explorer
        {tools.length > 0 && <span className="badge">{tools.length} tools</span>}
        {loading && <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 4 }}>loading…</span>}
        <button className="refresh-btn" onClick={fetchTools} title="Refresh">↻</button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      {/* Search bar */}
      <div className="tool-search-bar">
        <input
          placeholder="Search tools across all skills…"
          value={filter}
          onChange={e => setFilter(e.target.value)}
          autoComplete="off"
          spellCheck={false}
        />
        {filter && <button className="btn" onClick={() => setFilter("")}>Clear</button>}
      </div>

      {/* Body: skill nav + tool detail */}
      <div style={{ flex: 1, display: "flex", overflow: "hidden" }}>

        {/* ── Skill nav ── */}
        <div style={{
          width: 180, minWidth: 180, flexShrink: 0,
          borderRight: "1px solid var(--border)",
          background: "var(--bg-panel)",
          overflowY: "auto", display: "flex", flexDirection: "column",
        }}>
          <div style={{
            padding: "8px 14px 6px",
            fontSize: 9, fontWeight: 700, letterSpacing: "0.1em",
            textTransform: "uppercase", color: "var(--text-muted)",
            borderBottom: "1px solid var(--border)",
          }}>
            Skills
          </div>
          {allSkills.map(s => (
            <SkillNavItem
              key={s.label}
              skill={s as SkillDef}
              count={skillToolMap.get(s.label)?.length ?? 0}
              active={!isSearching && selected === s.label}
              onClick={() => { setSelected(s.label); setFilter(""); }}
            />
          ))}
        </div>

        {/* ── Tool area ── */}
        {isSearching ? (
          /* Search results — flat list with skill badges */
          <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>
            <div style={{
              padding: "8px 14px", borderBottom: "1px solid var(--border)",
              fontSize: 10, color: "var(--text-muted)", background: "var(--bg-panel)",
              display: "flex", alignItems: "center", gap: 8,
            }}>
              <span style={{ color: "var(--accent)", fontWeight: 700 }}>{searchResults.length}</span>
              {searchResults.length === 1 ? "match" : "matches"} for
              <span style={{ color: "var(--text)" }}>"{filter}"</span>
              across all skills
            </div>
            <div className="scroll" style={{ padding: "10px 14px", display: "flex", flexDirection: "column", gap: 6 }}>
              {searchResults.length === 0 ? (
                <div className="empty-state" style={{ marginTop: 40 }}>
                  <span className="icon">🔍</span>
                  <span>No tools match "{filter}"</span>
                </div>
              ) : (
                searchResults.map(t => {
                  const sk = skillFor(t.name);
                  const def = SKILLS.find(s => s.label === sk);
                  return (
                    <ToolCard
                      key={t.name}
                      tool={t}
                      skillColor={def?.color ?? "var(--text-muted)"}
                      showSkillBadge
                    />
                  );
                })
              )}
            </div>
          </div>
        ) : (
          /* Skill detail view */
          <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>

            {/* Skill header */}
            <div style={{
              padding: "12px 16px",
              borderBottom: "1px solid var(--border)",
              background: `color-mix(in srgb, ${activeColor} 5%, var(--bg-panel))`,
              flexShrink: 0,
            }}>
              <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 6 }}>
                <span style={{ fontSize: 18 }}>{activeSkillDef.icon}</span>
                <span style={{ fontSize: 14, fontWeight: 700, color: activeColor }}>
                  {activeSkillDef.label}
                </span>
                <span style={{
                  fontSize: 10, fontWeight: 700, padding: "2px 8px", borderRadius: 10,
                  background: `color-mix(in srgb, ${activeColor} 18%, transparent)`,
                  color: activeColor, border: `1px solid ${activeColor}44`,
                }}>
                  {activeTools.length} tools
                </span>
              </div>
              <p style={{ fontSize: 11.5, color: "var(--text-dim)", lineHeight: 1.6, margin: 0 }}>
                {activeSkillDef.desc}
              </p>
            </div>

            {/* Tools list */}
            <div
              className="scroll"
              style={{ padding: "10px 14px", display: "flex", flexDirection: "column", gap: 6 }}
            >
              {activeTools.length === 0 && !loading && (
                <div className="empty-state" style={{ marginTop: 40 }}>
                  <span className="icon">🔌</span>
                  <span>No tools loaded for this skill — check brain connection</span>
                </div>
              )}
              {activeTools.map(t => (
                <ToolCard key={t.name} tool={t} skillColor={activeColor} />
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
