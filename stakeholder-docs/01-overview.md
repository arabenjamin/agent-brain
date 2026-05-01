# Agent Brain — Stakeholder Overview

## What Is Agent Brain?

Agent Brain is a **persistent, autonomous AI memory and reasoning engine** built as a
[Model Context Protocol (MCP)](https://modelcontextprotocol.io) server. It gives any MCP-compatible
AI assistant (Claude, Cursor, custom agents) a durable long-term memory, a goal-tracking system,
a self-improving background scheduler, and a pluggable multi-provider LLM backend — all backed by a
Neo4j knowledge graph.

Think of it as the **cognitive infrastructure layer** that sits behind an AI assistant and makes it
capable of remembering, planning, reasoning, and improving over time.

---

## Core Value Propositions

| Capability | What It Means |
|------------|---------------|
| **Persistent Memory** | Notes, facts, and experiences survive across sessions. No more "cold start" amnesia. |
| **Semantic + Keyword Search** | Hybrid vector (cosine) + BM25 retrieval surfaces the right knowledge even with imprecise queries. |
| **Goal Tracking** | Tasks with measurable success criteria. Sub-tasks, dependencies, automatic re-dispatch on failure. |
| **Background Autonomy** | A scheduler continuously works through pending tasks without human prompting. |
| **Self-Improvement** | After idle periods the brain consolidates memories, prunes stale notes, and reflects on its own performance. |
| **Multi-Provider LLM** | Ollama (local), Anthropic Claude, Google Gemini, or any OpenAI-compatible endpoint. Switch per workload. |
| **Pluggable Tools** | 81+ built-in tools across 15 skill domains. New tools can be defined at runtime via natural language. |

---

## System at a Glance

```mermaid
graph TB
    subgraph Clients["AI Clients / Frontends"]
        CC[Claude Desktop / Cursor]
        HBI[HBI Web Frontend]
        API[Custom API Consumers]
    end

    subgraph Transport["Transport Layer"]
        STDIO[Stdio Transport\nMCP default]
        HTTP[HTTP + SSE Transport\nAxum server]
    end

    subgraph Brain["Agent Brain Core"]
        MCP[MCP Server Core\nJSON-RPC 2.0]
        BC[Brain Core\nSkill Registry + Engine]
        SCHED[Autonomous Scheduler\nSelf-Improvement Loop]
        QUEUE[Priority Job Queue\nDurable Background Worker]
        CHAT[Chat Service\n/chat SSE endpoint]
    end

    subgraph Storage["Persistent Storage"]
        NEO[Neo4j Graph DB\nMemory · Tasks · Jobs]
        DUCK[DuckDB\nTelemetry · Model Registry]
    end

    subgraph LLM["LLM Providers"]
        OLL[Ollama Local]
        ANT[Anthropic Claude]
        GEM[Google Gemini]
    end

    CC -->|MCP JSON-RPC| STDIO
    HBI -->|HTTP / SSE| HTTP
    API -->|HTTP / SSE| HTTP
    STDIO --> MCP
    HTTP --> MCP
    MCP --> BC
    BC --> SCHED
    BC --> QUEUE
    BC --> CHAT
    QUEUE --> NEO
    BC --> NEO
    BC --> DUCK
    BC --> OLL
    BC --> ANT
    BC --> GEM
    SCHED --> QUEUE
```

---

## The Skill Domains

Agent Brain exposes **81+ tools** organized into 15 skill domains that any connected AI client can call.

```mermaid
mindmap
  root((Agent Brain\n81+ Tools))
    Memory
      store_note
      search_notes
      semantic_search
      consolidate_memories
      reflect_on_work
    Tasks
      create_task
      decompose_goal
      list_tasks
      update_task_status
      link_dependency
    Background Jobs
      enqueue_jobs
      manage_job
      dead_letter
      set_worker_config
    Scheduler
      scheduler_control
      run_scheduler_tick
      manage_chain
      manage_scheduled_task
    Knowledge
      extract_entities
      reason_over_knowledge
      multi_hop_query
      knowledge_snapshot
    Working Memory
      set_working_memory
      get_working_memory
    Dynamic Tools
      define_tool
      list_dynamic_tools
      delete_tool
    Procedures
      store_procedure
      run_procedure
    Web Search
      search_web
    Models
      list_models
      reload_models
    Codebase
      analyze_own_structure
      read_file
      search_code
    HTTP
      http_request
      configure_credential
    Context Profiles
      switch_context
    Sleep
      digest_experiences
    Telemetry
      query_logs
```

---

## Who Uses It?

| User / System | How They Interact | What They Get |
|---------------|------------------|---------------|
| **AI Assistant** (Claude, GPT, etc.) | MCP tool calls | Persistent memory, task management, background work |
| **Developer** | HTTP REST / SSE | Direct API access, custom integrations |
| **HBI Frontend** | WebSocket + SSE | Visual dashboard: chat, knowledge graph, task board |
| **Autonomous Scheduler** | Internal job queue | Self-directed task execution without human prompting |
| **Other MCP Servers** | Chained MCP calls | Composable agent networks |

---

## Deployment Options

```mermaid
graph LR
    subgraph Local["Local Development"]
        D1[Docker Compose\nNeo4j + Ollama + Brain]
    end
    subgraph Cloud["Cloud / Production"]
        D2[Docker Container\nHTTP Transport + API Key Auth]
        D3[Kubernetes\nStateless Brain + External Neo4j]
    end
    subgraph Desktop["Desktop Integration"]
        D4[Claude Desktop\nMCP stdio transport]
    end

    Local -->|cargo run| Brain1[MCP stdio]
    Cloud -->|docker compose up| Brain2[MCP HTTP :3000]
    Desktop -->|spawn subprocess| Brain3[MCP stdio]
```

**Minimum Requirements**
- Rust 2024 edition toolchain
- Neo4j 5.x (graph storage)
- One LLM provider: Ollama (local, free) or an Anthropic/Gemini API key
