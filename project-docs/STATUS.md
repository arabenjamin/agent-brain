# Brain Status

**Build:** passing
**Tool count:** 81 static registered + N runtime (DynamicSkill)
**LLM Providers:** Ollama, Anthropic, Gemini, vLLM/OpenAI-compat
**Last updated:** 2026-03-05

---

## Architecture Overview

| Layer | Technology | Status |
|-------|-----------|--------|
| Protocol | MCP (JSON-RPC 2.0) via stdio + HTTP/SSE | Live |
| Graph DB | Neo4j via `neo4rs` | Live |
| Vector search | Ollama embeddings (bge-m3, 1024-dim) + BM25 hybrid RRF | Live |
| LLM | Ollama (local) | Live |
| Cloud LLM | Anthropic / Gemini | Live |
| High-perf LLM | vLLM / OpenAI-compat | Live |
| Job queue | Priority BinaryHeap + Neo4j persistence + Tokio coordinator | Live |
| Secret store | Local AES-256-GCM / HashiCorp Vault / AWS Secrets Manager | Live |
| Telemetry | DuckDB (`brain_logs.db`) | Live |
| Chat API | Server-side `/chat` SSE endpoint (Axum) | Live |
| Context profiles | YAML-defined tool allowlists + system prompts (9 profiles) | Live |
| Idle sleep mode | Auto-sleep after N idle ticks; bedtime consolidation chain | Live |

---

## Skill Registry (81 tools static + N runtime)
| Skill | Path | Tools | Notes |
|-------|------|-------|-------|
| HttpSkill | `src/skills/http.rs` | 2 | Generic HTTP requests and ApiContext management |
| KnowledgeSkill | `src/skills/knowledge.rs` | 6 | RAG, reasoning, consolidation, note CRUD |
| TaskSkill | `src/skills/task.rs` | 5 | Goal tracking, decomposition, outcomes, reflection |
| AgentSkill | `src/skills/agent.rs` | 5 | Background job queue + sequential chaining |
| QuerySkill | `src/skills/query.rs` | 2 | Generic Neo4j (Cypher) and DuckDB (SQL) primitives |
| ModelSkill | `src/skills/model.rs` | 2 | Model registry + selection |
| SchedulerSkill | `src/skills/scheduler.rs` | 4 | Autonomous background scheduler |
| ContextSkill | `src/skills/context.rs` | 1 | Context profile management |
| DynamicSkill | `src/skills/dynamic.rs` | 3 | Runtime tool definition and procedures |
| WorkingMemorySkill | `src/skills/working_memory.rs` | 2 | Session scratchpad and summarisation |
| CodebaseSkill | `src/skills/codebase.rs` | 7 | Codebase analysis, git logs/diffs, file reading |
| WsSkill | `src/skills/ws.rs` | 4 | WebSocket connection management |
| ResourceSkill | `src/skills/resource.rs` | 1 | Shared resource/token registry |
| SearchSkill | `src/skills/search.rs` | 1 | Web search integration |
| SleepSkill | `src/skills/sleep.rs` | 2 | Experience digestion and gap analysis |
| **Total** | | **47** | |

| ProcedureSkill | `src/skills/procedure.rs` | 2 | Stored multi-step workflows |
| SleepSkill | `src/skills/sleep.rs` | 2 | Training data export (`digest_experiences`), knowledge gap analysis |
| SearchSkill | `src/skills/search.rs` | 1 | Web search (SerpApi / Brave / Google) |

**KnowledgeSkill tools (16):** `store_note`, `search_notes`, `export_graph_visualization`, `find_related_notes`, `prune_old_notes`, `consolidate_memories`, `list_notes`, `review_due_notes`, `reason`, `audit_action`, `explain_reasoning`, `ask_clarification`, `get_note`, `search_by_entity`, `delete_note`, `update_note`

---

## Context Profiles (9 files in `contexts/`)

| Profile | Purpose |
|---------|---------|
| `general` | Full tool access, no restrictions |
| `knowledge-worker` | Notes, search, memory tools only |
| `task-manager` | Task lifecycle tools |
| `code-analyst` | Code + API tools |
| `api-builder` | API ingestion, query, execution |
| `scheduler` | Scheduler + queue management |
| `researcher` | Search + knowledge synthesis |
| `boot.yaml` | Startup protocol (runs every start) |
| `init.yaml` | Init protocol (runs on empty graph) |

---

## HBI Frontend Panels

