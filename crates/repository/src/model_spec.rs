use chrono::Utc;
use neo4rs::{query, Node};
use tracing::info;
use uuid::Uuid;

use agent_brain_models::ModelSpec;
use crate::{Neo4jClient, RepositoryError};

fn node_to_model_spec(node: &Node) -> ModelSpec {
    let capabilities_str: String = node.get("capabilities").unwrap_or_default();
    let capabilities = if capabilities_str.is_empty() {
        vec![]
    } else {
        capabilities_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    ModelSpec {
        id: node.get("id").unwrap_or_default(),
        name: node.get("name").unwrap_or_default(),
        provider: node.get("provider").unwrap_or_default(),
        cost_per_1k_tokens_input: node.get::<f64>("cost_per_1k_tokens_input").unwrap_or(0.0),
        cost_per_1k_tokens_output: node.get::<f64>("cost_per_1k_tokens_output").unwrap_or(0.0),
        context_window: node.get::<i64>("context_window").unwrap_or(0) as u32,
        capabilities,
        created_at: node.get("created_at").unwrap_or_default(),
    }
}

impl Neo4jClient {
    /// Register or update a model specification (upsert by name).
    pub async fn register_model_spec(&self, spec: &ModelSpec) -> Result<String, RepositoryError> {
        let new_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let capabilities_str = spec.capabilities.join(",");

        // MERGE on name so re-registering a model updates its properties.
        let q = query(
            "MERGE (m:ModelSpec {name: $name})
             ON CREATE SET
               m.id = $id,
               m.created_at = $now
             SET
               m.provider = $provider,
               m.cost_per_1k_tokens_input = $cost_input,
               m.cost_per_1k_tokens_output = $cost_output,
               m.context_window = $context_window,
               m.capabilities = $capabilities
             RETURN m.id AS id",
        )
        .param("id", new_id)
        .param("name", spec.name.clone())
        .param("provider", spec.provider.clone())
        .param("cost_input", spec.cost_per_1k_tokens_input)
        .param("cost_output", spec.cost_per_1k_tokens_output)
        .param("context_window", spec.context_window as i64)
        .param("capabilities", capabilities_str)
        .param("now", now);

        let rows = self.execute(q).await?;
        let id = rows
            .into_iter()
            .next()
            .and_then(|row| row.get::<String>("id").ok())
            .unwrap_or_default();

        info!(name = %spec.name, provider = %spec.provider, "Registered ModelSpec");
        Ok(id)
    }

    /// List all registered model specifications, ordered by provider then name.
    pub async fn list_model_specs(&self) -> Result<Vec<ModelSpec>, RepositoryError> {
        let q = query("MATCH (m:ModelSpec) RETURN m ORDER BY m.provider ASC, m.name ASC");
        let rows = self.execute(q).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let node: Node = row
                    .get("m")
                    .map_err(|e| RepositoryError::InvalidData(e.to_string()))
                    .expect("ModelSpec node");
                node_to_model_spec(&node)
            })
            .collect())
    }

    /// Get a model spec by name (exact match).
    pub async fn get_model_spec_by_name(
        &self,
        name: &str,
    ) -> Result<Option<ModelSpec>, RepositoryError> {
        let q = query("MATCH (m:ModelSpec {name: $name}) RETURN m").param("name", name);
        let rows = self.execute(q).await?;
        Ok(rows.into_iter().next().map(|row| {
            let node: Node = row
                .get("m")
                .map_err(|e| RepositoryError::InvalidData(e.to_string()))
                .expect("ModelSpec node");
            node_to_model_spec(&node)
        }))
    }

    /// Get job-based usage statistics for a given provider_hint value.
    pub async fn get_model_usage_stats(
        &self,
        model_or_provider: &str,
    ) -> Result<serde_json::Value, RepositoryError> {
        // Match jobs where provider_hint equals the given string.
        let q = query(
            "MATCH (j:AgentJob)
             WHERE j.provider_hint = $hint
             RETURN
               count(j) AS total,
               count(CASE WHEN j.status = 'completed' THEN 1 END) AS completed,
               count(CASE WHEN j.status = 'failed' THEN 1 END) AS failed,
               count(CASE WHEN j.status = 'dead' THEN 1 END) AS dead,
               count(CASE WHEN j.status = 'running' THEN 1 END) AS running",
        )
        .param("hint", model_or_provider);

        let rows = self.execute(q).await?;
        if let Some(row) = rows.into_iter().next() {
            let total: i64 = row.get("total").unwrap_or(0);
            let completed: i64 = row.get("completed").unwrap_or(0);
            let failed: i64 = row.get("failed").unwrap_or(0);
            let dead: i64 = row.get("dead").unwrap_or(0);
            let running: i64 = row.get("running").unwrap_or(0);
            let success_rate = if total > 0 {
                completed as f64 / total as f64
            } else {
                0.0
            };
            Ok(serde_json::json!({
                "model": model_or_provider,
                "total_jobs": total,
                "completed": completed,
                "failed": failed,
                "dead": dead,
                "running": running,
                "success_rate": success_rate,
            }))
        } else {
            Ok(serde_json::json!({
                "model": model_or_provider,
                "total_jobs": 0,
                "success_rate": 0.0,
            }))
        }
    }
}
