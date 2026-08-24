# Brain Status

<!-- Restored 2026-06-11 from commit 9f7e449 after the automated doc-update
     schedule drifted this file into hallucinated content. Do not regenerate
     this file wholesale; make targeted, commit-grounded edits only. -->

**Build:** passing
**Tool count:** 79 static registered across 20 skills + N runtime (DynamicSkill)
**LLM Providers:** Ollama (local), Ollama Cloud, Anthropic, Gemini
**Last updated:** 2026-08-10

> Tool counts below are read from the live `(:ToolDef)` meta-graph
> (`MATCH (t:ToolDef) RETURN t.skill, count(*)`), which `services/self_model.rs`
> re-derives from the registry on every `build_skills()`. Re-run that query
> rather than incrementing these by hand — the previous figures (70/17) had
> drifted three skills behind the code.

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

## Skill Registry (79 tools static + N runtime)
| Skill | Path | Tools | Notes |
|-------|------|-------|-------|
| CodebaseSkill | `src/skills/codebase.rs` | 14 | Codebase analysis, git logs/diffs, proposals, workspace files |
| DynamicSkill | `src/skills/dynamic.rs` | 8 | Runtime tool definition and procedures |
| KnowledgeSkill | `src/skills/knowledge.rs` | 7 | RAG, reasoning, consolidation, adversarial plan review |
| MediaSkill | `src/skills/media.rs` | 7 | Watch/summarize videos (yt-dlp captions), watchlist, RSS polling, discovery |
| GitSkill | `src/skills/git.rs` | 6 | git status/commit/push/branch/PR + codebase file writes |
| TaskSkill | `src/skills/task.rs` | 5 | Goal tracking, decomposition, outcomes, reflection |
| AgentSkill | `src/skills/agent.rs` | 5 | Background job queue + sequential chaining |
| SchedulerSkill | `src/skills/scheduler.rs` | 4 | Autonomous background scheduler |
| WsSkill | `src/skills/ws.rs` | 4 | WebSocket connection management |
| ModelSkill | `src/skills/model.rs` | 3 | Model registry + selection |
| WorkingMemorySkill | `src/skills/working_memory.rs` | 3 | Session scratchpad and summarisation |
| HttpSkill | `src/skills/http.rs` | 2 | Generic HTTP requests and ApiContext management |
| QuerySkill | `src/skills/query.rs` | 2 | Generic Neo4j (Cypher) and DuckDB (SQL) primitives |
| SearchSkill | `src/skills/search.rs` | 2 | Web search with engine failover ladder (SearXNG → Google → SerpApi → Brave) + usage ledger |
| SleepSkill | `src/skills/sleep.rs` | 2 | Experience digestion and gap analysis |
| ClaimSkill | `src/skills/claims.rs` | 1 | Claim extraction, verification, corroboration tiering |
| ConstructorSkill | `src/skills/constructor.rs` | 1 | Agent construction and reuse |
| ContextSkill | `src/skills/context.rs` | 1 | Context profile management |
| ExecSkill | `src/skills/exec.rs` | 1 | Sandboxed Python execution (`execute_code`) — tool-integrated reasoning |
| ResourceSkill | `src/skills/resource.rs` | 1 | Shared resource/token registry |
| **Total** | | **79** | |

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

### 2026-08-18

- fix: stop three autonomous loops that ran clean and produced nothing (`0d26801`)

### 2026-08-15

- Updated the daily news scheduled task to include the date of the report and to use source URLs from the source it finds the news (`39c5d78`)
- fix: store every timestamp as one type, and stop corrupting multi-line Cypher (`f37fa2e`)
- feat: give the brain a local sense of time (`c788867`)
- feat: tool-integrated reasoning via a sandboxed code executor (`f4c28b9`)

### 2026-08-10

- feat: claim kinds — separate "was asserted" from "is true" (`2af3957`)
- feat: source records and corroboration tiering (`8f20d24`)
- feat: claims — epistemic status for ingested assertions (`a9833f6`)
- refactor: derive self-knowledge in-process, drop the post-commit script (`e6d0db2`)
- feat: search engine failover, curiosity-engine web grounding, and self-knowledge fixes (`89a5de8`)

### 2026-08-05

- feat: Phase 4 — self-hosted Whisper fallback for caption-less media (`0a1a821`)
- feat: enable media watch with 13-channel watchlist + per-source guard (`85c1167`)

### 2026-08-04

- fix: robust caption download + gap-task spawning for Media Learning (`23b16d1`)
- chore: install yt-dlp in the agent-brain image for Media Learning (`26da1b4`)
- feat: media learning — watch & summarize videos to learn and stay current (`65fd8cf`)

## Known Issues / Backlog

### Open

- **SSE push for job results on stdio transport** — stdio path has no session manager; callers must poll `get_job_result`. No lightweight fix without adding an event bus.
- **Rhai scripting in procedure steps** — basic `on_failure` and `{{context.steps.N}}` conditionals added; full Rhai embed for dynamic logic still deferred.

### Fixed (recent)

- ~~Brain believed it was tomorrow for four hours a day~~ — the container ran with no `TZ` and every date came from `Utc::now()`, so from 20:00 America/Detroit onward `{{date}}`, the chat prompt, and every dated note were a day ahead. `TZ` now set in compose and `services/clock.rs` owns local-vs-UTC: display is local, storage stays UTC. Verified in-container (host and container agree) and end-to-end via `/chat`. The prompt also gained a full local instant + zone, `{{now}}`/`{{weekday}}` template vars, and relative note ages on retrieval
- ~~`graph_query_endpoint` CONTAINS fallback~~ — embeddings auto-generated at ingest time
- ~~Parent task stuck `in_progress` after subtasks complete~~ — `update_task` auto-completes parent when all subtasks done
- ~~DynamicSkill load on stdio~~ — `build_skills()` is async; `load_from_neo4j().await` called at startup
- ~~Per-provider semaphores not resizable~~ — `Arc<RwLock<Arc<Semaphore>>>` wrapper; `set_worker_config` swaps inner semaphore atomically
- ~~Auto-snapshot before `prune_old_notes`~~ — `AUTO_SNAPSHOT_BEFORE_PRUNE` env var; hook fires before deletion queries (default false)
- ~~`verify_knowledge_integrity` O(n²) duplicate check~~ — `LIMIT 50` applied in Cypher, truncation warning in response
- ~~`goal_to_steps()` missing failure/web/learn branches~~ — added failure analysis, web research, learn/study, improved default
- ~~Infinite consolidation loop~~ — fixed with `[Memory N]` labels (not `Note N:`), `"recent experiences and knowledge"` topic for auto-tasks, and `next_review_at = now + 30 days` reset after consolidation
- ~~`contexts/` baked into Docker image~~ — `CONTEXTS_DIR=/home/agent/agent-brain/contexts` env var now points to volume-mounted path
