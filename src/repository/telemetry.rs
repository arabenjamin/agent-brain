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
                feedback_score INTEGER, -- User feedback (1-5) or null
                feedback_text TEXT,     -- User comments
                latency_ms INTEGER,
                model_used TEXT
            );
            
            -- Table: knowledge_gaps
            -- Explicitly logs when the agent said 'I don't know' or failed a tool call.
            CREATE TABLE IF NOT EXISTS knowledge_gaps (
                id UUID PRIMARY KEY,
                timestamp TIMESTAMPTZ NOT NULL,
                query TEXT NOT NULL,
                context TEXT,           -- What we were doing
                gap_type TEXT           -- 'missing_tool', 'missing_info', 'api_error'
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
}
