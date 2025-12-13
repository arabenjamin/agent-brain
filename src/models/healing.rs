use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Defines exactly what the AI changed in the graph.
/// Uses a tagged enum for precise serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action_type", content = "details")]
pub enum HealingAction {
    /// The API doc had the wrong parameter name (e.g., 'id' -> 'user_id')
    RenameParameter {
        old_name: String,
        new_name: String,
        param_id: Uuid,
    },
    /// The API doc had the wrong data type (e.g., String -> Integer)
    ChangeParameterType {
        param_name: String,
        old_type: String,
        new_type: String,
    },
    /// The endpoint required a parameter that wasn't in the docs
    AddMissingParameter {
        param_name: String,
        required: bool,
        detected_in_error_msg: String,
    },
    /// The endpoint path itself was wrong (e.g., /v1/user -> /v2/user)
    UpdateEndpointPath { old_path: String, new_path: String },
    /// The expected response schema didn't match reality
    UpdateResponseSchema {
        status_code: u16,
        diff_summary: String,
    },
}

/// The immutable record of a healing event.
/// Maps to a Neo4j Node: (:HealingEvent)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealingEvent {
    pub id: Uuid,
    pub endpoint_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub action: HealingAction,
    /// The raw error message from the API that triggered this fix
    pub trigger_error: String,
    /// The LLM's reasoning for why this fix is correct
    pub ai_reasoning: String,
    /// Was this change verified by a successful 200 OK retry?
    pub verified: bool,
}

impl HealingEvent {
    pub fn new(
        endpoint_id: Uuid,
        action: HealingAction,
        trigger_error: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            endpoint_id,
            timestamp: Utc::now(),
            action,
            trigger_error: trigger_error.into(),
            ai_reasoning: reasoning.into(),
            verified: true,
        }
    }

    pub fn unverified(
        endpoint_id: Uuid,
        action: HealingAction,
        trigger_error: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            verified: false,
            ..Self::new(endpoint_id, action, trigger_error, reasoning)
        }
    }
}
