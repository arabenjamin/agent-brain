import { useCallback, useEffect, useState } from "react";
import { getMcpClient } from "../../api/mcp";

// ── Types ─────────────────────────────────────────────────────────────────────

interface Tool {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
}

// ── Skill group detection ──────────────────────────────────────────────────────

const SKILL_PREFIXES: [string, string][] = [
  ["ingest_openapi",              "API"],
  ["graph_query_endpoint",        "API"],
  ["execute_http_request",        "API"],
  ["get_api_context",             "API"],
  ["list_loaded_apis",            "API"],
  ["clear_api_context",           "API"],
  ["discover_openapi",            "API"],
  ["build_openapi_from_docs",     "API"],
  ["build_openapi_from_repo",     "API"],
  ["export_openapi",              "API"],
  ["diff_api_spec",               "API"],
  ["configure_api_credential",    "API"],
  ["list_api_credentials",        "API"],
  ["delete_api_credential",       "API"],
  ["search_web",                  "Search"],
  ["create_task",                 "Tasks"],
  ["reflect_on_work",             "Tasks"],
  ["decompose_goal",              "Tasks"],
  ["update_task",                 "Tasks"],
  ["list_tasks",                  "Tasks"],
  ["record_outcome",              "Tasks"],
  ["store_note",                  "Knowledge"],
  ["search_notes",                "Knowledge"],
  ["find_related_notes",          "Knowledge"],
  ["prune_old_notes",             "Knowledge"],
  ["consolidate_memories",        "Knowledge"],
  ["review_due_notes",            "Knowledge"],
  ["search_by_entity",            "Knowledge"],
  ["reason",                      "Knowledge"],
  ["audit_action",                "Knowledge"],
  ["explain_reasoning",           "Knowledge"],
  ["ask_clarification",           "Knowledge"],
  ["store_procedure",             "Procedures"],
  ["search_procedures",           "Procedures"],
  ["push_context",                "Working Memory"],
  ["get_context",                 "Working Memory"],
  ["summarise_session",           "Working Memory"],
  ["list_sessions",               "Working Memory"],
  ["define_tool",                 "Dynamic Tools"],
  ["execute_procedure",           "Dynamic Tools"],
  ["list_dynamic_tools",          "Dynamic Tools"],
  ["remove_dynamic_tool",         "Dynamic Tools"],
  ["enqueue_agent",               "Agent Queue"],
  ["queue_status",                "Agent Queue"],
  ["get_job_result",              "Agent Queue"],
  ["cancel_job",                  "Agent Queue"],
  ["retry_job",                   "Agent Queue"],
  ["set_worker_config",           "Agent Queue"],
  ["drain_queue",                 "Agent Queue"],
  ["enqueue_chain",               "Agent Queue"],
  ["delete_api",                  "Admin"],
  ["purge_duplicate_endpoints",   "Admin"],
  ["purge_orphaned_schemas",      "Admin"],
  ["reset_graph",                 "Admin"],
  ["backfill_endpoint_embeddings","Admin"],
  ["list_models",                 "Models"],
  ["use_model",                   "Models"],
  ["register_model",              "Models"],
  ["select_model",                "Models"],
  ["get_model_stats",             "Models"],
  ["digest_experiences",          "Sleep"],
  ["analyze_gaps",                "Sleep"],
  ["start_scheduler",             "Scheduler"],
  ["stop_scheduler",              "Scheduler"],
  ["get_scheduler_status",        "Scheduler"],
  ["configure_scheduler",         "Scheduler"],
  ["run_scheduler_tick",          "Scheduler"],
];

function skillFor(name: string): string {
  const entry = SKILL_PREFIXES.find(([n]) => n === name);
  return entry ? entry[1] : "Dynamic";
}

const SKILL_ORDER = [
  "Knowledge", "Tasks", "Agent Queue", "API", "Search",
  "Procedures", "Working Memory", "Dynamic Tools", "Models",
  "Admin", "Sleep", "Scheduler", "Dynamic",
];

