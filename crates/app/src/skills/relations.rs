//! `extract_relations`: typed relationships from claims, as proposals
//! (HBI plan Phase 17, `services/relations.rs`). Run by
//! `workflows/relation-extraction.yaml`, whose step pins the model: measured
//! on held-out claims, the local model got 2 of 8 relations right and the
//! cloud one 8 of 9 (5 of 9 strictly), and either way the user reviews them.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::repository::Neo4jClient;
use crate::services::LlmProvider;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

const DEFAULT_LIMIT: i64 = 30;
const MAX_LIMIT: i64 = 60;

pub struct RelationSkill {
    neo4j: Neo4jClient,
    llm: Arc<dyn LlmProvider>,
}

impl RelationSkill {
    pub fn new(neo4j: Neo4jClient, llm: Arc<dyn LlmProvider>) -> Self {
        Self { neo4j, llm }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "extract_relations".to_string(),
            description: "Read claims that name two or more entities and propose the typed \
                relationships they state (works_for, leads, member_of, part_of, located_in, owns, \
                made_by, allied_with, opposes; relations/*.yaml). Proposals wait for the user's \
                review; a relationship the user rejected is never proposed again. Reads up to \
                `limit` claims not read before, newest first, and reports what it proposed, what it \
                dropped and why, and how many claims are still waiting. `note_id` re-reads one \
                claim. Run by the relation-extraction schedule."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
                               "description": "Claims to read this run (default 30)." },
                    "note_id": { "type": "string",
                                 "description": "Re-read this one claim, even if it was read before." }
                }
            }),
        }
    }
}

#[async_trait]
impl Skill for RelationSkill {
    fn name(&self) -> &str {
        "Relations"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "extract_relations" {
            return None;
        }
        let limit = arguments
            .as_ref()
            .and_then(|a| a["limit"].as_i64())
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        Some(
            match crate::services::relations::extract_from_claims(
                &self.neo4j,
                self.llm.as_ref(),
                limit,
                arguments
                    .as_ref()
                    .and_then(|a| a["note_id"].as_str())
                    .filter(|s| !s.trim().is_empty()),
            )
            .await
            {
                Ok(v) => ToolCallResult::success_json(v),
                Err(e) => ToolCallResult::error(format!("Relation extraction failed: {e}")),
            },
        )
    }
}
