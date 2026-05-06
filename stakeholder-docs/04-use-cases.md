# Agent Brain — Use Case Examples

## Use Case 1: AI Research Assistant with Persistent Memory

**Scenario:** A developer uses Claude Desktop with Agent Brain as an MCP server.
Every research session builds on previous ones — no more re-explaining context.

```mermaid
sequenceDiagram
    participant Dev as Developer
    participant Claude as Claude Desktop
    participant Brain as Agent Brain
    participant DB as Neo4j

    Dev->>Claude: "Research Rust async runtimes\nand save the key findings"

    Claude->>Brain: search_notes("Rust async runtimes")
    Brain-->>Claude: 3 existing notes from previous sessions

    Claude->>Brain: search_web("Rust async runtime comparison 2025")
    Brain-->>Claude: search results

    Claude->>Brain: store_note(content="Tokio vs async-std...",\ntags=["rust","async","comparison"],\nnote_type="semantic")
    Brain-->>Claude: note stored + 2 auto-linked related notes

    Claude->>Brain: store_note(content="Key finding: Tokio dominates\nproduction use cases...",\nnote_type="reflection")
    Brain-->>Claude: stored

    Claude-->>Dev: "Here are my findings...\n(based on 5 notes including 2 from past sessions)"

    Note over Dev,DB: Next session — months later

    Dev->>Claude: "What do we know about async Rust?"
    Claude->>Brain: search_notes("async Rust")
    Brain-->>Claude: all historical notes + consolidated summaries
    Claude-->>Dev: "Based on your past research..."
```

---

## Use Case 2: Automated Code Review Pipeline

**Scenario:** Agent Brain runs as a background service. When a developer pushes code,
a task is created and the scheduler automatically reviews it and stores findings.

```mermaid
flowchart TD
    PUSH[Developer pushes\ncode changes]
    PUSH --> CREATE[create_task\ngoal: 'Review PR #42 for security issues'\nsuccess_criteria: 'All OWASP top-10 patterns checked'\ncontext: 'Changed files: auth.rs, api.rs']

    CREATE --> SCHED[Scheduler picks up task\nnext tick]

    SCHED --> CHAIN[Job Chain created]
    CHAIN --> J1[Job 1: analyze_own_structure\nread changed files]
    J1 --> J2[Job 2: search_notes\n'security vulnerabilities Rust']
    J2 --> J3[Job 3: store_note\nfindings from analysis]
    J3 --> J4[Job 4: reflect_on_work\nevaluator step]

    J4 --> SCORE{Score ≥ 3.5?}
    SCORE -->|Yes| DONE[Task completed\nFindings in knowledge graph]
    SCORE -->|No| RETRY[New task created\nwith critique injected]
    RETRY --> SCHED

    DONE --> NOTIFY[Brain emits\nJobCompleted event]
    NOTIFY --> CLIENT[Connected clients\nnotified via SSE]

    style CREATE fill:#4a90d9,color:#fff
    style DONE fill:#2e8b57,color:#fff
    style RETRY fill:#cd5c5c,color:#fff
```

---

## Use Case 3: Knowledge Base Builder for an Engineering Team

**Scenario:** A team uses Agent Brain as a shared knowledge repository. It ingests
documentation, meeting notes, and decisions over time and makes them all searchable.

```mermaid
graph TB
    subgraph Inputs["Knowledge Sources"]
        DOCS[Technical Docs\nMarkdown / PDFs]
        MEET[Meeting Notes\nPlain text]
        DEC[Architecture Decisions\nRFC documents]
        CODE[Code Comments\nand inline docs]
    end

    subgraph Ingestion["Ingestion via store_note"]
        S1[store_note\nnote_type=semantic\ntags=architecture]
        S2[store_note\nnote_type=episodic\ntags=meeting,Q1-2026]
        S3[store_note\nnote_type=semantic\ntags=decision,auth]
        S4[store_note\nnote_type=semantic\ntags=code,rust]
    end

    subgraph Graph["Knowledge Graph (Neo4j)"]
        N1[Note: Service Mesh Design]
        N2[Note: Q1 Planning Meeting]
        N3[Note: Use JWT for auth]
        N4[Note: async Rust patterns]
        E1[Entity: Kubernetes]
        E2[Entity: JWT]
        N1 -->|RELATES_TO 0.82| N4
        N2 -->|RELATES_TO 0.71| N1
        N3 -->|RELATES_TO 0.89| N2
        N1 -->|MENTIONS| E1
        N3 -->|MENTIONS| E2
    end

    subgraph Query["Natural Language Retrieval"]
        Q1["'Why did we choose JWT?'"]
        Q2["'What's our Kubernetes strategy?'"]
        Q3["'What was decided in Q1?'"]
    end

    DOCS --> S1
    MEET --> S2
    DEC --> S3
    CODE --> S4
    S1 --> N1
    S2 --> N2
    S3 --> N3
    S4 --> N4

    Q1 -->|hybrid search + reasoning| N3
    Q2 -->|entity hop via Kubernetes| N1
    Q3 -->|time-filtered episodic search| N2
```

