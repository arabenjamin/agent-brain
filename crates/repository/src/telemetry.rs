use anyhow::{Context, Result};
use chrono::Utc;
use duckdb::{Connection, params};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

use crate::{EngineUse, Ledger, ReadAttempt, SearchAttempt};

/// How long each search result link is kept for the "used" report.
const SEARCH_RESULTS_KEEP_DAYS: i64 = 90;

/// One provider's usage for the budget ledger, as returned by
/// [`TelemetryClient::budget_usage_since`]: `(provider, requests_today,
/// tokens_today, spend_today_usd, spend_month_usd)`.
pub type ProviderUsageRow = (String, i64, i64, f64, f64);
/// [`TelemetryClient::budget_model_usage_since`]: `(provider, model, requests_today,
/// tokens_today)`.
pub type ModelUsageRow = (String, String, i64, i64);

/// Client for the local DuckDB telemetry store.
/// This serves as the "Hippocampus" - storing raw experiences for later "sleep" (fine-tuning).
#[derive(Clone)]
pub struct TelemetryClient {
    // DuckDB connection is not thread-safe by default, so we wrap it.
    // In a high-throughput scenario, we might use a pool or r2d2-duckdb,
    // but for an agent brain, a mutex is usually fine for now.
    conn: Arc<Mutex<Connection>>,
}

/// Time-zone support (TIMESTAMPTZ arithmetic, casts to DATE) is DuckDB's icu
/// extension, which the crate cannot compile in (its `icu` feature needs DuckDB
/// sources the published crate does not ship). So it is loaded here, before
/// external access is locked off: from `dir` (next to the database, so it
/// survives rebuilds), downloaded into it the first time. A failure is logged
/// and init goes on: without icu only queries that do time-zone arithmetic on
/// `created_at` fail, and casting to TIMESTAMP first still works.
fn load_icu(conn: &Connection, dir: &Path) {
    let dir_sql = dir.display().to_string().replace('\'', "''");
    if let Err(e) = std::fs::create_dir_all(dir) {
        warn!(dir = %dir.display(), error = %e, "DuckDB extension dir not writable; icu not loaded");
        return;
    }
    if let Err(e) = conn.execute_batch(&format!("SET extension_directory = '{dir_sql}';")) {
        warn!(error = %e, "Could not set DuckDB extension_directory; icu not loaded");
        return;
    }
    if conn.execute_batch("LOAD icu;").is_ok() {
        info!("DuckDB icu loaded");
        return;
    }
    match conn.execute_batch("INSTALL icu; LOAD icu;") {
        Ok(()) => info!(dir = %dir.display(), "DuckDB icu downloaded and loaded"),
        Err(e) => warn!(
            error = %e,
            "DuckDB icu unavailable: time-zone arithmetic on TIMESTAMPTZ will fail (cast to TIMESTAMP first)"
        ),
    }
}

impl TelemetryClient {
    /// Create a new TelemetryClient backed by a file.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path).context("Failed to open DuckDB file")?;
        // Before `init_schema` locks external access: icu is the one extension
        // that cannot be compiled in. An in-memory database (tests) skips it.
        if path != Path::new(":memory:") {
            let dir = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("duckdb_extensions");
            load_icu(&conn, &dir);
        }

        let client = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        client.init_schema()?;

