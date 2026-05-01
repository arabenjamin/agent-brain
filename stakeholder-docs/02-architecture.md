# Agent Brain — Architecture Deep Dive

## Layered Architecture

Agent Brain is organized as a **four-crate Cargo workspace** with strict dependency layering.
Each layer only depends on layers below it, making every component independently testable
and replaceable.

```mermaid
block-beta
  columns 1
  block:transport["Transport Layer (stdio / HTTP+SSE)"]
    A["McpServerCore\n(MCP JSON-RPC adapter)"]
    B["ChatService\n(/chat SSE endpoint)"]
  end
  space
  block:skills["Skill Layer (~81 static tools + N runtime)"]
    C["KnowledgeSkill\n16 tools"]
    D["TaskSkill\n5 tools"]
    E["AgentSkill\n5 tools"]
    F["SchedulerSkill\n4 tools"]
    G["CodebaseSkill\n7 tools"]
    H["...11 more skills"]
  end
  space
  block:services["Services Layer"]
    I["LlmClient\nOllama/Anthropic/Gemini"]
    J["KnowledgeService\nRAG + embeddings"]
    K["QueueService\nPriority job queue"]
    L["SchedulerService\nAutonomous loop"]
    M["ContextBuilderService\nYAML profiles"]
    N["SecretsService\nAES-GCM/Vault/AWS"]
  end
  space
  block:repo["Repository Layer"]
    O["Neo4jClient\nGraph DB CRUD"]
    P["TelemetryClient\nDuckDB logging"]
  end
  space
  block:proto["Protocol / Models Layer"]
    Q["agent-brain-protocol\nMCP types + Skill trait"]
    R["agent-brain-models\nData types (serde)"]
  end

  transport --> skills
  skills --> services
  services --> repo
  repo --> proto
```

---

## Component Interaction Map

```mermaid
graph TD
    CLI[CLI Entry Point\nmain.rs]
    CLI --> STDIO[StdioTransport]
    CLI --> HTTP[HttpTransport\nAxum + SSE]

    STDIO --> MCPCORE
    HTTP --> MCPCORE

    MCPCORE[McpServerCore\nJSON-RPC state machine\nsession management]
    MCPCORE --> BC[BrainCore\nbrain_core.rs]
    MCPCORE --> CHAT[ChatService\nclients/chat.rs]

    BC --> SKILLS[Skill Registry\nArc RwLock Vec Skill]
    BC --> SCHED[SchedulerService]
    BC --> QUEUE[QueueService]
    BC --> NEO[Neo4jClient]
    BC --> DUCK[TelemetryClient]
    BC --> LLM[LlmClient]

    SCHED -->|dispatches chains| QUEUE
    QUEUE -->|executes tools| SKILLS
    QUEUE -->|persists jobs| NEO

    SKILLS -->|knowledge ops| NEO
    SKILLS -->|LLM inference| LLM
    SKILLS -->|telemetry| DUCK

    LLM -->|local inference| OLLAMA[Ollama]
    LLM -->|cloud inference| ANTHROPIC[Anthropic API]
    LLM -->|cloud inference| GEMINI[Gemini API]

    CHAT -->|own LLM config| LLM

    style BC fill:#4a90d9,color:#fff
    style MCPCORE fill:#7b68ee,color:#fff
    style SKILLS fill:#2e8b57,color:#fff
    style QUEUE fill:#cd853f,color:#fff
    style SCHED fill:#cd853f,color:#fff
```

---

## Neo4j Knowledge Graph Schema

The graph database is the **single source of truth** for all persistent state.

