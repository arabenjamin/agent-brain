//! `link_entities`: Wikidata/Wikipedia reference links for entities, as
//! proposals (HBI plan Phase 18, `services/wiki.rs`). Run hourly by
//! `schedules/entity-linking.yaml`, whose step pins the cloud model, and once
//! per entity when the user presses "Look up" in its drawer.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::repository::Neo4jClient;
use crate::services::LlmProvider;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

pub struct WikiSkill {
    neo4j: Neo4jClient,
    llm: Arc<dyn LlmProvider>,
}

impl WikiSkill {
    pub fn new(neo4j: Neo4jClient, llm: Arc<dyn LlmProvider>) -> Self {
        Self { neo4j, llm }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "link_entities".to_string(),
            description: "Find which Wikidata item (and English Wikipedia article) entities are, and \
                propose it as their reference link for the user to confirm. Searches Wikidata for the \
                name, then picks the candidate the entity's claims describe, or none. Without \
                `entity_id` it takes up to `limit` entities named in claims (public sources only) that \
                have not been looked up, review queues first; a 'no article' answer is checked again \
                after 90 days. `entity_id` looks up one entity; `on_demand` (the user pressed Look up) \
                allows an entity no claim mentions. Reports what it proposed, what had no article, and \
                how many are still waiting. Needs WIKIMEDIA_CONTACT. Run by the entity-linking schedule."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
                               "description": "Entities to look up this run (default 20)." },
                    "entity_id": { "type": "string",
                                   "description": "Look up this one entity, even if it was looked up before." },
                    "on_demand": { "type": "boolean",
                                   "description": "The user asked: an entity named only in private notes may be looked up." }
                }
            }),
        }
    }
}

#[async_trait]
impl Skill for WikiSkill {
    fn name(&self) -> &str {
        "Wiki"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "link_entities" {
            return None;
        }
        let args = arguments.unwrap_or_default();
        let limit = args["limit"]
            .as_i64()
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        let entity_id = args["entity_id"].as_str().filter(|s| !s.trim().is_empty());
        let on_demand = args["on_demand"].as_bool().unwrap_or(false);
        Some(
            match crate::services::wiki::link_entities(
                &self.neo4j,
                self.llm.as_ref(),
                limit,
                entity_id,
                on_demand,
            )
            .await
            {
                Ok(v) => ToolCallResult::success_json(v),
                Err(e) => ToolCallResult::error(format!("Reference lookup failed: {e}")),
            },
        )
    }
}