| Panel | File | Status |
|-------|------|--------|
| Chat | `chat/ChatPanel.tsx` | Session history sidebar, research mode, context profile selector, export, expandable event bubbles |
| Knowledge | `knowledge/KnowledgePanel.tsx` | Search, note CRUD (create/edit/delete), spaced-rep initial load |
| Tasks | `tasks/TaskPanel.tsx` | Subtask tree view, status filtering |
| Graph | `graph/GraphPanel.tsx` | Live `export_graph_visualization` data, node click → note detail, ResizeObserver sizing |
| Logs | `logs/LogsPanel.tsx` | AgentJob history timeline |
| Architecture | `architecture/ArchitecturePanel.tsx` | Static architecture diagram |
| Settings | `settings/SettingsModal.tsx` | Brain URL + API key (localStorage) |

---

## What Was Built

### Tier 1 Brain Capabilities (all complete)

- **1.1 Memory Consolidation** — `perception_scan()` auto-triggers `consolidate_memories + prune` chain when ≥10 overdue notes or ≥50 episodic notes
- **1.2 Semantic Chunking** — sentence/paragraph-aware splitter (min 200 chars, max 1500 chars); each chunk embedded independently
- **1.3 Richer Entity Extraction** — 7 entity types (person/tool/technology/concept/organisation/url/date); 16-word stopword filter
- **1.4 Multi-Hop Reasoning + Graph Viz** — `entity_expansion` bridges MENTIONS→Entity←MENTIONS; `export_graph_visualization` returns full graph JSON
- **1.5 `get_note` Tool** — direct fetch by UUID, updates access stats
- **1.6 Procedural Control Flow** — `{{context.steps.N}}` positional references; `on_failure: abort|skip|continue` per step

### Idle Sleep Mode

- After `idle_sleep_after_ticks` consecutive idle ticks (default 3 ≈ 15 min), `is_sleeping = true`
- Enqueues low-priority bedtime chain: `consolidate_memories → prune_old_notes → snapshot_knowledge(label="sleep") → store_note`
- Sleep tick interval: `sleep_interval_secs` (default 1800s)
- Wakes immediately on any incoming tool call via `notify_activity()`
- Configurable via `IDLE_SLEEP_AFTER_TICKS` and `SLEEP_INTERVAL_SECS` env vars; runtime via `configure_scheduler`

### Additional Tools Added

- `list_notes` — ordered note listing with optional type filter; used by KnowledgePanel initial load
- `search_by_entity` — find notes by named entity (partial name match, optional type filter)
- `delete_note` — permanent note deletion
- `update_note` — in-place note content update preserving all graph edges

### Tier 2 HBI Frontend (all complete)

- **2.1 Graph container sizing** — ResizeObserver + `useLayoutEffect` in GraphPanel
- **2.2 MCP reconnect** — `callTool` wraps transport errors with `resetMcpClient()` + one retry
- **2.3 Knowledge panel initial load** — `review_due_notes` on mount for meaningful default
- **2.4 Graph node click → note** — `onNodeClick` → `get_note({ id })` → side panel detail view
- **2.5 Task subtask tree view** — `childrenMap` groups by `parent_id`; subtasks indented
- **2.6 Graph from `export_graph_visualization`** — live MCP data; Note + Entity + Task nodes
- **2.7 Auth settings screen** — gear icon modal; Brain URL + API key stored in localStorage
- **2.8 Logs panel** — AgentJob history from `queue_status` + per-job detail polling

---

## Known Issues / Backlog

### Open

- **SSE push for job results on stdio transport** — stdio path has no session manager; callers must poll `get_job_result`. No lightweight fix without adding an event bus.
- **Rhai scripting in procedure steps** — basic `on_failure` and `{{context.steps.N}}` conditionals added; full Rhai embed for dynamic logic still deferred.

### Fixed (recent)

- ~~`graph_query_endpoint` CONTAINS fallback~~ — embeddings auto-generated at ingest time
- ~~Parent task stuck `in_progress` after subtasks complete~~ — `update_task` auto-completes parent when all subtasks done
- ~~DynamicSkill load on stdio~~ — `build_skills()` is async; `load_from_neo4j().await` called at startup
- ~~Per-provider semaphores not resizable~~ — `Arc<RwLock<Arc<Semaphore>>>` wrapper; `set_worker_config` swaps inner semaphore atomically
- ~~Auto-snapshot before `prune_old_notes`~~ — `AUTO_SNAPSHOT_BEFORE_PRUNE` env var; hook fires before deletion queries (default false)
- ~~`verify_knowledge_integrity` O(n²) duplicate check~~ — `LIMIT 50` applied in Cypher, truncation warning in response
- ~~`goal_to_steps()` missing failure/web/learn branches~~ — added failure analysis, web research, learn/study, improved default
- ~~Infinite consolidation loop~~ — fixed with `[Memory N]` labels (not `Note N:`), `"recent experiences and knowledge"` topic for auto-tasks, and `next_review_at = now + 30 days` reset after consolidation
- ~~`contexts/` baked into Docker image~~ — `CONTEXTS_DIR=/home/agent/agent-brain/contexts` env var now points to volume-mounted path
