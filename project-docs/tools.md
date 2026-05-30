# MCP Tools Reference

Complete reference for all static MCP tools exposed by Agent Brain. Tool count: **61 static** (+ N runtime tools registered via `manage_dynamic_tool`). Workspace tools (`write_workspace_file`, `list_workspace_files`) require `WORKSPACE_DIR` env var.

Skills and their tools are registered in `crates/app/src/brain_core.rs` → `build_skills()`. Some skills are conditional on configured services (Neo4j, QueueService, DuckDB).

---

## AgentSkill — Background Job Queue

`crates/app/src/skills/agent.rs` · requires QueueService

| Tool | Description |
|------|-------------|
| `enqueue_jobs` | Enqueue one or more tool-call jobs, optionally as a chained sequence. Args: `jobs[]` (tool_name, arguments, priority, max_attempts, provider_hint), `chain: bool`. |
| `manage_job` | Cancel, retry, or cleanup a job by ID. Args: `job_id`, `action: cancel\|retry\|cleanup`. |
| `dead_letter` | List or requeue dead-letter jobs (status=dead). Args: `action: list\|requeue`, `limit`. |
| `set_worker_config` | Update queue concurrency limits and enabled state at runtime. Args: `max_concurrent_ollama`, `max_concurrent_anthropic`, `max_concurrent_gemini`, `enabled`. |
| `update_job_progress` | Report progress on a running job (percent + message). Args: `job_id`, `percent`, `message`. |

---

## CodebaseSkill — Self-Analysis and File Access

`crates/app/src/skills/codebase.rs` · requires CODEBASE_DIR

| Tool | Description |
|------|-------------|
| `read_codebase_file` | Read a file by path relative to codebase root. Args: `path`, `max_lines` (default 500), `prepend_context` (text prepended before file content — useful in chains). |
| `list_codebase_files` | List files in the codebase. Args: `directory`, `pattern`, `max_results`. |
| `search_codebase` | Regex search across source files. Args: `query`, `file_pattern`, `context_lines`, `case_sensitive`, `max_results`. |
| `get_file_tree` | Directory tree view. Args: `directory`, `max_depth`. |
| `get_git_log` | Recent commit history. Args: `n` (default 10, max 50), `path`. |
| `get_git_diff` | Diff between two refs. Args: `from_ref` (required), `to_ref` (default HEAD), `path`. |
| `write_codebase_doc` | Write or overwrite a `.md` file in the codebase (only Markdown allowed). Args: `path`, `content`. |
| `write_proposal` | Stage a fix proposal to `PROPOSALS_DIR` for human review. Args: `title`, `task_id`, `diagnosis`, `proposed_fix`, `severity`, `affected_file`, `code_snippet`. |
| `list_proposals` | List pending proposals. Args: `include_applied`. |
| `read_proposal` | Read a proposal by filename. Args: `filename`. |
| `dismiss_proposal` | Move a proposal to `proposals/applied/`. Args: `filename`, `reason: applied\|rejected\|obsolete`. |
| `analyze_own_structure` | Generate a full codebase overview (tree + skills + git log). Args: `store_as_note`. |
| `write_workspace_file` | *(requires WORKSPACE_DIR)* Write a file to the writable workspace. Args: `path`, `content`, `mode: overwrite\|append`. |
| `list_workspace_files` | *(requires WORKSPACE_DIR)* List workspace files. Args: `directory`, `pattern`, `max_results`. |

---

## ContextSkill — Context Profile Management

`crates/app/src/skills/context.rs` · requires ContextBuilderService

| Tool | Description |
|------|-------------|
| `context` | Multi-action context tool. `action: list` → all profiles; `action: get` → profile details; `action: auto_assign` → best profile for a goal; `action: build` → build agent context bundle. |

---

## DynamicSkill — Runtime Tool Builder

`crates/app/src/skills/dynamic.rs` · requires Neo4j

| Tool | Description |
|------|-------------|
| `manage_dynamic_tool` | Define or remove a runtime MCP tool. Args: `action: define\|remove`, `name`, `description`, `input_schema`. |
| `store_procedure` | Store a named multi-step procedure in Neo4j. Args: `name`, `description`, `steps[]`. |
| `execute_procedure` | Execute a stored procedure by name with input substitution. Args: `name`, `input`. |

---

## GitSkill — Git Operations

`crates/app/src/skills/git.rs` · requires CODEBASE_DIR

| Tool | Description |
|------|-------------|
| `git_status` | Show working tree status. No args. |
| `git_create_branch` | Create and checkout a new branch. Args: `branch_name`, `from` (default HEAD). |
| `git_commit` | Stage and commit changes. Args: `message`, `paths[]` (optional, defaults to all). |
| `git_push` | Push current branch to origin. Args: `remote` (default origin), `force`. |
| `git_create_pr` | Create a GitHub pull request via `gh`. Args: `title`, `body`, `base` (default main). |
| `write_codebase_file` | Write any file in the codebase (unrestricted). Args: `path`, `content`. Intended for automated code generation; human review recommended before committing. |

---

## HttpSkill — Generic HTTP Requests

`crates/app/src/skills/http.rs`

| Tool | Description |
|------|-------------|
| `http_request` | Make an HTTP request, optionally using a named ApiContext for auth injection. Args: `method`, `url`, `headers`, `body`, `context_name`, `timeout_secs`. |
| `define_api_context` | Create or update an ApiContext node. Args: `name`, `base_url`, `auth_scheme`, `auth_param`, `auth_env_var`, `default_headers`, `description`. |

---

## KnowledgeSkill — Memory and Reasoning

`crates/app/src/skills/knowledge.rs` · requires Neo4j