        Ok(client)
    }

    /// Initialize the schema.
    fn init_schema(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        // Table: interactions
        // Logs every turn of conversation/action.
        conn.execute_batch(
            r"
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
            ",
        )?;

        // Containers are killed, not gracefully stopped, so DuckDB may never get
        // to checkpoint on close — the WAL then grows across deploys until a
        // mid-write kill leaves a tail that fails replay and bricks telemetry
        // (observed 2026-06-12: 16 MB WAL, main file untouched for six weeks).
        // Checkpointing right after open bounds the WAL to one process lifetime.
        let cutoff = (Utc::now() - chrono::Duration::days(SEARCH_RESULTS_KEEP_DAYS)).to_rfc3339();
        conn.execute(
            "DELETE FROM search_results WHERE created_at < CAST(? AS TIMESTAMPTZ)",
            params![cutoff],
        )?;

        conn.execute_batch("CHECKPOINT;")?;

        // Model-written SQL reaches this connection (`duckdb_query`), and a
        // SELECT can read files and URLs (`read_csv('/etc/passwd')`,
        // `read_text`, `glob`, httpfs). The brain itself never reads outside
        // the database here, so external access is off for the process, and
        // locked so no statement turns it back on (2026-10-16).
        //
        // `query_raw`'s SELECT check needs the json extension. It is compiled
        // in (the `json` crate feature); were it ever not, DuckDB would fetch
        // it on first use, which the lock forbids, and every `duckdb_query`
        // would then fail closed. So json is used once here, before the lock.
        // (icu is loaded in `new`, before this.)
        conn.query_row("SELECT json_serialize_sql('SELECT 1')", [], |_| Ok(()))?;
        conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;")?;

        info!("Telemetry (DuckDB) schema initialized");
        Ok(())
    }

    /// Record a tool call whose arguments did not fit its schema. `args` is
    /// cut to 2,000 characters; `problems` is stored as a JSON array.
    #[allow(clippy::too_many_arguments)]
    pub fn log_arg_check(
        &self,
        tool: &str,
        purpose: &str,
        mode: &str,
        refused: bool,
        problems: &[String],
        args: &str,
        agent: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cut: String = args.chars().take(2000).collect();
        conn.execute(
            "INSERT INTO arg_checks (id, tool, purpose, mode, refused, problems, args, agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                uuid::Uuid::new_v4().to_string(),
                tool,
                purpose,
                mode,
                refused,
                serde_json::to_string(problems).unwrap_or_default(),
                cut,
                agent,
            ],
        )?;
        Ok(())
    }

    /// Record one sub-agent run (PROGRAMS_PLAN Phase 4).
    #[allow(clippy::too_many_arguments)]
    pub fn record_agent_run(
        &self,
        agent: &str,
        caller: &str,
        model: &str,
        ok: bool,
        tool_calls: usize,
        tools_failed: usize,
        duration_ms: u64,
        task_id: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        conn.execute(
            "INSERT INTO agent_runs (id, agent, caller, model, ok, tool_calls, tools_failed, duration_ms, task_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                uuid::Uuid::new_v4().to_string(),
                agent,
                caller,
                model,
                ok,
                tool_calls as i64,
                tools_failed as i64,
                duration_ms as i64,
                task_id,
            ],
        )?;
        Ok(())
    }

    /// Log a completed interaction.
    pub fn log_interaction(
        &self,
        prompt: &str,
        response: &str,
        tools_used: Option<&Value>,
        success: bool,
        latency_ms: u64,
        model: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let id = uuid::Uuid::new_v4();
        let now = Utc::now();

        conn.execute(
            "INSERT INTO interactions (id, timestamp, prompt, response, tools_used, success, latency_ms, model_used) 
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.to_string(),
                now.to_rfc3339(),
                prompt,
                response,
                tools_used.map(|v| v.to_string()),
                success,
                latency_ms as i64,
                model
            ],
        )?;

        Ok(())
    }

    /// Log a knowledge gap (missing info, tool failure, etc.).
    pub fn log_knowledge_gap(
        &self,
        query: &str,
        context: Option<&str>,
        gap_type: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let id = uuid::Uuid::new_v4();
        let now = Utc::now();

        conn.execute(
            "INSERT INTO knowledge_gaps (id, timestamp, query, context, gap_type) 
             VALUES (?, ?, ?, ?, ?)",
            params![id.to_string(), now.to_rfc3339(), query, context, gap_type],
        )?;

        Ok(())
    }

    // =========================================================================
    // Model registry
    // =========================================================================

    /// Upsert a model entry into the model_registry table.
    ///
    /// `capabilities` is a JSON array string, e.g. `["reasoning","code"]`.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_model(
        &self,
        name: &str,
        provider: &str,
        model: &str,
        context_window: i64,
        cost_input: f64,
        cost_output: f64,
        capabilities: &str,
        system_prompt: Option<&str>,
        temperature: Option<f64>,
        max_tokens: Option<i64>,
        timeout_secs: Option<i64>,
        selection_rank: Option<i64>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        conn.execute(
            "INSERT OR REPLACE INTO model_registry
             (name, provider, model, context_window, cost_input, cost_output,
              capabilities, system_prompt, temperature, max_tokens, timeout_secs,
              selection_rank, loaded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, current_timestamp)",
            params![
                name,
                provider,
                model,
                context_window,
                cost_input,
                cost_output,
                capabilities,
                system_prompt,
                temperature,
                max_tokens,
                timeout_secs,
                selection_rank
            ],
        )?;
        Ok(())
    }

    /// Delete all rows from model_registry (used before a fresh sync).
    pub fn clear_model_registry(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        conn.execute("DELETE FROM model_registry", [])?;
        Ok(())
    }

    /// List all models, ordered by provider then name.
    pub fn list_models(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT name, provider, model, context_window, cost_input, cost_output,
                    capabilities, system_prompt, temperature, max_tokens, timeout_secs
             FROM model_registry
             ORDER BY provider ASC, name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({
                "name":           row.get::<_, String>(0)?,
                "provider":       row.get::<_, String>(1)?,
                "model":          row.get::<_, String>(2)?,
                "context_window": row.get::<_, i64>(3)?,
                "cost_per_1k_input":  row.get::<_, f64>(4)?,
                "cost_per_1k_output": row.get::<_, f64>(5)?,
                "capabilities":   row.get::<_, String>(6)?,
                "system_prompt":  row.get::<_, Option<String>>(7)?,
                "temperature":    row.get::<_, Option<f64>>(8)?,
                "max_tokens":     row.get::<_, Option<i64>>(9)?,
                "timeout_secs":   row.get::<_, Option<i64>>(10)?,
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Select models that satisfy capability and cost constraints.
    ///
    /// Ordering is **cost ascending, then `selection_rank` ascending, then
    /// `context_window` descending**.
    ///
    /// The middle term is the one that matters. Every local and Ollama-Cloud
    /// model in the catalog costs `0.0`, so cost decides nothing among them and
    /// the tiebreak *is* the selection. It used to be `context_window DESC`
    /// alone — a proxy for nothing anyone cares about, which handed every
    /// capability-routed step to whichever model declared the biggest window.
    /// On 2026-08-25 that was `minimax-m3:cloud` (524288), which displaced
    /// `gemma4:31b-cloud` on every `reasoning` step and ran them at 20.2s
    /// against the previous 5.6s — measured in the usage ledger, not predicted.
    ///
    /// `selection_rank` makes the choice explicit and reviewable in
    /// `models.yaml` instead of emergent from an unrelated number. Lower wins.
    /// Unranked rows sort behind every ranked one (`COALESCE(..., 1000)`) and
    /// keep the old widest-window order among themselves, so a catalog entry
    /// that omits the field degrades to the previous behaviour rather than
    /// jumping the queue.
    pub fn select_models(
        &self,
        required_capabilities: &[String],
        provider_hint: Option<&str>,
        max_cost_per_1k: Option<f64>,
    ) -> Result<Vec<serde_json::Value>> {
        // We filter capabilities in Rust after fetching candidates because
        // capabilities is stored as a JSON array string.
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let max_cost = max_cost_per_1k.unwrap_or(f64::MAX);
        let provider_filter = provider_hint.unwrap_or("%");

        let sql = if provider_hint.is_some() {
            "SELECT name, provider, model, context_window, cost_input, cost_output,
                    capabilities, COALESCE(selection_rank, 1000) AS rank
             FROM model_registry
             WHERE provider = ? AND (cost_input + cost_output) <= ?
             ORDER BY (cost_input + cost_output) ASC, rank ASC, context_window DESC"
        } else {
            "SELECT name, provider, model, context_window, cost_input, cost_output,
                    capabilities, COALESCE(selection_rank, 1000) AS rank
             FROM model_registry
             WHERE (cost_input + cost_output) <= ?
             ORDER BY (cost_input + cost_output) ASC, rank ASC, context_window DESC"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<_> = if provider_hint.is_some() {
            stmt.query_map(params![provider_filter, max_cost], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![max_cost], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut out = Vec::new();
        for (name, provider, model, ctx, cost_in, cost_out, caps_str, rank) in rows {
            // Parse capabilities JSON array and filter.
            let caps: Vec<String> = serde_json::from_str(&caps_str).unwrap_or_default();
            if required_capabilities.iter().all(|req| caps.contains(req)) {
                out.push(serde_json::json!({
                    "name":               name,
                    "provider":           provider,
                    "model":              model,
                    "context_window":     ctx,
                    "cost_per_1k_input":  cost_in,
                    "cost_per_1k_output": cost_out,
                    "capabilities":       caps,
                    "selection_rank":     rank,
                }));
            }
        }
        Ok(out)
    }

    // =========================================================================
    // Model usage tracking
    // =========================================================================

    /// Record a single model invocation.
    ///
    /// `error_kind` marks notable failure classes (e.g. `"rate_limited"`) so
    /// quota pressure is queryable; pass `None` for ordinary calls.
    /// Cost is computed from `model_registry` per-1k rates when token counts
    /// are present (matched on registry `model` or `name`).
    #[allow(clippy::too_many_arguments)]
    pub fn record_model_usage(
        &self,
        model_name: &str,
        provider: Option<&str>,
        tool_name: Option<&str>,
        success: bool,
        duration_ms: Option<i64>,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
        error_kind: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<Option<f64>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let id = uuid::Uuid::new_v4().to_string();

        // Compute cost from registry per-1k rates when we have token counts.
        let cost: Option<f64> = if tokens_in.is_some() || tokens_out.is_some() {
            conn.query_row(
                "SELECT cost_input, cost_output FROM model_registry
                 WHERE model = ? OR name = ? LIMIT 1",
                params![model_name, model_name],
                |row| {
                    let cin: f64 = row.get(0)?;
                    let cout: f64 = row.get(1)?;
                    Ok((cin, cout))
                },
            )
            .ok()
            .map(|(cin, cout)| {
                (tokens_in.unwrap_or(0) as f64 / 1000.0) * cin
                    + (tokens_out.unwrap_or(0) as f64 / 1000.0) * cout
            })
        } else {
            None
        };

        conn.execute(
            "INSERT INTO model_usage
             (id, model_name, provider, tool_name, success, duration_ms, tokens_in, tokens_out, cost, error_kind, task_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                model_name,
                provider,
                tool_name,
                success,
                duration_ms,
                tokens_in,
                tokens_out,
                cost,
                error_kind,
                task_id
            ],
        )?;
        Ok(cost)
    }

    /// Per-provider usage for the budget ledger: requests, tokens and cost since
    /// `day_start`, and cost since `month_start` (both RFC 3339). A row written
    /// before `model_usage.provider` existed takes its provider from the catalog.
    pub fn budget_usage_since(
        &self,
        day_start: &str,
        month_start: &str,
    ) -> Result<Vec<ProviderUsageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT provider,
                    CAST(SUM(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ) THEN 1 ELSE 0 END) AS BIGINT),
                    CAST(SUM(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ)
                                  THEN COALESCE(tokens_in, 0) + COALESCE(tokens_out, 0) ELSE 0 END) AS BIGINT),
                    CAST(SUM(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ)
                                  THEN COALESCE(cost, 0) ELSE 0 END) AS DOUBLE),
                    CAST(SUM(COALESCE(cost, 0)) AS DOUBLE)
             FROM (
                 SELECT COALESCE(u.provider, r.provider) AS provider,
                        u.created_at, u.tokens_in, u.tokens_out, u.cost
                 FROM model_usage u
                 LEFT JOIN (SELECT model, ANY_VALUE(provider) AS provider
                            FROM model_registry GROUP BY model) r
                   ON r.model = u.model_name
                 WHERE u.created_at >= CAST(? AS TIMESTAMPTZ)
             ) t
             WHERE provider IS NOT NULL
             GROUP BY provider",
        )?;
        let rows = stmt.query_map(
            params![day_start, day_start, day_start, month_start],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            },
        )?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Per-model requests and tokens since `day_start` (RFC 3339), for models
    /// that declare their own limits: `(provider, model, requests, tokens)`.
    /// Provider resolution matches [`Self::budget_usage_since`].
    pub fn budget_model_usage_since(&self, day_start: &str) -> Result<Vec<ModelUsageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(u.provider, r.provider) AS provider, u.model_name,
                    CAST(COUNT(*) AS BIGINT),
                    CAST(SUM(COALESCE(u.tokens_in, 0) + COALESCE(u.tokens_out, 0)) AS BIGINT)
             FROM model_usage u
             LEFT JOIN (SELECT model, ANY_VALUE(provider) AS provider
                        FROM model_registry GROUP BY model) r
               ON r.model = u.model_name
             WHERE u.created_at >= CAST(? AS TIMESTAMPTZ)
               AND COALESCE(u.provider, r.provider) IS NOT NULL
             GROUP BY 1, 2",
        )?;
        let rows = stmt.query_map(params![day_start], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Model names with a given `error_kind` recorded in the last `hours`.
    /// The model router uses this as observed availability: a model that
    /// recently returned "subscription_required" is skipped at selection.
    pub fn models_with_recent_errors(&self, error_kind: &str, hours: i64) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cutoff = (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT model_name FROM model_usage
             WHERE error_kind = ? AND created_at >= CAST(? AS TIMESTAMPTZ)",
        )?;
        let rows = stmt.query_map(params![error_kind, cutoff], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // =========================================================================
    // Search usage ledger
    // =========================================================================

    /// Record one `search_web` engine attempt, and its result links.
    ///
    /// One row per *engine attempt*, not per tool call: a failover that tries
    /// two engines writes two rows, because quota accounting needs to know
    /// what each engine was actually asked to do. `call_id` groups them.
    ///
    /// `error_kind` marks notable failure classes; `"quota_exhausted"` is the
    /// one that matters, since it is how a free tier dying becomes queryable
    /// instead of an opaque 429 buried in a dead job's error text.
    pub fn record_search_attempt(&self, a: &SearchAttempt) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let id = uuid::Uuid::new_v4().to_string();
        // Queries can be whole reasoning prompts on gap-fill chains; the ledger
        // only needs enough to identify the search, not to reproduce it.
        let truncated: String = a.query.chars().take(500).collect();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO search_usage
             (id, engine, query, success, result_count, duration_ms, error_kind,
              call_id, units, picked, requested, purpose, tool, task_id,
              recency, news, dated, in_window, known_source, domains)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                a.engine,
                truncated,
                a.success,
                a.result_count,
                a.duration_ms,
                a.error_kind,
                a.call_id,
                a.units,
                a.picked,
                a.requested,
                a.purpose,
                a.tool,
                a.task_id,
                a.recency,
                a.news,
                a.dated,
                a.in_window,
                a.known_source,
                a.domains
            ],
        )?;
        for (rank, link) in a.links.iter().enumerate() {
            tx.execute(
                "INSERT INTO search_results (usage_id, rank, link) VALUES (?, ?, ?)",
                params![id, rank as i64 + 1, link],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One engine's use since its budget period began (successful attempts
    /// only: no vendor bills a failed call), today's calls (UTC), and its
    /// attempts in the last hour and minute.
    pub fn engine_use(
        &self,
        engine: &str,
        period_start: chrono::DateTime<Utc>,
    ) -> Result<EngineUse> {
        self.provider_use(Ledger::Search, engine, period_start)
    }

    /// [`Self::engine_use`] for either ledger: `search_usage` by `engine`,
    /// `read_usage` by `reader`.
    pub fn provider_use(
        &self,
        ledger: Ledger,
        name: &str,
        period_start: chrono::DateTime<Utc>,
    ) -> Result<EngineUse> {
        let (table, column) = match ledger {
            Ledger::Search => ("search_usage", "engine"),
            Ledger::Read => ("read_usage", "reader"),
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let now = Utc::now();
        let day_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc())
            .unwrap_or(now);
        let (period, day, hour, minute) = (
            period_start.to_rfc3339(),
            day_start.to_rfc3339(),
            (now - chrono::Duration::hours(1)).to_rfc3339(),
            (now - chrono::Duration::minutes(1)).to_rfc3339(),
        );
        let mut stmt = conn.prepare(&format!(
            "SELECT
               COALESCE(SUM(CASE WHEN success AND created_at >= CAST(? AS TIMESTAMPTZ)
                                 THEN units END), 0) AS period_units,
               COUNT(CASE WHEN success AND units IS NULL
                               AND created_at >= CAST(? AS TIMESTAMPTZ) THEN 1 END),
               COUNT(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ) THEN 1 END),
               COUNT(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ) THEN 1 END),
               COUNT(CASE WHEN created_at >= CAST(? AS TIMESTAMPTZ) THEN 1 END)
             FROM {table}
             WHERE {column} = ? AND created_at >= CAST(? AS TIMESTAMPTZ)"
        ))?;
        // The scan starts at the earliest cutoff it needs (just after
        // midnight, the last hour reaches back past the day's start).
        let earliest = period.clone().min(day.clone()).min(hour.clone());
        let row = stmt.query_row(
            params![period, period, day, hour, minute, name, earliest],
            |r| {
                Ok(EngineUse {
                    engine: name.to_string(),
                    period_units: r.get::<_, f64>(0)?,
                    period_unpriced: r.get::<_, i64>(1)?,
                    day_calls: r.get::<_, i64>(2)?,
                    hour_calls: r.get::<_, i64>(3)?,
                    minute_calls: r.get::<_, i64>(4)?,
                })
            },
        )?;
        Ok(row)
    }

    /// Record one `read_page` reader attempt.
    pub fn record_read_attempt(&self, a: &ReadAttempt) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let url: String = a.url.chars().take(1000).collect();
        conn.execute(
            "INSERT INTO read_usage
             (id, call_id, reader, url, success, chars, duration_ms, error_kind,
              units, picked, purpose, tool, task_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                uuid::Uuid::new_v4().to_string(),
                a.call_id,
                a.reader,
                url,
                a.success,
                a.chars,
                a.duration_ms,
                a.error_kind,
                a.units,
                a.picked,
                a.purpose,
                a.tool,
                a.task_id
            ],
        )?;
        Ok(())
    }

    /// Per reader: attempts, successes, failures by kind, units, median
    /// text length and latency, and how each was picked.
    pub fn get_read_stats(&self, window_hours: Option<i64>) -> Result<serde_json::Value> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cutoff = window_hours
            .map(|h| (Utc::now() - chrono::Duration::hours(h)).to_rfc3339())
            .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
        let mut stmt = conn.prepare(
            "SELECT reader,
                    COUNT(*) AS attempts,
                    SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
                    SUM(CASE WHEN error_kind = 'quota_exhausted' THEN 1 ELSE 0 END) AS quota_exhausted,
                    SUM(CASE WHEN error_kind = 'too_little_text' THEN 1 ELSE 0 END) AS too_little_text,
                    SUM(units) AS units,
                    MEDIAN(CASE WHEN success THEN chars END) AS median_chars,
                    AVG(duration_ms) AS avg_duration_ms,
                    SUM(CASE WHEN picked = 'rotation' THEN 1 ELSE 0 END) AS by_rotation,
                    SUM(CASE WHEN picked = 'direct' THEN 1 ELSE 0 END) AS by_direct,
                    SUM(CASE WHEN picked = 'fallback' THEN 1 ELSE 0 END) AS by_fallback,
                    SUM(CASE WHEN picked = 'pinned' THEN 1 ELSE 0 END) AS by_pin
             FROM read_usage
             WHERE created_at >= CAST(? AS TIMESTAMPTZ)
             GROUP BY reader ORDER BY attempts DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            let n = |i: usize| row.get::<_, Option<i64>>(i);
            Ok(serde_json::json!({
                "reader": row.get::<_, String>(0)?,
                "attempts": row.get::<_, i64>(1)?,
                "successes": n(2)?.unwrap_or(0),
                "quota_exhausted": n(3)?.unwrap_or(0),
                "too_little_text": n(4)?.unwrap_or(0),
                "units": row.get::<_, Option<f64>>(5)?,
                "median_chars": row.get::<_, Option<f64>>(6)?,
                "avg_duration_ms": row.get::<_, Option<f64>>(7)?.map(f64::round),
                "picked": {
                    "rotation": n(8)?.unwrap_or(0),
                    "direct": n(9)?.unwrap_or(0),
                    "fallback": n(10)?.unwrap_or(0),
                    "pinned": n(11)?.unwrap_or(0),
                },
            }))
        })?;
        let readers: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
        Ok(serde_json::json!({ "window_hours": window_hours, "readers": readers }))
    }

    /// `(engine, link)` for every result returned in the last `window_hours`.
    pub fn search_links_since(&self, window_hours: i64) -> Result<Vec<(String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cutoff = (Utc::now() - chrono::Duration::hours(window_hours)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT u.engine, r.link FROM search_results r
             JOIN search_usage u ON u.id = r.usage_id
             WHERE r.created_at >= CAST(? AS TIMESTAMPTZ) AND r.link IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Engines that recorded a given `error_kind` within the last `hours`.
    ///
    /// The failover ladder uses this to *deprioritise* (never to permanently
    /// exclude) an engine whose quota just died — retrying an exhausted key on
    /// every step of an 8-search news chain wastes a round-trip each time.
    pub fn search_engines_with_recent_errors(
        &self,
        error_kind: &str,
        hours: i64,
    ) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cutoff = (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT engine FROM search_usage
             WHERE error_kind = ? AND created_at >= CAST(? AS TIMESTAMPTZ)",
        )?;
        let rows = stmt.query_map(params![error_kind, cutoff], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Readers that recorded `error_kind` within the last `hours`
    /// ([`Self::search_engines_with_recent_errors`] for `read_usage`).
    pub fn readers_with_recent_errors(&self, error_kind: &str, hours: i64) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let cutoff = (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT reader FROM read_usage
             WHERE error_kind = ? AND created_at >= CAST(? AS TIMESTAMPTZ)",
        )?;
        let rows = stmt.query_map(params![error_kind, cutoff], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Aggregated search usage: per-engine totals plus a per-day breakdown.
    ///
    /// The daily rollup is the part that answers the question that actually
    /// matters — "am I about to blow a monthly cap" — which per-engine
    /// all-time totals cannot.
    pub fn get_search_stats(&self, window_hours: Option<i64>) -> Result<serde_json::Value> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        // Cutoff computed in Rust for the same reason as get_model_stats:
        // DuckDB's TIMESTAMPTZ-minus-INTERVAL binding is version-dependent.
        let cutoff = window_hours.map(|h| (Utc::now() - chrono::Duration::hours(h)).to_rfc3339());
        let window_clause = if cutoff.is_some() {
            "created_at >= CAST(? AS TIMESTAMPTZ)"
        } else {
            "1 = 1"
        };

        let engine_sql = format!(
            "SELECT
               engine,
               COUNT(*) AS total,
               SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
               SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures,
               SUM(CASE WHEN error_kind = 'quota_exhausted' THEN 1 ELSE 0 END) AS quota_exhausted,
               SUM(result_count) AS total_results,
               AVG(duration_ms)  AS avg_duration_ms,
               SUM(units) AS units,
               -- Quality hints, over the attempts that recorded them.
               SUM(CASE WHEN dated IS NOT NULL THEN result_count END) AS measured_results,
               SUM(dated) AS dated,
               SUM(CASE WHEN in_window IS NOT NULL THEN result_count END) AS windowed_results,
               SUM(in_window) AS in_window,
               SUM(known_source) AS known_source,
               SUM(CASE WHEN picked = 'rotation' THEN 1 ELSE 0 END) AS by_rotation,
               SUM(CASE WHEN picked = 'pinned' THEN 1 ELSE 0 END) AS by_pin,
               SUM(CASE WHEN picked = 'fallback' THEN 1 ELSE 0 END) AS by_fallback,
               SUM(CASE WHEN picked = 'ladder' THEN 1 ELSE 0 END) AS by_ladder
             FROM search_usage
             WHERE {window_clause}
             GROUP BY engine
             ORDER BY total DESC"
        );
        let mut stmt = conn.prepare(&engine_sql)?;
        let ratio = |part: Option<i64>, whole: Option<i64>| match (part, whole) {
            (Some(p), Some(w)) if w > 0 => {
                serde_json::json!(((p as f64 / w as f64) * 1000.0).round() / 1000.0)
            }
            _ => serde_json::Value::Null,
        };
        let map_engine = |row: &duckdb::Row<'_>| {
            let n = |i: usize| row.get::<_, Option<i64>>(i);
            let measured = n(8)?;
            Ok(serde_json::json!({
                "engine":          row.get::<_, String>(0)?,
                "total_searches":  row.get::<_, i64>(1)?,
                "successes":       n(2)?.unwrap_or(0),
                "failures":        n(3)?.unwrap_or(0),
                "quota_exhausted": n(4)?.unwrap_or(0),
                "total_results":   n(5)?.unwrap_or(0),
                "avg_duration_ms": row.get::<_, Option<f64>>(6)?.map(f64::round),
                "units":           row.get::<_, Option<f64>>(7)?,
                "dated_share":     ratio(n(9)?, measured),
                "in_window_share": ratio(n(11)?, n(10)?),
                "known_source_share": ratio(n(12)?, measured),
                "picked": {
                    "rotation": n(13)?.unwrap_or(0),
                    "pinned":   n(14)?.unwrap_or(0),
                    "fallback": n(15)?.unwrap_or(0),
                    "ladder":   n(16)?.unwrap_or(0),
                },
            }))
        };
        let engines: Vec<serde_json::Value> = if let Some(ref c) = cutoff {
            stmt.query_map(params![c], map_engine)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_engine)?
                .filter_map(|r| r.ok())
                .collect()
        };

        // Who searched: the queue job's tool, else the routing purpose.
        let caller_sql = format!(
            "SELECT COALESCE(tool, purpose, 'unrecorded') AS caller,
                    COUNT(DISTINCT call_id) AS calls,
                    COUNT(*) AS attempts,
                    STRING_AGG(DISTINCT engine, ',') AS engines
             FROM search_usage
             WHERE {window_clause}
             GROUP BY caller
             ORDER BY attempts DESC"
        );
        let mut stmt = conn.prepare(&caller_sql)?;
        let map_caller = |row: &duckdb::Row<'_>| {
            Ok(serde_json::json!({
                "caller":   row.get::<_, String>(0)?,
                "calls":    row.get::<_, i64>(1)?,
                "attempts": row.get::<_, i64>(2)?,
                "engines":  row.get::<_, Option<String>>(3)?,
            }))
        };
        let by_caller: Vec<serde_json::Value> = if let Some(ref c) = cutoff {
            stmt.query_map(params![c], map_caller)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_caller)?
                .filter_map(|r| r.ok())
                .collect()
        };

        let daily_sql = format!(
            "SELECT
               -- strftime, not CAST(... AS DATE): DuckDB has no direct
               -- TIMESTAMPTZ -> DATE cast, and the string form is what the
               -- row mapper reads anyway. The inner cast to TIMESTAMP is
               -- required too — strftime has no TIMESTAMPTZ overload.
               strftime(CAST(created_at AS TIMESTAMP), '%Y-%m-%d') AS day,
               engine,
               COUNT(*) AS total
             FROM search_usage
             WHERE {window_clause}
             GROUP BY day, engine
             ORDER BY day DESC, total DESC"
        );
        let mut stmt = conn.prepare(&daily_sql)?;
        let map_day = |row: &duckdb::Row<'_>| {
            Ok((
                row.get::<_, String>(0)
                    .unwrap_or_else(|_| "unknown".to_string()),
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        };
        let daily_rows: Vec<_> = if let Some(ref c) = cutoff {
            stmt.query_map(params![c], map_day)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_day)?
                .filter_map(|r| r.ok())
                .collect()
        };
        let by_day: Vec<serde_json::Value> = daily_rows
            .into_iter()
            .map(|(day, engine, total)| {
                serde_json::json!({ "day": day, "engine": engine, "searches": total })
            })
            .collect();

        Ok(serde_json::json!({
            "window_hours": window_hours,
            "engines":      engines,
            "by_caller":    by_caller,
            "by_day":       by_day,
        }))
    }

    /// Get aggregated usage statistics for a model.
    ///
    /// `window_hours` restricts the aggregation to the last N hours — quota
    /// budgets are time-windowed, so all-time totals are useless for "how much
    /// have I used today". `None` keeps the historical all-time view.
    pub fn get_model_stats(
        &self,
        model_name: Option<&str>,
        window_hours: Option<i64>,
    ) -> Result<serde_json::Value> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        // Compute the cutoff in Rust — DuckDB's TIMESTAMPTZ minus INTERVAL
        // binding is version-dependent, a literal timestamp comparison is not.
        let cutoff: Option<String> =
            window_hours.map(|h| (Utc::now() - chrono::Duration::hours(h)).to_rfc3339());
        let window_clause = if cutoff.is_some() {
            "created_at >= CAST(? AS TIMESTAMPTZ)"
        } else {
            "1 = 1"
        };

        // When no model is specified, return per-model stats for all models.
        if model_name.is_none() {
            let sql = format!(
                "SELECT
                   model_name,
                   COUNT(*) AS total,
                   SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
                   SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures,
                   AVG(duration_ms) AS avg_duration_ms,
                   SUM(tokens_in)  AS total_tokens_in,
                   SUM(tokens_out) AS total_tokens_out,
                   SUM(cost)       AS total_cost,
                   SUM(CASE WHEN error_kind = 'rate_limited' THEN 1 ELSE 0 END) AS rate_limited
                 FROM model_usage
                 WHERE {window_clause}
                 GROUP BY model_name
                 ORDER BY total DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let map_row = |row: &duckdb::Row<'_>| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<f64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                ))
            };
            let collected: Vec<_> = if let Some(ref c) = cutoff {
                stmt.query_map(params![c], map_row)?
                    .filter_map(|r| r.ok())
                    .collect()
            } else {
                stmt.query_map([], map_row)?
                    .filter_map(|r| r.ok())
                    .collect()
            };
            let rows: Vec<serde_json::Value> = collected
                .into_iter()
                .map(
                    |(model, total, succ, fail, avg_ms, tin, tout, cost, rate_limited)| {
                        let successes = succ.unwrap_or(0);
                        let failures = fail.unwrap_or(0);
                        let success_rate = if total > 0 {
                            successes as f64 / total as f64
                        } else {
                            0.0
                        };
                        serde_json::json!({
                            "model":           model,
                            "total_calls":     total,
                            "successes":       successes,
                            "failures":        failures,
                            "success_rate":    success_rate,
                            "avg_duration_ms": avg_ms,
                            "total_tokens_in": tin,
                            "total_tokens_out": tout,
                            "total_cost":      cost,
                            "rate_limited":    rate_limited.unwrap_or(0),
                        })
                    },
                )
                .collect();
            return Ok(serde_json::json!({
                "window_hours": window_hours,
                "models": rows,
            }));
        }

        let name = model_name.unwrap();
        let sql = format!(
            "SELECT
               COUNT(*) AS total,
               SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
               SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures,
               AVG(duration_ms) AS avg_duration_ms,
               SUM(tokens_in)  AS total_tokens_in,
               SUM(tokens_out) AS total_tokens_out
             FROM model_usage
             WHERE model_name = ? AND {window_clause}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = if let Some(ref c) = cutoff {
            stmt.query(params![name, c])?
        } else {
            stmt.query(params![name])?
        };
        if let Some(row) = rows.next()? {
            let total: i64 = row.get(0)?;
            let successes: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
            let failures: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
            let avg_ms: Option<f64> = row.get(3)?;
            let tokens_in: Option<i64> = row.get(4)?;
            let tokens_out: Option<i64> = row.get(5)?;
            let success_rate = if total > 0 {
                successes as f64 / total as f64
            } else {
                0.0
            };
            Ok(serde_json::json!({
                "model":             name,
                "total_calls":       total,
                "successes":         successes,
                "failures":          failures,
                "success_rate":      success_rate,
                "avg_duration_ms":   avg_ms,
                "total_tokens_in":   tokens_in,
                "total_tokens_out":  tokens_out,
            }))
        } else {
            Ok(serde_json::json!({
                "model": name,
                "total_calls": 0,
                "success_rate": 0.0,
            }))
        }
    }

    // =========================================================================
    // Training data export
    // =========================================================================

    /// Execute a read-only SQL query and return results as a JSON array.
    ///
    /// Exactly one `SELECT` (CTEs and set operations included) runs, judged by
    /// DuckDB's own parser (`json_serialize_sql`), not by scanning the text:
    /// before 2026-10-16 a keyword scan let `SELECT 1 LIMIT 1;DROP TABLE t`
    /// run its `DROP`, and never knew `COPY`, `ATTACH`, `INSTALL` or `PRAGMA`.
    /// A `LIMIT` clause is appended automatically if the query does not
    /// already contain one.
    pub fn query_raw(&self, sql: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        use duckdb::types::ValueRef;

        let upper = sql.trim().to_uppercase();
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        check_single_select(&conn, sql)?;

        let base = sql.trim().trim_end_matches(';');
        let limited = if upper.contains(" LIMIT ") {
            base.to_string()
        } else {
            format!("{} LIMIT {}", base, limit)
        };

        let mut stmt = conn.prepare(&limited)?;

        // duckdb 1.4.x: both column_count() and column_names() read from the
        // execution result (RawStatement::result), which is None until the
        // statement actually runs.  Calling either before execution panics.
        //
        // Workaround:
        //   1. Execute via query_map; inside the closure probe columns by index
        //      until get_ref() returns InvalidColumnIndex to learn the count.
        //   2. After the MappedRows iterator is consumed (result populated),
        //      call column_names() — now safe — and re-key the collected rows.
        let mut observed_col_count: usize = 0;

        let raw_rows: Vec<Vec<serde_json::Value>> = stmt
            .query_map([], |row| {
                let mut vals: Vec<serde_json::Value> = Vec::new();
                loop {
                    let i = vals.len();
                    match row.get_ref(i) {
                        Err(_) => {
                            // Out-of-bounds or other error — stop probing.
                            break;
                        }
                        Ok(vref) => {
                            let json_val = match vref {
                                ValueRef::Null => serde_json::Value::Null,
                                ValueRef::Boolean(b) => serde_json::Value::Bool(b),
                                ValueRef::TinyInt(n) => serde_json::json!(n),
                                ValueRef::SmallInt(n) => serde_json::json!(n),
                                ValueRef::Int(n) => serde_json::json!(n),
                                ValueRef::BigInt(n) => serde_json::json!(n),
                                ValueRef::HugeInt(n) => serde_json::json!(n.to_string()),
                                ValueRef::UTinyInt(n) => serde_json::json!(n),
                                ValueRef::USmallInt(n) => serde_json::json!(n),
                                ValueRef::UInt(n) => serde_json::json!(n),
                                ValueRef::UBigInt(n) => serde_json::json!(n),
                                ValueRef::Float(f) => serde_json::json!(f),
                                ValueRef::Double(f) => serde_json::json!(f),
                                ValueRef::Text(t) => serde_json::Value::String(
                                    std::str::from_utf8(t).unwrap_or("").to_string(),
                                ),
                                _ => serde_json::Value::String("(unsupported type)".to_string()),
                            };
                            vals.push(json_val);
                        }
                    }
                }
                Ok(vals)
            })?
            .filter_map(|r| r.ok())
            .inspect(|vals| {
                if observed_col_count == 0 && !vals.is_empty() {
                    observed_col_count = vals.len();
                }
            })
            .collect();

        // MappedRows consumed — result is populated, column_names() is now safe.
        // Fall back to numeric names if it still panics (extra safety net).
        let col_names: Vec<String> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stmt.column_names()))
                .unwrap_or_else(|_| {
                    (0..observed_col_count)
                        .map(|i| format!("col_{i}"))
                        .collect()
                });

        let rows = raw_rows
            .into_iter()
            .map(|vals| {
                let mut obj = serde_json::Map::new();
                for (i, val) in vals.into_iter().enumerate() {
                    let key = col_names
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("col_{i}"));
                    obj.insert(key, val);
                }
                serde_json::Value::Object(obj)
            })
            .collect();

        Ok(rows)
    }
}