```mermaid
erDiagram
    Note {
        string id PK
        string content
        float[] embedding
        string note_type
        string[] tags
        int access_count
        datetime last_accessed_at
        datetime next_review_at
        float review_interval_days
        string source_context
        datetime event_at
    }
    Entity {
        string id PK
        string name
        string entity_type
    }
    Task {
        string id PK
        string goal
        string context
        string success_criteria
        string status
    }
    AgentJob {
        string id PK
        string tool_name
        string args_json
        int priority
        string status
        int attempt_count
        int max_attempts
        string result_json
        string error
        string session_id
        string parent_job_id
    }
    Procedure {
        string id PK
        string name
        string description
        json steps
    }
    WorkingMemory {
        string id PK
        string session_id
        string content
        string role
        int turn_index
    }
    DynamicTool {
        string id PK
        string name
        string description
        json input_schema
    }

    Note ||--o{ Note : "RELATES_TO (similarity)"
    Note ||--o| Note : "SUMMARIZED_BY"
    Note ||--o{ Entity : "MENTIONS (count)"
    Note ||--o{ Task : "REFLECTS_ON"
    Note ||--o{ Note : "PART_OF (chunks)"
    Note ||--o{ Note : "DERIVED_FROM (inferences)"
    Task ||--o{ Task : "SUBTASK_OF"
    Task ||--o{ Task : "DEPENDS_ON"
    AgentJob ||--o{ AgentJob : "parent_job_id (chain)"
    DynamicTool ||--o| Procedure : "USES"
```

---

## LLM Provider Routing

```mermaid
flowchart LR
    Tool[Tool Call\nrequiring LLM]
    Tool --> Check{provider_hint\nin job args?}
    Check -->|yes: ollama| LOCAL[OLLAMA_LOCAL_URL\nlocal model only\nnever cloud quota]
    Check -->|yes: anthropic| ANT[Anthropic API\nper-provider semaphore\nmax 2 concurrent]
    Check -->|yes: gemini| GEM[Gemini API\nmax 5 concurrent]
    Check -->|no hint| DEFAULT[Default Provider\nfrom OLLAMA_MODEL env\nor CHAT_LLM_PROVIDER]
    LOCAL --> RESP[LLM Response]
    ANT --> RESP
    GEM --> RESP
    DEFAULT --> RESP

    subgraph Scheduler["Background Scheduler Jobs"]
        SCHED_JOB[Scheduled Job] -->|always local| LOCAL
    end
```

---

## Startup Initialization Sequence

```mermaid
sequenceDiagram
    participant CLI as CLI / main.rs
    participant BC as BrainCore
    participant DB as Neo4j
    participant LLM as LLM Provider
    participant SCHED as Scheduler

    CLI->>BC: initialize()
    BC->>DB: connect + verify schema
    BC->>DB: recover crashed jobs → queued
    BC->>BC: build_skills() — register 81+ tools
    BC->>LLM: health check
    BC->>SCHED: spawn background loop
    BC->>DB: run boot.yaml protocol
    Note over BC,DB: scheduler_control(status)\naudit scheduled task steps

    alt Graph is empty (first run)
        BC->>DB: run init.yaml protocol
        Note over BC,DB: seed self-knowledge notes\ncreate initial tasks
    end

    BC-->>CLI: ready — accepting tool calls
```

---

## Context Profiles

Context profiles are YAML files that let operators configure **which tools are available**
and **what system prompt** is used for a given persona or use case.

```mermaid
graph LR
    subgraph Profiles["contexts/ directory"]
        BOOT[boot.yaml\nRuns every startup]
        INIT[init.yaml\nRuns on empty graph]
        GEN[general.yaml\nDefault persona]
        KW[knowledge-worker.yaml\nMemory-focused]
        TM[task-manager.yaml\nGoal-focused]
        CA[code-analyst.yaml\nCodebase tools]
        AB[api-builder.yaml\nHTTP + dynamic tools]
        RES[researcher.yaml\nSearch + reasoning]
        SCH[scheduler.yaml\nFull autonomy]
    end

    SELECT{Active Profile} --> GEN
    SELECT --> KW
    SELECT --> TM
    SELECT --> CA
    SELECT --> AB
    SELECT --> RES
    SELECT --> SCH

    GEN -->|tool allowlist + system prompt| BRAIN[Agent Brain\nFiltered Tool Set]
    KW --> BRAIN
    TM --> BRAIN
```

Each profile defines:
- `allowed_tools` — allowlist of tool names the LLM can see
- `system_prompt` — persona/task framing text
- `token_budget` — optional context window constraint
