# Agent Brain — Core Workflows

## 1. Memory Storage & Retrieval

The most fundamental workflow: storing a piece of knowledge and later retrieving it
with semantic understanding.

```mermaid
sequenceDiagram
    participant Client as AI Client
    participant Brain as Agent Brain
    participant LLM as LLM Provider
    participant DB as Neo4j

    Client->>Brain: store_note(content, tags, note_type)

    Brain->>LLM: embed(content) → 1024-dim vector
    Brain->>DB: MERGE Note with embedding + metadata

    alt content > 1500 chars
        Brain->>Brain: semantic chunking\n(sentence-aware 200-1500 chars)
        Brain->>DB: store N chunk Notes\n[:PART_OF] → parent
    end

    Brain->>LLM: extract_entities(content)
    Brain->>DB: MERGE Entity nodes\n[:MENTIONS] edges

    Brain->>DB: find Notes with cosine_sim ≥ 0.75
    Brain->>DB: CREATE [:RELATES_TO {similarity}] edges

    Brain-->>Client: note_id, chunk_count, entities_found

    Note over Client,DB: Later: retrieval

    Client->>Brain: search_notes(query, limit)
    Brain->>LLM: embed(query) → vector
    Brain->>DB: vector similarity search (cosine)\n+ BM25 keyword search
    Brain->>Brain: Reciprocal Rank Fusion\n(hybrid RRF merge)
    Brain-->>Client: ranked Note results
```

---

## 2. Goal Execution with Evaluator Loop

When a task has `success_criteria`, the scheduler automatically appends an evaluator step
that grades the output and re-dispatches if quality is insufficient.

```mermaid
flowchart TD
    CREATE[create_task\ngoal + success_criteria]
    CREATE --> SCHED_TICK[Scheduler Tick\ngoal_to_steps]
    SCHED_TICK --> CHAIN[Job Chain\nstep 1 → step 2 → ... → evaluator]

    CHAIN --> JOB1[Job: tool call\ne.g. search_notes]
    JOB1 -->|result| JOB2[Job: tool call\ne.g. store_note]
    JOB2 -->|result| EVAL[Evaluator Job\nreflect_on_work\ncurrent_state=prev_output]

    EVAL --> SCORE{Score ≥ min_score\ndefault 3.5/5?}

    SCORE -->|Yes| SUCCESS[Task → completed\nchain done]
    SCORE -->|No| RETRY[Create new Task\noriginal goal +\ncritique injected in context]
    RETRY --> SCHED_TICK

    style CREATE fill:#4a90d9,color:#fff
    style SUCCESS fill:#2e8b57,color:#fff
    style RETRY fill:#cd5c5c,color:#fff
    style EVAL fill:#9370db,color:#fff
```

---

## 3. Autonomous Scheduler Loop

The scheduler runs every `SCHEDULER_INTERVAL_SECS` (default 5 min) without any human prompting.

```mermaid
stateDiagram-v2
    [*] --> Waiting : scheduler_control(start)
    Waiting --> Scanning : interval elapsed
    Scanning --> Dispatching : found Tasks with status=created
    Dispatching --> Waiting : all tasks enqueued as job chains
    Scanning --> IdleCount : no Tasks found
    IdleCount --> Waiting : idle_count < 3
    IdleCount --> Sleeping : idle_count ≥ 3

    Sleeping --> Consolidating : enter sleep mode
    Consolidating --> Pruning : consolidate_memories complete
    Pruning --> Snapshotting : prune_stale_notes complete
    Snapshotting --> Waiting : knowledge_snapshot complete\nidle_count reset

    Waiting --> [*] : scheduler_control(stop)
```

**Sleep Mode Triggers:**
- 3 consecutive ticks with no new tasks dispatched
- Manually via `scheduler_control(action=stop)`

**Sleep Sequence Jobs:**
1. `consolidate_memories` — LLM summarizes overdue/abundant episodic notes
2. `prune_stale_notes` — removes notes not accessed in 30+ days
3. `knowledge_snapshot` — stores a reflection note summarizing the session

---

## 4. Background Job Queue Lifecycle

