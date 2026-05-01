# Agent Brain — Skill & Tool Reference

## Tool Count Summary

| Skill | Tools | Purpose |
|-------|-------|---------|
| KnowledgeSkill | 16 | Store, search, reason over, and maintain long-term memory |
| CodebaseSkill | 7 | Read, search, and analyze source code |
| AgentSkill | 5 | Manage the background job queue |
| TaskSkill | 5 | Create and track goals with measurable outcomes |
| HttpSkill | 2 | Make HTTP requests with credential injection |
| ModelSkill | 2 | Manage and query the LLM model registry |
| SchedulerSkill | 4 | Control the autonomous scheduler loop |
| ContextSkill | 1 | Switch active context profiles |
| DynamicSkill | 3 | Define and manage runtime tools |
| WorkingMemorySkill | 2 | Session-scoped scratchpad storage |
| ProcedureSkill | 2 | Store and run multi-step workflows |
| SearchSkill | 1 | Web search via SerpAPI/Brave/Google |
| SleepSkill | 2 | Experience digestion and training data export |
| WsSkill | 4 | WebSocket connections |
| ResourceSkill | 1 | MCP resource exposure |
| **Dynamic tools** | N | Runtime-defined by AI or operator |
| **Total static** | **81+** | |

---

## Knowledge Skill — Tool Map

```mermaid
mindmap
  root((KnowledgeSkill\n16 tools))
    Write
      store_note\ncontent + tags + type
      update_note\npartial update
      delete_note\nby id
    Read
      search_notes\nhybrid vector+BM25
      get_note\nby id
      list_notes\nfiltered
      semantic_search\nvector-only cosine
    Reasoning
      reason_over_knowledge\nLLM synthesis
      multi_hop_query\ngraph traversal
      extract_entities\nNER from text
    Maintenance
      consolidate_memories\nLLM summarization
      prune_stale_notes\nremove old notes
      knowledge_snapshot\ncurrent state summary
    Reflection
      reflect_on_work\nevaluator step
      analyze_own_structure\nself-introspection
      spaced_rep_review\ndue notes
```

---

## Task Lifecycle

```mermaid
stateDiagram-v2
    [*] --> created : create_task()
    created --> in_progress : scheduler dispatches\nor manual update
    in_progress --> completed : success_criteria met\nor manual completion
    in_progress --> failed : evaluator score < min_score\nor error
    failed --> created : new task with critique\n(auto-retry via scheduler)
    in_progress --> blocked : depends_on task not done
    blocked --> in_progress : dependency resolves
    completed --> [*]
```

**Task Fields:**
| Field | Required | Description |
|-------|----------|-------------|
| `goal` | Yes | Plain-language objective |
| `context` | No | Constraints, background, critique from previous attempts |
| `success_criteria` | No | Measurable definition of done — triggers evaluator loop when set |
| `status` | Auto | created / in_progress / completed / failed / blocked |

---

## AgentJob Priority Queue

```mermaid
graph TB
    subgraph Queue["In-Memory BinaryHeap"]
        P0["Priority 0 — Critical\n🔴 Immediate execution"]
        P1["Priority 1 — High\n🟠 User-initiated tasks"]
        P2["Priority 2 — Normal\n🟡 Scheduler dispatches"]
        P3["Priority 3 — Low\n🟢 Background maintenance"]
    end

    subgraph Concurrency["Per-Provider Semaphores"]
        GL["Global: max 5\nconcurrent jobs"]
        OL["Ollama: max 3"]
        AL["Anthropic: max 2"]
        GL2["Gemini: max 5"]
    end

    P0 --> GL
    P1 --> GL
    P2 --> GL
    P3 --> GL
    GL --> OL
    GL --> AL
    GL --> GL2
```

---

## Context Profile Comparison

| Profile | Key Tools Allowed | System Prompt Focus | Best For |
|---------|------------------|---------------------|----------|
| `general` | All tools | Balanced assistant | Default use |
| `knowledge-worker` | Knowledge + Working Memory | Memory-first reasoning | Research sessions |
| `task-manager` | Task + Agent + Scheduler | Goal decomposition | Project management |
| `code-analyst` | Codebase + Knowledge | Code review + analysis | Dev workflows |
| `api-builder` | HTTP + Dynamic + Procedure | API integration | Building integrations |
| `researcher` | Search + Knowledge + Reasoning | Information synthesis | Fact-finding |
| `scheduler` | All tools | Full autonomy | Unattended operation |

---

## LLM Provider Configuration

```mermaid
graph LR
    subgraph Config["Environment Variables"]
        OM[OLLAMA_MODEL\ndefault: qwen3.5:4b]
        OLM[OLLAMA_LOCAL_MODEL\ndefault: gemma4:latest\nused by scheduler]
        OEM[OLLAMA_EMBED_MODEL\ndefault: bge-m3:latest\nembeddings only]
        CLP[CHAT_LLM_PROVIDER\noverride for /chat endpoint]
        CLM[CHAT_LLM_MODEL\noverride for /chat endpoint]
    end

    subgraph Routing["Request Routing"]
        BRAIN_LLM[Brain LLM\nall skill tool calls]
        CHAT_LLM[Chat LLM\n/chat SSE sessions]
        EMBED_LLM[Embed LLM\nvector generation]
        SCHED_LLM[Scheduler LLM\nalways local Ollama]
    end

    OM --> BRAIN_LLM
    OLM --> SCHED_LLM
    OEM --> EMBED_LLM
    CLP --> CHAT_LLM
    CLM --> CHAT_LLM
```

---

## Deployment Architecture Options

```mermaid
graph TB
    subgraph Option1["Option A: Local Dev (stdio)"]
        DEV_CLIENT[Claude Desktop\nor Cursor]
        DEV_BRAIN[Agent Brain process\nstdio MCP]
        DEV_NEO[Neo4j Docker :7688]
        DEV_OLL[Ollama :11434]
        DEV_CLIENT -->|spawn| DEV_BRAIN
        DEV_BRAIN --> DEV_NEO
        DEV_BRAIN --> DEV_OLL
    end

    subgraph Option2["Option B: Docker Compose (HTTP)"]
        PROD_CLIENT[Any HTTP Client\nor AI Frontend]
        PROD_BRAIN[Agent Brain container\nHTTP :3000]
        PROD_NEO[Neo4j container :7688]
        PROD_OLL[Ollama container :11434]
        PROD_CLIENT -->|Bearer auth| PROD_BRAIN
        PROD_BRAIN --> PROD_NEO
        PROD_BRAIN --> PROD_OLL
    end

    subgraph Option3["Option C: Cloud (external LLM)"]
        CLOUD_CLIENT[Web App / API Consumer]
        CLOUD_BRAIN[Agent Brain container\nHTTP :3000]
        CLOUD_NEO[Neo4j Aura\nor self-hosted]
        CLOUD_ANT[Anthropic API\nor Gemini API]
        CLOUD_CLIENT -->|HTTPS| CLOUD_BRAIN
        CLOUD_BRAIN --> CLOUD_NEO
        CLOUD_BRAIN -->|API key| CLOUD_ANT
    end
```
