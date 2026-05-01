# Agent Brain — Data Flows

## End-to-End Request Flow (HTTP Transport)

```mermaid
flowchart LR
    CLIENT[Client\nHTTP POST /mcp]

    CLIENT -->|Bearer token auth| AUTH{API Key\nValid?}
    AUTH -->|401| REJECT[Reject]
    AUTH -->|pass| ROUTE[Route to session]

    ROUTE --> STATE{Session\nState?}
    STATE -->|Initializing| INIT_H[Handle initialize\nor notifications/initialized]
    STATE -->|Ready| DISPATCH[McpServerCore\ntool dispatch]

    DISPATCH --> REGISTRY[Skill Registry\nlookup tool_name]
    REGISTRY -->|not found| ERR[ToolNotFound error]
    REGISTRY -->|found| EXEC[Execute skill method]

    EXEC --> NEO4J[(Neo4j)]
    EXEC --> LLM_SVC[LLM Service]
    EXEC --> DUCK[(DuckDB)]

    LLM_SVC -->|local| OLLAMA[Ollama :11434]
    LLM_SVC -->|cloud| CLOUD[Anthropic / Gemini API]

    EXEC --> RESULT[ToolCallResult]
    RESULT -->|JSON response| CLIENT
    RESULT -->|SSE stream| SSE_CLIENT[SSE Client]
```

---

## Memory Write Path (store_note)

```mermaid
flowchart TD
    INPUT[store_note called\ncontent, tags, note_type]

    INPUT --> SIZE{content\n> 1500 chars?}

    SIZE -->|Yes| CHUNK[Semantic chunker\nsentence-boundary split\n200-1500 chars per chunk]
    CHUNK --> EMBED_EACH[Embed each chunk\nOllama bge-m3 → 1024-dim]
    EMBED_EACH --> STORE_CHUNKS[Store N chunk Notes\n[:PART_OF] → parent]

    SIZE -->|No| EMBED_SINGLE[Embed content\n1024-dim vector]
    EMBED_SINGLE --> STORE_NOTE[MERGE Note\nembedding + metadata]

    STORE_CHUNKS --> ENTITY_EXT
    STORE_NOTE --> ENTITY_EXT

    ENTITY_EXT[LLM: extract entities\nperson / tool / technology\nconcept / org / url / date]
    ENTITY_EXT --> MERGE_ENT[MERGE Entity nodes\nCREATE :MENTIONS edges\nwith count]

    MERGE_ENT --> SIM_QUERY[Query: find Notes\ncosine_sim ≥ 0.75]
    SIM_QUERY --> REL_EDGES[CREATE :RELATES_TO edges\nfor each similar Note]

    REL_EDGES --> RETURN[Return: note_id\nchunk_count, entities, links]

    style INPUT fill:#4a90d9,color:#fff
    style RETURN fill:#2e8b57,color:#fff
```

---

## Memory Read Path (search_notes)

```mermaid
flowchart TD
    QUERY[search_notes called\nquery string, limit, filters]

    QUERY --> EMBED_Q[Embed query\n1024-dim vector]
    EMBED_Q --> PARALLEL{Parallel search}

    PARALLEL --> VECTOR[Vector search\nNeo4j cosine similarity\ntop-K candidates]
    PARALLEL --> BM25[BM25 keyword search\nterm frequency scoring\ntop-K candidates]

    VECTOR --> RRF[Reciprocal Rank Fusion\nmerge + re-rank]
    BM25 --> RRF

    RRF --> FILTER[Apply filters\nnote_type, tags,\ndate range]

    FILTER --> UPDATE[Update access stats\naccess_count++\nlast_accessed_at = now]
    UPDATE --> RETURN[Return ranked results\nwith scores + metadata]

    style QUERY fill:#4a90d9,color:#fff
    style RRF fill:#9370db,color:#fff
    style RETURN fill:#2e8b57,color:#fff
```

---

## Job Execution Flow (QueueService Coordinator)

