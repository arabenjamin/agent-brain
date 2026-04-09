use anyhow::{Context, Result};
use chrono::Utc;
use duckdb::{Connection, params};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Client for the local DuckDB telemetry store.
/// This serves as the "Hippocampus" - storing raw experiences for later "sleep" (fine-tuning).
#[derive(Clone)]
pub struct TelemetryClient {
    // DuckDB connection is not thread-safe by default, so we wrap it.
    // In a high-throughput scenario, we might use a pool or r2d2-duckdb,
    // but for an agent brain, a mutex is usually fine for now.
    conn: Arc<Mutex<Connection>>,
}

impl TelemetryClient {
    /// Create a new TelemetryClient backed by a file.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open DuckDB file")?;

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
                loaded_at      TIMESTAMPTZ DEFAULT current_timestamp
            );

            CREATE TABLE IF NOT EXISTS model_usage (
                id             TEXT PRIMARY KEY,
                model_name     TEXT NOT NULL,
                tool_name      TEXT,
                success        BOOLEAN,
                duration_ms    INTEGER,
                tokens_in      INTEGER,
                tokens_out     INTEGER,
                cost           DOUBLE,
                created_at     TIMESTAMPTZ DEFAULT current_timestamp
            );
            ",
        )?;

        info!("Telemetry (DuckDB) schema initialized");
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

    /// Retrieve recent knowledge gaps for analysis.
    pub fn get_recent_gaps(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        let mut stmt = conn.prepare(
            "SELECT query, COALESCE(context, ''), gap_type 
             FROM knowledge_gaps 
             ORDER BY timestamp DESC 
             LIMIT ?",
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;

        let mut gaps = Vec::new();
        for row in rows {
            gaps.push(row?);
        }

        Ok(gaps)
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
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        conn.execute(
            "INSERT OR REPLACE INTO model_registry
             (name, provider, model, context_window, cost_input, cost_output,
              capabilities, system_prompt, temperature, max_tokens, timeout_secs, loaded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, current_timestamp)",
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
                timeout_secs
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

    /// Return the system_prompt for a given model name, or None if not found.
    pub fn get_model_system_prompt(&self, name: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let mut stmt = conn.prepare("SELECT system_prompt FROM model_registry WHERE name = ?")?;
        let mut rows = stmt.query(params![name])?;
        if let Some(row) = rows.next()? {
            Ok(row.get(0)?)
        } else {
            Ok(None)
        }
    }

    /// Select models that satisfy capability and cost constraints.
    ///
    /// Returns rows ordered by total cost ascending, then context_window descending.
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
            "SELECT name, provider, model, context_window, cost_input, cost_output, capabilities
             FROM model_registry
             WHERE provider = ? AND (cost_input + cost_output) <= ?
             ORDER BY (cost_input + cost_output) ASC, context_window DESC"
        } else {
            "SELECT name, provider, model, context_window, cost_input, cost_output, capabilities
             FROM model_registry
             WHERE (cost_input + cost_output) <= ?
             ORDER BY (cost_input + cost_output) ASC, context_window DESC"
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
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut out = Vec::new();
        for (name, provider, model, ctx, cost_in, cost_out, caps_str) in rows {
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
                }));
            }
        }
        Ok(out)
    }

    // =========================================================================
    // Model usage tracking
    // =========================================================================

    /// Record a single model invocation.
    pub fn record_model_usage(
        &self,
        model_name: &str,
        tool_name: Option<&str>,
        success: bool,
        duration_ms: Option<i64>,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        let id = uuid::Uuid::new_v4().to_string();
        // Compute cost from registry rates if available.
        let cost: Option<f64> = None; // populated by a separate query if needed
        conn.execute(
            "INSERT INTO model_usage
             (id, model_name, tool_name, success, duration_ms, tokens_in, tokens_out, cost)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                model_name,
                tool_name,
                success,
                duration_ms,
                tokens_in,
                tokens_out,
                cost
            ],
        )?;
        Ok(())
    }

    /// Get aggregated usage statistics for a model.
    pub fn get_model_stats(&self, model_name: Option<&str>) -> Result<serde_json::Value> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        // When no model is specified, return per-model stats for all models.
        if model_name.is_none() {
            let mut stmt = conn.prepare(
                "SELECT
                   model_name,
                   COUNT(*) AS total,
                   SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
                   SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures,
                   AVG(duration_ms) AS avg_duration_ms,
                   SUM(tokens_in)  AS total_tokens_in,
                   SUM(tokens_out) AS total_tokens_out
                 FROM model_usage
                 GROUP BY model_name
                 ORDER BY total DESC",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<f64>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .map(|(model, total, succ, fail, avg_ms, tin, tout)| {
                    let successes = succ.unwrap_or(0);
                    let failures = fail.unwrap_or(0);
                    let success_rate = if total > 0 { successes as f64 / total as f64 } else { 0.0 };
                    serde_json::json!({
                        "model":           model,
                        "total_calls":     total,
                        "successes":       successes,
                        "failures":        failures,
                        "success_rate":    success_rate,
                        "avg_duration_ms": avg_ms,
                        "total_tokens_in": tin,
                        "total_tokens_out": tout,
                    })
                })
                .collect();
            return Ok(serde_json::json!({ "models": rows }));
        }

        let name = model_name.unwrap();
        let mut stmt = conn.prepare(
            "SELECT
               COUNT(*) AS total,
               SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes,
               SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures,
               AVG(duration_ms) AS avg_duration_ms,
               SUM(tokens_in)  AS total_tokens_in,
               SUM(tokens_out) AS total_tokens_out
             FROM model_usage
             WHERE model_name = ?",
        )?;
        let mut rows = stmt.query(params![name])?;
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

    /// Export successful interactions for fine-tuning.
    /// Returns a list of (prompt, response) tuples.
    pub fn get_training_examples(&self, min_score: Option<i32>) -> Result<Vec<(String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        let sql = if let Some(score) = min_score {
            // Get explicitly rated good responses
            format!(
                "SELECT prompt, response FROM interactions WHERE success = true AND feedback_score >= {}",
                score
            )
        } else {
            // Get all successful responses
            "SELECT prompt, response FROM interactions WHERE success = true".to_string()
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

        let mut examples = Vec::new();
        for row in rows {
            examples.push(row?);
        }

        Ok(examples)
    }

    /// Execute a read-only SQL query and return results as a JSON array.
    ///
    /// Write operations (`INSERT`, `UPDATE`, `DELETE`, `DROP`, `CREATE`, `ALTER`,
    /// `TRUNCATE`) are rejected with an error.  A `LIMIT` clause is appended
    /// automatically if the query does not already contain one.
    pub fn query_raw(&self, sql: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        use duckdb::types::ValueRef;

        let upper = sql.trim().to_uppercase();
        for kw in &[
            "INSERT", "UPDATE", "DELETE", "DROP", "CREATE", "ALTER", "TRUNCATE",
        ] {
            if upper.split_whitespace().any(|w| w == *kw) {
                anyhow::bail!("Write operations are not allowed via query_raw (keyword: {})", kw);
            }
        }

        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        let base = sql.trim().trim_end_matches(';');
        let limited = if upper.contains(" LIMIT ") {
            base.to_string()
        } else {
            format!("{} LIMIT {}", base, limit)
        };

        let mut stmt = conn.prepare(&limited)?;
        let col_names: Vec<String> = stmt.column_names();
        let col_count = stmt.column_count();

        let rows: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                let mut obj = serde_json::Map::new();
                for i in 0..col_count {
                    let json_val = match row.get_ref(i)? {
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
                    obj.insert(col_names[i].clone(), json_val);
                }
                Ok(serde_json::Value::Object(obj))
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }
}
