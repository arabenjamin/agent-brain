import { useCallback, useEffect, useState } from "react";
import { getMcpClient } from "../../api/mcp";
import { getBrainUrl, getApiKey } from "../../api/config";

// ── Types ─────────────────────────────────────────────────────────────────────

interface Tool {
  name: string;
  description?: string;
  inputSchema?: Record<string, unknown>;
}

interface LiveSkill {
  name: string;
  tools: string[];
}

// ── Static UI metadata keyed by skill name ────────────────────────────────────
// Icons, colours, and descriptions are frontend-only concerns.
// Tool lists come from /api/skills at runtime — never hardcoded here.

interface SkillMeta {
  icon:  string;
  color: string;
  desc:  string;
}

const SKILL_META: Record<string, SkillMeta> = {
  knowledge: {
    icon:  "🧠",
    color: "var(--green)",
    desc:  "Notes, hybrid RAG search (BM25+semantic+entity), memory consolidation, and multi-mode LLM reasoning.",
  },
  task: {
    icon:  "🎯",
    color: "var(--cyan)",
    desc:  "Goal tracking, LLM-powered decomposition into subtasks, dependency edges, outcome recording, and reflection.",
  },
  agent: {
    icon:  "⚙",
    color: "var(--purple)",
    desc:  "Durable priority job queue (0–3), per-provider semaphores, job chaining with parked/unparked state.",
  },
  scheduler: {
    icon:  "⏱",
    color: "var(--purple)",
    desc:  "Autonomous Tokio tick loop. Perception scan auto-creates tasks. Idle sleep mode with bedtime chain.",
  },
  http: {
    icon:  "🔌",
    color: "#fb923c",
    desc:  "Generic HTTP requests with automatic ApiContext credential injection and LLM self-healing.",
  },
  codebase: {
    icon:  "💻",
    color: "var(--accent)",
    desc:  "Read files, search code, browse git log/diff, and manage codebase improvement proposals.",
  },
  model: {
    icon:  "🤖",
    color: "var(--purple)",
    desc:  "LLM provider/model registry, runtime switching, and YAML catalog reload.",
  },
  context: {
    icon:  "📋",
    color: "var(--cyan)",
    desc:  "YAML context profiles with tool allowlists and system prompts. Boot/init protocols on startup.",
  },
  working_memory: {
    icon:  "📝",
    color: "var(--accent)",
    desc:  "Per-session scratchpad for multi-step tasks. Roles: observation/plan/result/error. LLM summarise to long-term.",
  },
  dynamic: {
    icon:  "⚡",
    color: "var(--yellow)",
    desc:  "Define new MCP tools at runtime backed by stored procedures. Hot-registered without restart.",
  },
  procedure: {
    icon:  "📜",
    color: "var(--accent)",
    desc:  "Named multi-step workflow storage with template substitution and per-step on_failure handling.",
  },
  search: {
    icon:  "🔍",
    color: "#fb923c",
    desc:  "Web search via SerpApi, Brave, or Google Custom Search. Results storable as long-term notes.",
  },
  sleep: {
    icon:  "💤",
    color: "var(--red)",
    desc:  "Offline learning from DuckDB telemetry. Exports training data and surfaces knowledge gaps.",
  },
  query: {
    icon:  "🗄",
    color: "var(--green)",
    desc:  "Raw Cypher against Neo4j and raw SQL against DuckDB telemetry.",
  },
  resource: {
    icon:  "🔗",
    color: "var(--cyan)",
    desc:  "Named resource registry for cross-agent connection pooling and shared state.",
  },
  ws: {
    icon:  "📡",
    color: "#fb923c",
    desc:  "Live WebSocket connections — connect, send, receive, and close.",
  },
};

const UNKNOWN_META: SkillMeta = {
  icon:  "🔧",
  color: "var(--text-muted)",
  desc:  "Runtime-registered skill.",
};

function metaFor(skillName: string): SkillMeta {
  return SKILL_META[skillName.toLowerCase()] ?? UNKNOWN_META;
}

// ── Fetch helpers ─────────────────────────────────────────────────────────────