#[cfg(test)]
mod selection_order_tests {
    use super::*;

    /// Register a model with only the fields the ordering reads.
    fn add(db: &TelemetryClient, name: &str, cost: f64, rank: Option<i64>, ctx: i64, caps: &str) {
        db.upsert_model(
            name,
            "ollama-cloud",
            name,
            ctx,
            cost,
            0.0,
            caps,
            None,
            None,
            None,
            None,
            rank,
        )
        .unwrap();
    }

    fn client() -> TelemetryClient {
        TelemetryClient::new(":memory:").unwrap()
    }

    fn winner(db: &TelemetryClient, cap: &str) -> String {
        db.select_models(&[cap.to_string()], None, None).unwrap()[0]["model"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn rank_beats_context_window_among_cost_tied_models() {
        // The regression this whole field exists for: a new $0 entry with a
        // huge context window must NOT capture the capability just by being
        // wide. `big` would win under the old `cost ASC, context_window DESC`.
        let db = client();
        add(&db, "preferred", 0.0, Some(10), 131_072, r#"["reasoning"]"#);
        add(&db, "big", 0.0, Some(50), 524_288, r#"["reasoning"]"#);
        assert_eq!(winner(&db, "reasoning"), "preferred");
    }

    #[test]
    fn cost_still_outranks_rank() {
        // Rank breaks ties; it must never let an expensive model jump a free
        // one, or a tier-2 deployment starts paying for steps it need not.
        let db = client();
        add(
            &db,
            "free-but-last",
            0.0,
            Some(900),
            8_192,
            r#"["reasoning"]"#,
        );
        add(
            &db,
            "paid-but-first",
            0.03,
            Some(1),
            1_000_000,
            r#"["reasoning"]"#,
        );
        assert_eq!(winner(&db, "reasoning"), "free-but-last");
    }

    #[test]
    fn unranked_models_sort_behind_ranked_ones() {
        // Omitting selection_rank must be safe: an entry that does not opt in
        // cannot displace one that was deliberately ranked.
        let db = client();
        add(&db, "ranked", 0.0, Some(70), 4_096, r#"["reasoning"]"#);
        add(&db, "unranked", 0.0, None, 999_999, r#"["reasoning"]"#);
        assert_eq!(winner(&db, "reasoning"), "ranked");
    }

    #[test]
    fn unranked_models_keep_widest_window_order_among_themselves() {
        // Legacy behaviour is preserved where nothing has opted in, so adding
        // the column does not silently reorder an unranked catalog.
        let db = client();
        add(&db, "narrow", 0.0, None, 4_096, r#"["reasoning"]"#);
        add(&db, "wide", 0.0, None, 262_144, r#"["reasoning"]"#);
        assert_eq!(winner(&db, "reasoning"), "wide");
    }

    #[test]
    fn rank_is_global_so_a_capability_filter_can_change_the_winner() {
        // Rank is one number across all capabilities; the capability filter is
        // what makes a lower-ranked model win. This is the `vision` case in
        // models.yaml — the top-ranked reasoning models have no vision, so the
        // vision winner is a model ranked below them.
        let db = client();
        add(&db, "text-only", 0.0, Some(10), 131_072, r#"["reasoning"]"#);
        add(
            &db,
            "sees",
            0.0,
            Some(30),
            262_144,
            r#"["reasoning","vision"]"#,
        );
        assert_eq!(winner(&db, "reasoning"), "text-only");
        assert_eq!(winner(&db, "vision"), "sees");
    }
}

#[cfg(test)]
mod search_ledger_tests {
    use super::*;

    fn attempt(engine: &str, success: bool, units: Option<f64>) -> SearchAttempt {
        SearchAttempt {
            call_id: "c1".into(),
            engine: engine.into(),
            query: "q".into(),
            success,
            result_count: success.then_some(2),
            units,
            picked: "rotation".into(),
            tool: Some("claim".into()),
            purpose: Some("background".into()),
            dated: success.then_some(1),
            known_source: success.then_some(1),
            links: if success {
                vec!["https://a.example/1".into(), "https://b.example/2".into()]
            } else {
                vec![]
            },
            ..Default::default()
        }
    }

    #[test]
    fn an_attempt_is_counted_priced_and_its_links_kept() {
        let db = TelemetryClient::new(":memory:").unwrap();
        db.record_search_attempt(&attempt("exa", true, Some(0.007)))
            .unwrap();
        db.record_search_attempt(&attempt("exa", true, None))
            .unwrap();
        // A failed call is not billed: it counts toward rate limits only.
        db.record_search_attempt(&attempt("exa", false, None))
            .unwrap();
        let month = Utc::now() - chrono::Duration::days(1);
        let u = db.engine_use("exa", month).unwrap();
        assert!((u.period_units - 0.007).abs() < 1e-9);
        assert_eq!(u.period_unpriced, 1);
        assert_eq!((u.day_calls, u.hour_calls, u.minute_calls), (3, 3, 3));
        assert_eq!(db.engine_use("tavily", month).unwrap().day_calls, 0);

        let links = db.search_links_since(1).unwrap();
        assert_eq!(links.len(), 4);
        assert!(links.iter().all(|(e, _)| e == "exa"));

        let stats = db.get_search_stats(Some(24)).unwrap();
        let exa = &stats["engines"][0];
        assert_eq!(exa["total_searches"], 3);
        assert_eq!(exa["picked"]["rotation"], 3);
        assert_eq!(exa["dated_share"], 0.5);
        assert_eq!(stats["by_caller"][0]["caller"], "claim");
        assert_eq!(stats["by_caller"][0]["calls"], 1);
    }
}

/// Refuse anything but one `SELECT`, using DuckDB's parser: it serializes
/// only SELECT statements and lists each statement it parsed. Fails closed:
/// if the check itself cannot run, the query does not either.
fn check_single_select(conn: &Connection, sql: &str) -> Result<()> {
    let serialized: String = conn
        .query_row(
            "SELECT json_serialize_sql(CAST(? AS VARCHAR))",
            [sql],
            |r| r.get(0),
        )
        .context("could not check the query is a single SELECT")?;
    let v: serde_json::Value =
        serde_json::from_str(&serialized).context("unreadable query check")?;
    if v["error"].as_bool().unwrap_or(true) {
        let why = v["error_message"].as_str().unwrap_or("not a SELECT");
        anyhow::bail!(
            "only a single SELECT runs here ({why}). For a table's columns, \
             SELECT column_name, data_type FROM information_schema.columns WHERE table_name = '<t>'"
        );
    }
    let n = v["statements"].as_array().map_or(0, Vec::len);
    if n != 1 {
        anyhow::bail!("only a single SELECT runs here; this has {n} statements");
    }
    Ok(())
}

#[cfg(test)]
mod query_raw_tests {
    use super::*;

    fn client() -> TelemetryClient {
        TelemetryClient::new(":memory:").expect("in-memory duckdb")
    }

    #[test]
    fn a_select_and_a_cte_run() {
        let t = client();
        assert_eq!(t.query_raw("SELECT 1 AS a", 10).unwrap().len(), 1);
        assert_eq!(
            t.query_raw(
                "WITH x AS (SELECT 1 AS a) SELECT a FROM x UNION ALL SELECT 2",
                10
            )
            .unwrap()
            .len(),
            2
        );
    }

    /// The 2026-10-16 bypass: a LIMIT already present meant nothing was
    /// appended, and the keyword scan never saw `;DROP`.
    #[test]
    fn a_second_statement_is_refused_before_it_runs() {
        let t = client();
        t.query_raw("SELECT count(*) AS n FROM model_usage", 1)
            .unwrap();
        let err = t
            .query_raw("SELECT 1 AS a LIMIT 1;DROP TABLE model_usage", 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only a single SELECT runs here"), "{err}");
        t.query_raw("SELECT count(*) AS n FROM model_usage", 1)
            .expect("model_usage must still exist");
    }

    /// json must be in the binary, not fetched: with external access locked
    /// off before any json use, `json_serialize_sql` (the SELECT check) still
    /// runs. Fails if the `json` duckdb feature is dropped.
    #[test]
    fn json_is_compiled_in() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;")
            .unwrap();
        let out: String = conn
            .query_row("SELECT json_serialize_sql('SELECT 1')::VARCHAR", [], |r| {
                r.get(0)
            })
            .expect("json extension must be compiled in");
        assert!(out.contains("SELECT_NODE"), "{out}");
    }

    /// A SELECT that reads outside the database: local files, globs, URLs.
    /// `enable_external_access = false` (set at open, then locked) refuses
    /// them; before 2026-10-16 `SELECT * FROM read_csv('/etc/passwd')` ran.
    #[test]
    fn a_select_cannot_read_files_or_urls() {
        let t = client();
        for sql in [
            "SELECT * FROM read_csv('/etc/passwd')",
            "SELECT * FROM read_text('/etc/hostname')",
            "SELECT * FROM glob('/*')",
            "SELECT * FROM read_json_auto('https://example.com/x.json')",
        ] {
            let err = t.query_raw(sql, 10).unwrap_err().to_string();
            assert!(
                !err.contains("only a single SELECT"),
                "{sql} should reach DuckDB: {err}"
            );
            assert!(
                err.to_lowercase().contains("external") || err.to_lowercase().contains("disabled"),
                "{sql}: {err}"
            );
        }
    }

    /// What the brain itself uses still works with external access off.
    #[test]
    fn json_and_ordinary_reads_still_work() {
        let t = client();
        let rows = t
            .query_raw(
                "SELECT json_keys('{\"a\": 1}') AS k, regexp_extract('ab1', '[0-9]') AS d",
                1,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        t.query_raw("SELECT table_name FROM information_schema.tables", 50)
            .unwrap();
    }

    /// Locked: the setting cannot be switched back on from inside.
    #[test]
    fn external_access_cannot_be_turned_back_on() {
        let t = client();
        let conn = t.conn.lock().unwrap();
        assert!(
            conn.execute_batch("SET enable_external_access = true")
                .is_err()
        );
    }

    /// A write hidden in a SELECT: `query()` runs a string as SQL.
    #[test]
    fn a_write_inside_query_function_does_not_run() {
        let t = client();
        let _ = t.query_raw("SELECT * FROM query('DROP TABLE model_usage')", 10);
        t.query_raw("SELECT count(*) AS n FROM model_usage", 1)
            .expect("model_usage must still exist");
    }

    #[test]
    fn writes_and_side_effects_are_refused() {
        let t = client();
        for sql in [
            "CREATE TABLE zz (a INT)",
            "COPY (SELECT 1) TO '/tmp/zz_probe.csv'",
            "ATTACH ':memory:' AS zz",
            "INSTALL httpfs",
            "PRAGMA version",
            "SET threads = 1",
        ] {
            let err = t.query_raw(sql, 10).unwrap_err().to_string();
            assert!(
                err.contains("only a single SELECT runs here"),
                "{sql}: {err}"
            );
        }
    }
}

#[cfg(test)]
mod model_usage_task_tests {
    use super::*;

    /// A call made under a workflow run is attributable to it: cost per
    /// workflow is a join on `task_id` (Programs plan Phase 0).
    #[test]
    fn a_call_records_the_task_that_owned_it() {
        let t = TelemetryClient::new(":memory:").unwrap();
        t.record_model_usage(
            "m",
            Some("p"),
            Some("reason"),
            true,
            Some(1),
            Some(1000),
            Some(0),
            None,
            Some("task-1"),
        )
        .unwrap();
        t.record_model_usage(
            "m",
            Some("p"),
            Some("chat"),
            true,
            Some(1),
            Some(1000),
            Some(0),
            None,
            None,
        )
        .unwrap();
        let rows = t
            .query_raw(
                "SELECT tool_name, task_id FROM model_usage ORDER BY tool_name",
                10,
            )
            .unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["task_id"], serde_json::Value::Null);
        assert_eq!(rows[1]["task_id"], "task-1");
    }
}

#[cfg(test)]
mod agent_attribution_tests {
    use super::*;

    /// An agent's runs and the misfit arguments it sent are attributable to
    /// it: the agent scorecard reads both (Programs plan Phase 4).
    #[test]
    fn runs_and_misfits_record_their_agent() {
        let t = TelemetryClient::new(":memory:").unwrap();
        t.record_agent_run("researcher", "chat", "m", true, 4, 1, 1200, None)
            .unwrap();
        t.record_agent_run(
            "researcher",
            "background",
            "m",
            false,
            2,
            2,
            900,
            Some("task-1"),
        )
        .unwrap();
        t.log_arg_check(
            "search_web",
            "subagent",
            "shadow",
            false,
            &["x".into()],
            "{}",
            Some("researcher"),
        )
        .unwrap();
        let conn = t.conn.lock().unwrap();
        let (runs, ok, failed): (i64, i64, i64) = conn
            .query_row(
                "SELECT count(*), sum(CASE WHEN ok THEN 1 ELSE 0 END), sum(tools_failed) FROM agent_runs WHERE agent = 'researcher'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((runs, ok, failed), (2, 1, 3));
        let agent: String = conn
            .query_row("SELECT agent FROM arg_checks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(agent, "researcher");
    }
}
