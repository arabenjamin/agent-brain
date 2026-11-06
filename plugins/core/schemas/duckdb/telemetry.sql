-- Core's DuckDB tables (the telemetry database, DATASET_DIR/telemetry.duckdb),
-- run whole on every open by TelemetryClient::init_schema
-- (crates/repository/src/telemetry.rs). Every statement must be idempotent:
-- CREATE TABLE IF NOT EXISTS, and a column added later as ALTER TABLE ... ADD
-- COLUMN IF NOT EXISTS after its CREATE, so an old database gains it and a
-- fresh one gets it the same way.
-- Docs: project-docs/schema.md § DuckDB.

CREATE TABLE IF NOT EXISTS interactions (
    id UUID PRIMARY KEY,
    timestamp TIMESTAMPTZ NOT NULL,
    prompt TEXT NOT NULL,
    response TEXT,
    tools_used JSON,
    success BOOLEAN,
    feedback_score INTEGER,
    feedback_text TEXT,
    latency_ms INTEGER,
    model_used TEXT
);

CREATE TABLE IF NOT EXISTS knowledge_gaps (
    id UUID PRIMARY KEY,
    timestamp TIMESTAMPTZ NOT NULL,
    query TEXT NOT NULL,
    context TEXT,
    gap_type TEXT
);

CREATE TABLE IF NOT EXISTS model_registry (
    name           TEXT PRIMARY KEY,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    context_window INTEGER NOT NULL,
    cost_input     DOUBLE NOT NULL,
    cost_output    DOUBLE NOT NULL,
    capabilities   TEXT NOT NULL,
    system_prompt  TEXT,
    temperature    DOUBLE,
    max_tokens     INTEGER,
    timeout_secs   INTEGER,
    selection_rank INTEGER,
    loaded_at      TIMESTAMPTZ DEFAULT current_timestamp
);

ALTER TABLE model_registry ADD COLUMN IF NOT EXISTS selection_rank INTEGER;

CREATE TABLE IF NOT EXISTS model_usage (
    id             TEXT PRIMARY KEY,
    model_name     TEXT NOT NULL,
    tool_name      TEXT,
    success        BOOLEAN,
    duration_ms    INTEGER,
    tokens_in      INTEGER,
    tokens_out     INTEGER,
    cost           DOUBLE,
    error_kind     TEXT,
    created_at     TIMESTAMPTZ DEFAULT current_timestamp,
    provider       TEXT
);