Every action the scheduler dispatches runs as a durable `AgentJob` in the priority queue.

```mermaid
stateDiagram-v2
    [*] --> queued : enqueue_jobs()
    queued --> running : coordinator picks up\n(within concurrency limit)
    running --> completed : tool call succeeded
    running --> queued : transient failure\nattempt_count < max_attempts
    running --> failed : unrecoverable error
    running --> dead : attempt_count ≥ max_attempts
    failed --> queued : manage_job(retry)
    dead --> queued : manage_job(retry)
    queued --> cancelled : manage_job(cancel)
    running --> cancelled : manage_job(cancel)

    completed --> [*]
    cancelled --> [*]

    note right of running
        Parent job completes →
        unpark child jobs
        (chain execution)
    end note
```

**Priority Levels:**
| Level | Value | Use Case |
|-------|-------|----------|
| Critical | 0 | Immediate execution (e.g., error recovery) |
| High | 1 | User-initiated tasks |
| Normal | 2 | Scheduler-dispatched work |
| Low | 3 | Background maintenance |

---

## 5. Memory Consolidation (Spaced Repetition)

Agent Brain implements a spaced repetition system for long-term memory health.

```mermaid
flowchart TD
    TRIGGER{Consolidation\nTrigger} -->|≥10 overdue notes\nOR ≥50 episodic notes| GATHER

    GATHER[Gather candidate notes\nnext_review_at ≤ now\nor note_type=episodic]
    GATHER --> GROUP[Group by semantic\nsimilarity clusters]
    GROUP --> LLM_CONS[LLM: synthesize\ngroup → 1 consolidated note]
    LLM_CONS --> STORE[Store consolidated Note\nnote_type=consolidated\n[:SUMMARIZED_BY] edges]
    STORE --> UPDATE[Update source notes\nnext_review_at = now + 30 days\nreview_interval_days += 5]
    UPDATE --> DONE[Memory footprint reduced\nKnowledge preserved]

    style TRIGGER fill:#cd853f,color:#fff
    style LLM_CONS fill:#9370db,color:#fff
    style DONE fill:#2e8b57,color:#fff
```

---

## 6. Dynamic Tool Creation

New tools can be defined at runtime without recompiling, using natural language descriptions.

```mermaid
sequenceDiagram
    participant Client as AI Client
    participant Brain as Agent Brain
    participant DB as Neo4j

    Client->>Brain: define_tool(name, description,\ninput_schema, procedure_steps)
    Brain->>DB: MERGE DynamicTool node
    Brain->>DB: MERGE Procedure node
    Brain->>DB: CREATE [:USES] DynamicTool→Procedure
    Brain->>Brain: register tool in live registry\n(available immediately)
    Brain-->>Client: tool_id, now callable as MCP tool

    Note over Client,Brain: Immediately usable

    Client->>Brain: call new_tool_name(args)
    Brain->>Brain: execute Procedure steps\nwith template substitution
    Brain-->>Client: procedure result
```

---

## 7. Multi-Hop Knowledge Reasoning

For complex questions, Agent Brain can traverse multiple relationship hops in the graph.

```mermaid
graph LR
    Q[Query:\n'What projects use\nRust async?']
    Q --> EMBED[Vector embed query]
    EMBED --> TOP5[Top-5 similar notes\nby cosine + BM25]
    TOP5 --> ENTITY[Extract entities\nfrom results]
    ENTITY --> HOP1[Hop 1: notes\n[:MENTIONS] entity='Rust']
    HOP1 --> HOP2[Hop 2: notes\n[:RELATES_TO] Rust notes]
    HOP2 --> HOP3[Hop 3: tasks\n[:REFLECTS_ON] related notes]
    HOP3 --> FUSE[RRF fusion\nre-rank all candidates]
    FUSE --> LLM_R[LLM: synthesize\nfinal answer from\ntop-K context]
    LLM_R --> ANS[Reasoned Answer\nwith source citations]

    style Q fill:#4a90d9,color:#fff
    style ANS fill:#2e8b57,color:#fff
    style LLM_R fill:#9370db,color:#fff
```
