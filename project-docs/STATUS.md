# Brain Status

<!-- Restored 2026-06-11 from commit 9f7e449 after the automated doc-update
     schedule drifted this file into hallucinated content. Do not regenerate
     this file wholesale; make targeted, commit-grounded edits only. -->

**Build:** passing
**Tool count:** 70 static registered across 17 skills + N runtime (DynamicSkill)
**LLM Providers:** Ollama (local), Ollama Cloud, Anthropic, Gemini
**Last updated:** 2026-06-11

---

## Architecture Overview

| Layer | Technology | Status |
|-------|-----------|--------|
| Protocol | MCP (JSON-RPC 2.0) via stdio + HTTP/SSE | Live |
| Graph DB | Neo4j via `neo4rs` | Live |
| Vector search | Ollama embeddings (bge-m3, 1024-dim) + BM25 hybrid RRF | Live |
| LLM | Ollama (local) | Live |
| Cloud LLM | Ollama Cloud / Anthropic / Gemini | Live |
| Job queue | Priority BinaryHeap + Neo4j persistence + Tokio coordinator | Live |
| Secret store | Local AES-256-GCM / HashiCorp Vault / AWS Secrets Manager | Live |
| Telemetry | DuckDB (`brain_logs.db`) | Live |
| Chat API | Server-side `/chat` SSE endpoint (Axum) | Live |
| Context profiles | YAML-defined tool allowlists + system prompts (9 profiles) | Live |
| Idle sleep mode | Auto-sleep after N idle ticks; bedtime consolidation chain | Live |

---

## Skill Registry (70 tools static + N runtime)
| Skill | Path | Tools | Notes |
|-------|------|-------|-------|
| HttpSkill | `src/skills/http.rs` | 2 | Generic HTTP requests and ApiContext management |
| KnowledgeSkill | `src/skills/knowledge.rs` | 7 | RAG, reasoning, consolidation, adversarial plan review |
| TaskSkill | `src/skills/task.rs` | 7 | Goal tracking, decomposition, outcomes, reflection |
| AgentSkill | `src/skills/agent.rs` | 5 | Background job queue + sequential chaining |
| QuerySkill | `src/skills/query.rs` | 2 | Generic Neo4j (Cypher) and DuckDB (SQL) primitives |
| ModelSkill | `src/skills/model.rs` | 2 | Model registry + selection |
| SchedulerSkill | `src/skills/scheduler.rs` | 4 | Autonomous background scheduler |
| ContextSkill | `src/skills/context.rs` | 1 | Context profile management |
| DynamicSkill | `src/skills/dynamic.rs` | 3 | Runtime tool definition and procedures |
| WorkingMemorySkill | `src/skills/working_memory.rs` | 3 | Session scratchpad and summarisation |
| CodebaseSkill | `src/skills/codebase.rs` | 14 | Codebase analysis, git logs/diffs, proposals, workspace files |
| GitSkill | `src/skills/git.rs` | 6 | git status/commit/push/branch/PR + codebase file writes |
| WsSkill | `src/skills/ws.rs` | 4 | WebSocket connection management |
| ResourceSkill | `src/skills/resource.rs` | 1 | Shared resource/token registry |
| SearchSkill | `src/skills/search.rs` | 1 | Web search integration |
| MediaSkill | `src/skills/media.rs` | 6 | Watch/summarize videos (yt-dlp captions), channel watchlist, autonomous RSS polling |
| SleepSkill | `src/skills/sleep.rs` | 2 | Experience digestion and gap analysis |
| **Total** | | **70** | |

**KnowledgeSkill tools (7):** `store_note`, `search_notes`, `prune_old_notes`, `consolidate_memories`, `reason`, `synthesize_knowledge`, `adversarial_plan_review`

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

## Recent Changes (auto)

### 2026-08-03

- chore: auth timing, enqueue round trips, drain completeness, dead code (`9e7f198`)
- fix: cascade chain cancellation to all descendants (`2280639`)
- fix: skip meta-learning on transient infra errors; daily dedup (`9e87409`)
- fix: sanitize Lucene queries; surface hybrid-search failures (`191c54d`)
- fix: per-provider skip and permit-release wakeup in coordinator (`cd06ba3`)
- fix: guard job finalizers against cancel race (`9f81892`)
- fix: treat unparseable evaluator output as explicit non-score (`d2e6f25`)
- perf: unique constraints and indexes for Note/Task/AgentJob lookups (`27a465b`)
- fix: value-level template substitution in chain loading (`4284186`)
- chore: baseline in-progress work before code-review fixes (`3c32d52`)

### 2026-06-12

- fix: ground constructor plans in tool argument schemas (`fe77e9f`)
- feat: constructor learning loop — Phase 3 of the Agent Constructor plan (`778e884`)
- feat: construct_agent tool — Phase 2 of the Agent Constructor plan (`939ff7c`)
- fix: learn observed cloud-model availability from failures; fall back on 403 (`3ac0596`)
- feat: per-step model routing within cloud tiers — Phase 1 of Agent Constructor (`489d8f5`)

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