```mermaid
flowchart TD
    ENQUEUE[enqueue_jobs called\ntool_name, args, priority]
    ENQUEUE --> DB_WRITE[Write AgentJob to Neo4j\nstatus=queued]
    DB_WRITE --> HEAP[Push to in-memory BinaryHeap\nordered: priority → FIFO]
    HEAP --> NOTIFY[Notify coordinator\ntokio::sync::Notify]

    NOTIFY --> COORD[Coordinator loop\nwakes up]
    COORD --> SEMAPHORE{Semaphore slots\navailable?}
    SEMAPHORE -->|No| WAIT[Wait for slot release]
    WAIT --> SEMAPHORE
    SEMAPHORE -->|Yes| PICK[Pop highest priority job]

    PICK --> PROVIDER{Provider\nhint?}
    PROVIDER -->|ollama| OLL_SEM[Acquire Ollama semaphore\nmax 3 concurrent]
    PROVIDER -->|anthropic| ANT_SEM[Acquire Anthropic semaphore\nmax 2 concurrent]
    PROVIDER -->|gemini| GEM_SEM[Acquire Gemini semaphore\nmax 5 concurrent]
    PROVIDER -->|none| EXEC

    OLL_SEM --> EXEC
    ANT_SEM --> EXEC
    GEM_SEM --> EXEC

    EXEC[Execute tool call\nstatus=running]
    EXEC --> RESULT{Success?}

    RESULT -->|Yes| COMPLETE[status=completed\nresult_json saved\nunpark child jobs]
    RESULT -->|Transient error\nattempts remaining| REQUEUE[status=queued\nattempt_count++]
    RESULT -->|Max attempts hit| DEAD[status=dead\nDead Letter Queue]
    RESULT -->|Unrecoverable| FAILED[status=failed\nerror saved]

    COMPLETE --> EMIT[Emit BrainEvent\nJobCompleted]
    EMIT --> SSE[Push SSE to\nconnected sessions]

    style ENQUEUE fill:#4a90d9,color:#fff
    style COMPLETE fill:#2e8b57,color:#fff
    style DEAD fill:#8b0000,color:#fff
    style FAILED fill:#cd5c5c,color:#fff
```

---

## Secret Resolution Flow

```mermaid
flowchart TD
    TOOL[Tool calls requiring\ncredentials\ne.g. http_request with API key]

    TOOL --> PROVIDER{SECRET_PROVIDER\nenv var}

    PROVIDER -->|local| LOCAL[Read .secrets.enc\nAES-256-GCM decrypt\nusing SECRETS_ENCRYPTION_KEY]
    PROVIDER -->|vault| VAULT[HashiCorp Vault\nKV read via VAULT_TOKEN]
    PROVIDER -->|aws| AWS[AWS Secrets Manager\nvia IAM role / credentials]
    PROVIDER -->|none| PLAIN[Plain env vars\nor inline config]

    LOCAL --> SECRET_VAL[Secret value]
    VAULT --> SECRET_VAL
    AWS --> SECRET_VAL
    PLAIN --> SECRET_VAL

    SECRET_VAL --> INJECT[Inject into tool call\nheaders / args]
    INJECT --> EXEC[Execute tool\nwith credentials]

    style SECRET_VAL fill:#9370db,color:#fff
    style EXEC fill:#2e8b57,color:#fff
```

---

## Scheduler Chain Building (goal_to_steps)

```mermaid
flowchart TD
    TASK[Task: goal + context\n+ optional success_criteria]

    TASK --> ANALYZE[LLM: analyze goal\nidentify required tools]
    ANALYZE --> STEPS[Generate ChainStep list\n[tool_name, args_template, ...]]

    STEPS --> HAS_CRITERIA{success_criteria\nset?}
    HAS_CRITERIA -->|Yes| APPEND[Append evaluator step\ntool=reflect_on_work\nis_evaluator=true\nmin_score=3.5]
    HAS_CRITERIA -->|No| NO_EVAL[Plain chain\nno evaluation]

    APPEND --> CHAIN[ChainStep array]
    NO_EVAL --> CHAIN

    CHAIN --> PARK{Has dependencies?}
    PARK -->|Yes| PARK_JOBS[Status=parked\nuntil parent completes]
    PARK -->|No| QUEUE_DIRECT[Status=queued\nimmediately runnable]

    PARK_JOBS --> CHAIN_IN_DB[All jobs written\nto Neo4j as AgentJob\nwith parent_job_id links]
    QUEUE_DIRECT --> CHAIN_IN_DB

    CHAIN_IN_DB --> COORD[Coordinator unparks\nchildren on parent success]
```