---

## Use Case 4: Autonomous Task Agent (No Human Prompting)

**Scenario:** Agent Brain runs fully autonomously. You give it goals; it works through
them on its own schedule, evaluates its own output, and improves on failure.

```mermaid
gantt
    title Autonomous Agent — 24 Hour Timeline
    dateFormat HH:mm
    axisFormat %H:%M

    section Task Dispatch
    Scan for pending tasks       :active, 00:00, 5m
    Dispatch 3 research tasks    :active, 00:05, 10m
    Scan (idle)                  :05:00, 5m
    Scan (idle)                  :10:00, 5m
    Scan — enters sleep mode     :15:00, 5m

    section Background Work
    Research Task 1 (job chain)  :00:05, 45m
    Research Task 2 (job chain)  :00:10, 60m
    Evaluator — Task 2 fails     :01:10, 5m
    Re-dispatch Task 2 improved  :06:00, 45m
    Research Task 3 (job chain)  :00:15, 30m

    section Sleep Cycle
    consolidate_memories         :15:05, 15m
    prune_stale_notes            :15:20, 10m
    knowledge_snapshot           :15:30, 5m

    section Next Cycle
    Scan — new tasks available   :18:00, 5m
    Dispatch new work            :18:05, 30m
```

---

## Use Case 5: Multi-Step Procedure Execution

**Scenario:** A recurring workflow (e.g., daily standup prep) is stored as a Procedure
and can be triggered by name.

```mermaid
flowchart TD
    STORE["store_procedure(\n  name='daily-standup-prep',\n  steps=(\n    search_notes yesterday's work,\n    search_notes blockers,\n    search_notes planned tasks,\n    store_note summary\n  )\n)"]

    TRIGGER[run_procedure\n'daily-standup-prep'\nargs={date: '2026-05-01'}]
    TRIGGER --> S1["Step 1: search_notes\ncompleted work on date-1"]
    S1 --> S2[Step 2: search_notes\nblockers issues problems]
    S2 --> S3[Step 3: list_tasks\nstatus=in_progress]
    S3 --> S4["Step 4: store_note\nStandup summary\nnote_type=episodic"]
    S4 --> DONE[Standup prep note\nstored in knowledge graph]

    STORE -.->|defines| TRIGGER

    style STORE fill:#4a90d9,color:#fff
    style DONE fill:#2e8b57,color:#fff
```

---

## Use Case 6: LLM-Backed Chat with Persistent Context

**Scenario:** A web application embeds the `/chat` SSE endpoint.
The conversation is grounded in the knowledge graph automatically.

```mermaid
sequenceDiagram
    participant Web as Web App
    participant Chat as ChatService /chat SSE
    participant Brain as Agent Brain
    participant DB as Neo4j
    participant LLM as LLM Provider

    Web->>Chat: POST /chat {message: "Explain our auth design"}
    Chat->>Brain: search_notes("auth design")
    Brain->>DB: hybrid vector+BM25 search
    DB-->>Brain: top-5 relevant notes
    Brain-->>Chat: context notes

    Chat->>Brain: get_working_memory(session_id)
    DB-->>Chat: previous conversation turns

    Chat->>LLM: system_prompt + context_notes\n+ conversation_history + user_message
    LLM-->>Chat: streaming response tokens

    Chat-->>Web: SSE stream: token by token
    Chat->>Brain: set_working_memory(session_id,\nnew turn)
    Brain->>DB: persist conversation turn

    Note over Web,DB: Context automatically preserved\nacross page refreshes and sessions
```

---

## Decision Matrix: When to Use Which Skill

```mermaid
flowchart TD
    NEED{What do you need?}

    NEED -->|Store information| KNOW[KnowledgeSkill\nstore_note / search_notes]
    NEED -->|Track a goal| TASK[TaskSkill\ncreate_task / decompose_goal]
    NEED -->|Run background work| AGENT[AgentSkill\nenqueue_jobs]
    NEED -->|Automate recurring work| PROC[ProcedureSkill\nstore_procedure / run_procedure]
    NEED -->|Search the internet| SEARCH[SearchSkill\nsearch_web]
    NEED -->|Call an external API| HTTP[HttpSkill\nhttp_request]
    NEED -->|Analyze code| CODE[CodebaseSkill\nread_file / search_code]
    NEED -->|Create a new tool| DYN[DynamicSkill\ndefine_tool]
    NEED -->|Session scratch notes| WM[WorkingMemorySkill\nset/get_working_memory]
    NEED -->|Reason over everything| MULTI[KnowledgeSkill\nreason_over_knowledge\nmulti_hop_query]

    style KNOW fill:#2e8b57,color:#fff
    style TASK fill:#4a90d9,color:#fff
    style AGENT fill:#cd853f,color:#fff
    style MULTI fill:#9370db,color:#fff
```