| Tool | Description |
|------|-------------|
| `store_note` | Store a note with optional embedding. Args: `content`, `note_type`, `source_context`, `event_at`. |
| `search_notes` | Hybrid vector+BM25 RAG search. Args: `query`, `limit`, `note_type`, `graph_hops`, `entity_expansion`. |
| `reason` | Ask the LLM a question with optional context. Args: `question`, `context`, `store_inference`. |
| `consolidate_memories` | LLM-synthesise episodic notes into semantic memory. Args: `topic`, `limit`. |
| `prune_old_notes` | Delete stale episodic notes past a retention window. Args: `older_than_days`, `note_type`, `dry_run`. |
| `synthesize_knowledge` | Cross-domain synthesis — pull notes on a theme and produce a structured insight note. Args: `theme`, `limit`. |
| `adversarial_plan_review` | Stress-test a plan with N failure scenarios. Args: `goal`, `plan_description`, `n_hypotheses`, `min_robustness`. |

---

## ModelSkill — LLM Registry

`crates/app/src/skills/model.rs`

| Tool | Description |
|------|-------------|
| `use_model` | Switch the active LLM provider and model at runtime. Args: `provider`, `model`, `base_url`, `api_key`. |
| `reload_models` | Reload the model catalog from `models.yaml` into DuckDB. No args. |

---

## QuerySkill — Raw Database Access

`crates/app/src/skills/query.rs` · requires Neo4j + DuckDB

| Tool | Description |
|------|-------------|
| `neo4j_query` | Execute arbitrary Cypher against Neo4j. Args: `cypher`. |
| `duckdb_query` | Execute arbitrary SQL against DuckDB telemetry. Args: `sql`. |

---

## ResourceSkill — Shared Resource Registry

`crates/app/src/skills/resource.rs`

| Tool | Description |
|------|-------------|
| `resource` | Multi-action resource registry. `action: register\|get\|list\|release`. Stores named tokens or connection handles shared across tool calls. |

---

## SchedulerSkill — Autonomous Scheduler

`crates/app/src/skills/scheduler.rs` · requires Neo4j + QueueService

| Tool | Description |
|------|-------------|
| `scheduler_control` | Enable/disable, get state, or update config. Args: `action: enable\|disable\|status\|update_config`, plus `interval_secs`, `idle_sleep_after_ticks`, `sleep_interval_secs`. |
| `run_scheduler_tick` | Force an immediate scheduler tick (bypasses interval). No args. |
| `manage_chain` | Create, update, or delete a SchedulerChain node. Args: `action: create\|update\|delete\|list`, `name`, `pattern`, `patterns[]`, `steps[]`, `priority`, `no_evaluator`, `no_adversarial`, `evaluation_rubric`. |
| `manage_scheduled_task` | Create, update, enable/disable, or delete a ScheduledTask. Args: `action: create\|update\|delete\|list\|enable\|disable`, `name`, `description`, `interval_seconds`, `steps[]`. |

---

## SearchSkill — Web Search

`crates/app/src/skills/search.rs`

| Tool | Description |
|------|-------------|
| `search_web` | Search the web via SerpApi, Brave, or Google CSE. Args: `query`, `count`, `source_list`. Requires at least one API key (`SERPAPI_KEY`, `BRAVE_API_KEY`, or `GOOGLE_API_KEY` + `GOOGLE_CX`). |

---

## SleepSkill — Experience Digestion

`crates/app/src/skills/sleep.rs` · requires DuckDB telemetry + DATASET_DIR

| Tool | Description |
|------|-------------|
| `digest_experiences` | Export interaction log to JSONL training dataset. Args: `limit`, `since`. |
| `analyze_gaps` | Query knowledge_gaps table; surface unanswered queries for self-study. Args: `limit`. |

---

## TaskSkill — Goal Tracking

`crates/app/src/skills/task.rs` · requires Neo4j

| Tool | Description |
|------|-------------|
| `create_task` | Create a new Task node. Args: `goal`, `context`, `success_criteria`. |
| `update_task` | Update task status or metadata. Args: `task_id`, `status`, `context`. |
| `decompose_goal` | Break a task into sub-tasks with SUBTASK_OF edges and optional DEPENDS_ON ordering. Args: `task_id`, `subtasks[]`. |
| `record_outcome` | Record a success/failure outcome note linked to a task. Args: `task_id`, `outcome`, `details`. |
| `reflect_on_work` | LLM reflection on current state vs. success criteria; outputs `Score: N/5`. Args: `task_id`, `current_state`, `success_criteria`, `min_score`. |
| `store_note` | Convenience alias for KnowledgeSkill's `store_note` (same underlying call). |

---

## WorkingMemorySkill — Session Scratchpad

`crates/app/src/skills/working_memory.rs` · requires Neo4j

| Tool | Description |
|------|-------------|
| `summarise_session` | Summarise the current session's working memory into a semantic note. Args: `session_id`. |
| `notify_user` | Send a message to a session (SSE push to HTTP clients). Args: `message`, `context`, `related_session_id`. |
| `push_context` | Seed a session's working memory with a message. Args: `session_id`, `content`, `role`. |

---

## WsSkill — WebSocket Connections

`crates/app/src/skills/ws.rs`

| Tool | Description |
|------|-------------|
| `ws_connect` | Open a WebSocket connection and store it by ID. Args: `url`, `connection_id`. |
| `ws_send` | Send a text message on an open connection. Args: `connection_id`, `message`. |
| `ws_receive` | Receive the next message (with timeout). Args: `connection_id`, `timeout_ms` (default 5000). |
| `ws_close` | Close a WebSocket connection. Args: `connection_id`. |