ALTER TABLE model_usage ADD COLUMN IF NOT EXISTS error_kind TEXT;
-- Provider-registry name (ollama-cloud, mistral, …). NULL on rows
-- written before 2026-09-13. Per-provider budgets are keyed on it.
ALTER TABLE model_usage ADD COLUMN IF NOT EXISTS provider TEXT;
-- The Task that owned the call (`ctx::current_owner_task`: a
-- workflow run's record), so cost is attributable per workflow.
-- NULL on chat turns and on rows written before 2026-10-17.
ALTER TABLE model_usage ADD COLUMN IF NOT EXISTS task_id TEXT;
-- What the provider said about the call itself (2026-11-05): prompt
-- tokens it served from its cache (OpenRouter's
-- prompt_tokens_details.cached_tokens, part of tokens_in), and
-- whether `cost` is the provider's billed amount (true) or the
-- catalog estimate (NULL). Anthropic chat folds its cache into
-- tokens_in instead, so this stays NULL there.
ALTER TABLE model_usage ADD COLUMN IF NOT EXISTS cached_tokens INTEGER;
ALTER TABLE model_usage ADD COLUMN IF NOT EXISTS cost_reported BOOLEAN;

CREATE TABLE IF NOT EXISTS search_usage (
    id           TEXT PRIMARY KEY,
    engine       TEXT NOT NULL,
    query        TEXT,
    success      BOOLEAN,
    result_count INTEGER,
    duration_ms  INTEGER,
    error_kind   TEXT,
    created_at   TIMESTAMPTZ DEFAULT current_timestamp
);

-- Attribution, budget units and quality hints (2026-10-16, search
-- rotation). NULL on rows written before then.
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS call_id TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS units DOUBLE;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS picked TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS requested TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS purpose TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS tool TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS task_id TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS recency TEXT;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS news BOOLEAN;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS dated INTEGER;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS in_window INTEGER;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS known_source INTEGER;
ALTER TABLE search_usage ADD COLUMN IF NOT EXISTS domains INTEGER;

-- Each attempt's result links, so a report can tell which results
-- were later used (cited as claim evidence, ingested). Pruned to
-- 90 days at startup.
-- One row per read_page reader attempt (2026-10-16): the page
-- readers' quota ledger and performance record.
CREATE TABLE IF NOT EXISTS read_usage (
    id          TEXT PRIMARY KEY,
    call_id     TEXT,
    reader      TEXT NOT NULL,
    url         TEXT,
    success     BOOLEAN,
    chars       INTEGER,
    duration_ms INTEGER,
    error_kind  TEXT,
    units       DOUBLE,
    picked      TEXT,
    purpose     TEXT,
    tool        TEXT,
    task_id     TEXT,
    created_at  TIMESTAMPTZ DEFAULT current_timestamp
);

CREATE TABLE IF NOT EXISTS search_results (
    usage_id   TEXT NOT NULL,
    rank       INTEGER,
    link       TEXT,
    created_at TIMESTAMPTZ DEFAULT current_timestamp
);

-- One row per tool call whose arguments did not fit the tool's
-- schema (2026-10-16, the dispatch check). `refused` is false in
-- shadow mode: the call ran anyway.
CREATE TABLE IF NOT EXISTS arg_checks (
    id         TEXT PRIMARY KEY,
    tool       TEXT NOT NULL,
    purpose    TEXT,
    mode       TEXT,
    refused    BOOLEAN,
    problems   TEXT,
    args       TEXT,
    created_at TIMESTAMPTZ DEFAULT current_timestamp
);
-- The agent whose loop made the call (ctx::current_agent); NULL
-- outside a sub-agent and before 2026-10-17.
ALTER TABLE arg_checks ADD COLUMN IF NOT EXISTS agent TEXT;

-- One row per sub-agent run (run_agent, delegate): PROGRAMS_PLAN
-- Phase 4's agent scorecard. `caller` is chat or background;
-- tool_calls / tools_failed come from the run's own trace.
CREATE TABLE IF NOT EXISTS agent_runs (
    id           TEXT PRIMARY KEY,
    agent        TEXT NOT NULL,
    caller       TEXT,
    model        TEXT,
    ok           BOOLEAN,
    tool_calls   INTEGER,
    tools_failed INTEGER,
    duration_ms  BIGINT,
    task_id      TEXT,
    created_at   TIMESTAMPTZ DEFAULT current_timestamp
);
-- HARNESS2_PLAN Phase 1 (2026-10-23): the run's task (cut) and its
-- trajectory (trajectory::Trajectory as JSON: each call's tool,
-- args, ok, stubbed, result head; then the answer). `caller` is
-- `probe` for a probe run, which acted on nothing.
ALTER TABLE agent_runs ADD COLUMN IF NOT EXISTS task TEXT;
ALTER TABLE agent_runs ADD COLUMN IF NOT EXISTS trajectory TEXT;

-- One row per call to an outside service the brain paces or
-- budgets (YOUTUBE_DATA_API_PLAN Phase 0: every yt-dlp call, from
-- 2026-10-17). `outcome` is ok, error, bot_check or backoff (refused
-- by the brain's own back-off, nothing sent); `units` is the
-- service's quota cost when it has one; `subject` is what was
-- asked for (a video URL), so repeats show.
CREATE TABLE IF NOT EXISTS external_calls (
    id           TEXT PRIMARY KEY,
    service      TEXT NOT NULL,
    op           TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    duration_ms  BIGINT,
    units        BIGINT,
    subject      TEXT,
    task_id      TEXT,
    created_at   TIMESTAMPTZ DEFAULT current_timestamp
);
