# Graph Schema & Transport Architecture

## Neo4j Node Types

| Node | Key Properties |
|------|---------------|
| `Resource` | `name` |
| `Endpoint` | `path`, `method`, `summary`, `operationId`, `embedding` |
| `Schema` | `name`, `json_structure` |
| `Parameter` | `name`, `in` (query/path/body/header), `required` |
| `HealingEvent` | `field`, `original`, `corrected`, `reason`, `created_at` |
| `ApiCredential` | `api_name`, `credential_type`, `inject_location`, `inject_key`, `base_url` |
| `Task` | `id`, `goal`, `context`, `status` (created/in_progress/completed/failed/blocked), `created_at` |
| `Note` | `id`, `content`, `note_type`, `embedding`, `access_count`, `last_accessed_at`, `next_review_at`, `review_interval_days`, `source_context`, `event_at` |
| `Procedure` | `id`, `name`, `description`, `steps` (JSON array), `created_at` |
| `WorkingMemory` | `id`, `session_id`, `content`, `role`, `turn_index`, `created_at` |
| `Entity` | `id`, `name` (unique, lowercased), `entity_type`, `created_at` |
| `DynamicTool` | `id`, `name` (unique), `description`, `input_schema` (JSON), `created_at` |
| `AgentJob` | `id`, `tool_name`, `args_json`, `priority` (0-3), `status` (queued/running/completed/failed/dead/parked/cancelled), `attempt_count`, `max_attempts`, `result_json`, `error`, timestamps, `session_id`, `parent_job_id` |
| `ModelSpec` | `id`, `name`, `provider`, `cost_per_1k_input`, `cost_per_1k_output`, `context_window`, `capabilities` (JSON array) |

**Note types:** `semantic`, `episodic`, `reflection`, `consolidated`, `outcome`, `inference`

## Neo4j Relationships

| Relationship | From → To | Properties |
|-------------|-----------|------------|
| `HAS_ENDPOINT` | Resource → Endpoint | — |
| `REQUIRES_PARAM` | Endpoint → Parameter | — |
| `RETURNS_SCHEMA` | Endpoint → Schema | `status: 200` |
| `ACCEPTS_SCHEMA` | Endpoint → Schema | — |
| `LINKS_TO` | Schema → Schema | — |
| `HAS_HISTORY` | Endpoint → HealingEvent | — |
| `RELATES_TO` | Note → Note | `similarity: float` (auto-created ≥ 0.75) |
| `SUMMARIZED_BY` | Note → Note | — (source → consolidated) |
| `REFLECTS_ON` | Note → Task | — |
| `PART_OF` | Note → Note | — (chunk → parent) |
| `MENTIONS` | Note → Entity | `count: int` |
| `DERIVED_FROM` | Note → Note | — (inference → sources) |
| `SUBTASK_OF` | Task → Task | — |
| `DEPENDS_ON` | Task → Task | — |
| `USES` | DynamicTool → Procedure | — |

## Transport Architecture

```
                         CLI (main.rs)
                              │
               ┌──────────────┴──────────────┐
               │                             │
     ┌─────────▼─────────┐         ┌─────────▼─────────┐
     │  StdioTransport   │         │   HttpTransport   │
     │    (stdio)        │         │   (Axum + SSE)    │
     └─────────┬─────────┘         └─────────┬─────────┘
               │                             │
               │                   ┌─────────▼─────────┐
               │                   │  SessionManager   │
               │                   │ (Mcp-Session-Id)  │
               │                   └─────────┬─────────┘
               │                             │
     ┌─────────▼─────────────────────────────▼─────────┐
     │            McpTransport Trait                   │
     └─────────────────────┬───────────────────────────┘
                           │
     ┌─────────────────────▼───────────────────────────┐
     │              McpServerCore                      │
     │    (Arc<RwLock<ServerState>> for thread-safe)  │
     └─────────────────────┬───────────────────────────┘
                           │
     ┌─────────────────────▼─────────────────────────────────┐
     │    Skill Registry (70 static + N runtime tools)       │
     │  ApiSkill(14)  KnowledgeSkill(15)  TaskSkill(6)       │
     │  AgentSkill(8)  AdminSkill(9)  ModelSkill(5)          │
     │  SchedulerSkill(5)  WorkingMemorySkill(4)             │
     │  ProcedureSkill(2)  DynamicSkill(4+N)                 │
     │  SearchSkill(1)  SleepSkill(2)                        │
     └───────────────────────────────────────────────────────┘
```

### HTTP Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/mcp` | JSON-RPC requests |
| `GET` | `/mcp` | SSE stream for server-initiated messages |
| `DELETE` | `/mcp` | Terminate session |
| `POST` | `/chat` | Conversational SSE endpoint |
| `GET` | `/health` | Health check |

**Session headers:** `Mcp-Protocol-Version`, `Mcp-Session-Id`

### HTTP Session Initialization (Critical)
1. `POST /mcp` with `initialize` method → get `session_id`
2. `POST /mcp` with `notifications/initialized` (no `id` field) + session_id header
3. Only now are tool calls accepted. Without step 2, server stays `Initializing` and rejects requests.

### SSE Push Notifications
Job completions pushed as `event: agent_job` / `data: {"jsonrpc":"2.0","method":"notifications/agent_job","params":{...}}`.
