use chrono::{DateTime, Utc};
use neo4rs::{Row, query};
use uuid::Uuid;

use super::{Neo4jClient, RepositoryError, Result};
use crate::models::{HealingAction, HealingEvent};

impl Neo4jClient {
    /// Create a new HealingEvent node and link it to the Endpoint.
    pub async fn create_healing_event(&self, event: &HealingEvent) -> Result<()> {
        let action_json = serde_json::to_string(&event.action)?;

        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id})
             CREATE (h:HealingEvent {
                 id: $id,
                 endpoint_id: $endpoint_id,
                 timestamp: datetime($timestamp),
                 action: $action,
                 trigger_error: $trigger_error,
                 ai_reasoning: $ai_reasoning,
                 verified: $verified
             })
             MERGE (e)-[:HAS_HISTORY]->(h)",
        )
        .param("id", event.id.to_string())
        .param("endpoint_id", event.endpoint_id.to_string())
        .param("timestamp", event.timestamp.to_rfc3339())
        .param("action", action_json)
        .param("trigger_error", event.trigger_error.clone())
        .param("ai_reasoning", event.ai_reasoning.clone())
        .param("verified", event.verified);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Find a HealingEvent by ID.
    pub async fn get_healing_event(&self, id: Uuid) -> Result<HealingEvent> {
        let q = query("MATCH (h:HealingEvent {id: $id}) RETURN h").param("id", id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            row_to_healing_event(row)
        } else {
            Err(RepositoryError::NotFound {
                entity: "HealingEvent",
                id: id.to_string(),
            })
        }
    }

    /// Get all HealingEvents for an Endpoint.
    pub async fn get_healing_history(&self, endpoint_id: Uuid) -> Result<Vec<HealingEvent>> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id})-[:HAS_HISTORY]->(h:HealingEvent)
             RETURN h
             ORDER BY h.timestamp DESC",
        )
        .param("endpoint_id", endpoint_id.to_string());

        let mut result = self.graph().execute(q).await?;
        let mut events = Vec::new();

        while let Some(row) = result.next().await? {
            events.push(row_to_healing_event(row)?);
        }

        Ok(events)
    }

    /// Get all unverified HealingEvents.
    pub async fn get_unverified_healing_events(&self) -> Result<Vec<HealingEvent>> {
        let q = query(
            "MATCH (h:HealingEvent {verified: false})
             RETURN h
             ORDER BY h.timestamp DESC",
        );

        let mut result = self.graph().execute(q).await?;
        let mut events = Vec::new();

        while let Some(row) = result.next().await? {
            events.push(row_to_healing_event(row)?);
        }

        Ok(events)
    }

    /// Mark a HealingEvent as verified.
    pub async fn verify_healing_event(&self, id: Uuid) -> Result<()> {
        let q = query(
            "MATCH (h:HealingEvent {id: $id})
             SET h.verified = true",
        )
        .param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Get statistics about healing events.
    pub async fn get_healing_stats(&self) -> Result<HealingStats> {
        let q = query(
            "MATCH (h:HealingEvent)
             RETURN
                 count(h) as total,
                 sum(CASE WHEN h.verified THEN 1 ELSE 0 END) as verified,
                 sum(CASE WHEN NOT h.verified THEN 1 ELSE 0 END) as unverified",
        );

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            let total: i64 = row.get("total").unwrap_or(0);
            let verified: i64 = row.get("verified").unwrap_or(0);
            let unverified: i64 = row.get("unverified").unwrap_or(0);

            Ok(HealingStats {
                total: total as u64,
                verified: verified as u64,
                unverified: unverified as u64,
            })
        } else {
            Ok(HealingStats::default())
        }
    }

    /// Apply a healing action to the graph.
    pub async fn apply_healing_action(
        &self,
        endpoint_id: Uuid,
        action: &HealingAction,
    ) -> Result<()> {
        match action {
            HealingAction::RenameParameter {
                param_id, new_name, ..
            } => {
                self.update_parameter_name(*param_id, new_name).await?;
            }
            HealingAction::ChangeParameterType {
                param_name,
                new_type,
                ..
            } => {
                // Find the parameter by name for the endpoint and update type
                let q = query(
                    "MATCH (e:Endpoint {id: $endpoint_id})-[:REQUIRES_PARAM]->(p:Parameter {name: $param_name})
                     SET p.param_type = $new_type, p.last_updated = datetime()"
                )
                .param("endpoint_id", endpoint_id.to_string())
                .param("param_name", param_name.clone())
                .param("new_type", new_type.clone());
                self.graph().run(q).await?;
            }
            HealingAction::AddMissingParameter {
                param_name,
                required,
                ..
            } => {
                // Create new parameter and link to endpoint
                let param = crate::models::Parameter::new(
                    param_name.clone(),
                    crate::models::ParameterLocation::Query, // Default, could be inferred
                    *required,
                );
                self.create_parameter(&param).await?;
                self.link_endpoint_to_parameter(endpoint_id, param.id)
                    .await?;
            }
            HealingAction::UpdateEndpointPath { new_path, .. } => {
                let q = query(
                    "MATCH (e:Endpoint {id: $endpoint_id})
                     SET e.path = $new_path, e.last_updated = datetime()",
                )
                .param("endpoint_id", endpoint_id.to_string())
                .param("new_path", new_path.clone());
                self.graph().run(q).await?;
            }
            HealingAction::UpdateResponseSchema {
                status_code,
                diff_summary,
            } => {
                // Log the schema update - actual schema update would require more info
                let q = query(
                    "MATCH (e:Endpoint {id: $endpoint_id})
                     SET e.schema_notes = $notes, e.last_updated = datetime()",
                )
                .param("endpoint_id", endpoint_id.to_string())
                .param("notes", format!("Status {}: {}", status_code, diff_summary));
                self.graph().run(q).await?;
            }
        }
        Ok(())
    }
}

/// Statistics about healing events.
#[derive(Debug, Clone, Default)]
pub struct HealingStats {
    pub total: u64,
    pub verified: u64,
    pub unverified: u64,
}

fn row_to_healing_event(row: Row) -> Result<HealingEvent> {
    let node: neo4rs::Node = row.get("h").map_err(|e| {
        RepositoryError::InvalidData(format!("Failed to get healing event node: {}", e))
    })?;

    let id: String = node
        .get("id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get id: {}", e)))?;
    let endpoint_id: String = node
        .get("endpoint_id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get endpoint_id: {}", e)))?;
    let timestamp: String = node
        .get("timestamp")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get timestamp: {}", e)))?;
    let action: String = node
        .get("action")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get action: {}", e)))?;
    let trigger_error: String = node
        .get("trigger_error")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get trigger_error: {}", e)))?;
    let ai_reasoning: String = node
        .get("ai_reasoning")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get ai_reasoning: {}", e)))?;
    let verified: bool = node
        .get("verified")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get verified: {}", e)))?;

    let action: HealingAction = serde_json::from_str(&action)?;
    let timestamp: DateTime<Utc> = timestamp
        .parse()
        .map_err(|e| RepositoryError::InvalidData(format!("Invalid timestamp: {}", e)))?;

    Ok(HealingEvent {
        id: Uuid::parse_str(&id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
        endpoint_id: Uuid::parse_str(&endpoint_id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid endpoint UUID: {}", e)))?,
        timestamp,
        action,
        trigger_error,
        ai_reasoning,
        verified,
    })
}