function groupTools(tools: Tool[]): Map<string, Tool[]> {
  const map = new Map<string, Tool[]>();
  for (const t of tools) {
    const skill = skillFor(t.name);
    const list = map.get(skill) ?? [];
    list.push(t);
    map.set(skill, list);
  }
  // Sort by canonical skill order
  const sorted = new Map<string, Tool[]>();
  for (const skill of SKILL_ORDER) {
    if (map.has(skill)) sorted.set(skill, map.get(skill)!);
  }
  // Append any remaining (unknown) groups
  for (const [k, v] of map) {
    if (!sorted.has(k)) sorted.set(k, v);
  }
  return sorted;
}

// ── Schema display ─────────────────────────────────────────────────────────────

function SchemaView({ schema }: { schema: Record<string, unknown> }) {
  const props = schema.properties as Record<string, { type?: string; description?: string }> | undefined;
  const required = (schema.required as string[]) ?? [];

  if (!props || Object.keys(props).length === 0) {
    return <span className="tool-schema-empty">No parameters</span>;
  }

  return (
    <table className="tool-schema-table">
      <thead>
        <tr>
          <th>Param</th>
          <th>Type</th>
          <th>Description</th>
        </tr>
      </thead>
      <tbody>
        {Object.entries(props).map(([param, def]) => (
          <tr key={param}>
            <td>
              <code className={required.includes(param) ? "required" : "optional"}>
                {param}
              </code>
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

// ── Tool card ──────────────────────────────────────────────────────────────────

function ToolCard({ tool }: { tool: Tool }) {
  const [open, setOpen] = useState(false);
  const hasSchema =
    !!tool.inputSchema?.properties &&
    Object.keys(tool.inputSchema.properties as object).length > 0;

  return (
    <div className={`tool-card${open ? " tool-card-open" : ""}`}>
      <div className="tool-card-header" onClick={() => setOpen((v) => !v)}>
        <span className="tool-name">{tool.name}</span>
        {hasSchema && (
          <span className="tool-toggle">{open ? "▲" : "▼"}</span>
        )}
      </div>
      {tool.description && (
        <div className="tool-desc">{tool.description}</div>
      )}
      {open && tool.inputSchema && (
        <div className="tool-schema">
          <SchemaView schema={tool.inputSchema} />
        </div>
      )}
    </div>
  );
}

// ── Main component ─────────────────────────────────────────────────────────────

export default function ToolPanel() {
  const [tools, setTools] = useState<Tool[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

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

  useEffect(() => {
    fetchTools();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const q = filter.trim().toLowerCase();
  const filtered = q
    ? tools.filter(
        (t) =>
          t.name.toLowerCase().includes(q) ||
          (t.description ?? "").toLowerCase().includes(q)
      )
    : tools;

  const groups = groupTools(filtered);

  return (
    <div className="panel">
      <div className="panel-header">
        🔧 Tool Explorer
        {tools.length > 0 && (
          <span className="badge">{tools.length} tools</span>
        )}
        {loading && (
          <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 8 }}>
            loading…
          </span>
        )}
        <button className="refresh-btn" onClick={fetchTools} title="Refresh">↻</button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div className="tool-search-bar">
        <input
          placeholder="Filter tools…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        {filter && (
          <button className="btn" onClick={() => setFilter("")}>
            Clear
          </button>
        )}
      </div>

      <div className="tool-list scroll">
        {tools.length === 0 && !loading && (
          <div className="empty-state" style={{ marginTop: 60 }}>
            <span className="icon">🔧</span>
            <span>No tools loaded — check brain connection</span>
          </div>
        )}

        {[...groups.entries()].map(([skill, skillTools]) => (
          <div key={skill} className="tool-group">
            <div className="tool-group-header">
              {skill}
              <span className="tool-group-count">{skillTools.length}</span>
            </div>
            <div className="tool-group-body">
              {skillTools.map((t) => (
                <ToolCard key={t.name} tool={t} />
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