async function fetchLiveSkills(): Promise<LiveSkill[]> {
  const url = `${getBrainUrl()}/api/skills`;
  const res = await fetch(url, {
    headers: { Authorization: `Bearer ${getApiKey()}` },
  });
  if (!res.ok) throw new Error(`GET /api/skills → ${res.status}`);
  const data = await res.json();
  return (data.skills ?? []) as LiveSkill[];
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

function ToolCard({ tool, skillColor, skillLabel, showSkillBadge }: {
  tool: Tool;
  skillColor: string;
  skillLabel?: string;
  showSkillBadge?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const hasSchema =
    !!tool.inputSchema?.properties &&
    Object.keys(tool.inputSchema.properties as object).length > 0;

  return (
    <div
      className={`tool-card${open ? " tool-card-open" : ""}`}
      style={{ borderLeftColor: open ? skillColor : undefined, borderLeftWidth: open ? 2 : undefined }}
    >
      <div className="tool-card-header" onClick={() => setOpen(v => !v)}>
        <span className="tool-name" style={{ color: skillColor }}>{tool.name}</span>
        {showSkillBadge && skillLabel && (
          <span style={{
            fontSize: 9, fontWeight: 700, padding: "1px 6px", borderRadius: 8,
            background: `${skillColor}22`, color: skillColor,
            border: `1px solid ${skillColor}44`, flexShrink: 0,
            textTransform: "uppercase", letterSpacing: "0.05em",
          }}>{skillLabel}</span>
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

function SkillNavItem({ name, meta, count, active, onClick }: {
  name: string; meta: SkillMeta; count: number; active: boolean; onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      style={{
        display: "flex", alignItems: "center", gap: 9,
        width: "100%", padding: "8px 14px",
        background: active ? `color-mix(in srgb, ${meta.color} 12%, transparent)` : "none",
        border: "none",
        borderRight: active ? `2px solid ${meta.color}` : "2px solid transparent",
        cursor: "pointer", textAlign: "left", fontFamily: "var(--font)",
        transition: "background 0.12s",
      }}
    >
      <span style={{ fontSize: 13, flexShrink: 0 }}>{meta.icon}</span>
      <span style={{
        flex: 1, fontSize: 11.5, fontWeight: active ? 700 : 400,
        color: active ? meta.color : "var(--text-dim)",
        overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
      }}>{name}</span>
      <span style={{
        fontSize: 10, fontWeight: 600,
        color: active ? meta.color : "var(--text-muted)",
        background: active ? `color-mix(in srgb, ${meta.color} 18%, transparent)` : "var(--bg-card)",
        border: `1px solid ${active ? meta.color + "55" : "var(--border)"}`,
        padding: "1px 6px", borderRadius: 8, flexShrink: 0,
      }}>{count}</span>
    </button>
  );
}

// ── Main panel ────────────────────────────────────────────────────────────────

export default function ToolPanel() {
  const [tools,    setTools]    = useState<Tool[]>([]);
  const [skills,   setSkills]   = useState<LiveSkill[]>([]);
  const [loading,  setLoading]  = useState(false);
  const [error,    setError]    = useState<string | null>(null);
  const [filter,   setFilter]   = useState("");
  const [selected, setSelected] = useState<string>("");

  const fetchAll = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [mcpClient, liveSkills] = await Promise.all([
        getMcpClient(),
        fetchLiveSkills(),
      ]);
      const result = await mcpClient.listTools();
      setTools(result.tools as Tool[]);
      setSkills(liveSkills);
      if (liveSkills.length > 0) {
        setSelected(prev => prev || liveSkills[0].name);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchAll(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Build tool-name → skill-name index from live registry
  const toolToSkill = new Map<string, string>();
  for (const sk of skills) {
    for (const t of sk.tools) toolToSkill.set(t, sk.name);
  }

  // Build skill → Tool[] map using live MCP tool list
  const skillToolMap = new Map<string, Tool[]>();
  for (const sk of skills) skillToolMap.set(sk.name, []);
  const unknownTools: Tool[] = [];
  for (const t of tools) {
    const skName = toolToSkill.get(t.name);
    if (skName) {
      skillToolMap.get(skName)!.push(t);
    } else {
      unknownTools.push(t);
    }
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

  const activeSkill   = skills.find(s => s.name === selected);
  const activeMeta    = activeSkill ? metaFor(activeSkill.name) : UNKNOWN_META;
  const activeTools   = skillToolMap.get(selected) ?? [];

  return (
    <div className="panel">

      {/* Header */}
      <div className="panel-header">
        🔧 Tool Explorer
        {tools.length > 0 && <span className="badge">{tools.length} tools</span>}
        {loading && <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 4 }}>loading…</span>}
        <button className="refresh-btn" onClick={fetchAll} title="Refresh">↻</button>
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
          {skills.map(sk => (
            <SkillNavItem
              key={sk.name}
              name={sk.name}
              meta={metaFor(sk.name)}
              count={skillToolMap.get(sk.name)?.length ?? 0}
              active={!isSearching && selected === sk.name}
              onClick={() => { setSelected(sk.name); setFilter(""); }}
            />
          ))}
          {unknownTools.length > 0 && (
            <SkillNavItem
              name="(unregistered)"
              meta={{ icon: "❓", color: "var(--text-muted)", desc: "Tools present in MCP list but not in any registered skill." }}
              count={unknownTools.length}
              active={!isSearching && selected === "(unregistered)"}
              onClick={() => { setSelected("(unregistered)"); setFilter(""); }}
            />
          )}
        </div>

        {/* ── Tool area ── */}
        {isSearching ? (
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
                  const skName = toolToSkill.get(t.name) ?? "(unregistered)";
                  const m = metaFor(skName);
                  return (
                    <ToolCard
                      key={t.name}
                      tool={t}
                      skillColor={m.color}
                      skillLabel={skName}
                      showSkillBadge
                    />
                  );
                })
              )}
            </div>
          </div>
        ) : (
          <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>
            {/* Skill header */}
            <div style={{
              padding: "12px 16px",
              borderBottom: "1px solid var(--border)",
              background: `color-mix(in srgb, ${activeMeta.color} 5%, var(--bg-panel))`,
              flexShrink: 0,
            }}>
              <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 6 }}>
                <span style={{ fontSize: 18 }}>{activeMeta.icon}</span>
                <span style={{ fontSize: 14, fontWeight: 700, color: activeMeta.color }}>
                  {selected}
                </span>
                <span style={{
                  fontSize: 10, fontWeight: 700, padding: "2px 8px", borderRadius: 10,
                  background: `color-mix(in srgb, ${activeMeta.color} 18%, transparent)`,
                  color: activeMeta.color, border: `1px solid ${activeMeta.color}44`,
                }}>
                  {activeTools.length} tools
                </span>
              </div>
              <p style={{ fontSize: 11.5, color: "var(--text-dim)", lineHeight: 1.6, margin: 0 }}>
                {activeMeta.desc}
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
                <ToolCard key={t.name} tool={t} skillColor={activeMeta.color} />
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
