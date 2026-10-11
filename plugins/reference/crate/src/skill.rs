//! `link_entities`: Wikidata/Wikipedia reference links for entities, as
//! proposals (HBI plan Phase 18, `wiki.rs`). Run hourly by
//! `workflows/entity-linking.yaml`, whose step pins the cloud model, and once
//! per entity when the user presses "Look up" in its drawer.
//!
//! `wiki_background`: Wikipedia as the first step of researching a question
//! (Phase 21, `background.rs`), used by the knowledge-gap chain.

use std::sync::Arc;

use agent_brain_plugin_api::{
    Knowledge, Llm as LlmProvider, Skill, ToolCallResult, ToolDefinition, async_trait,
};
use serde_json::{Value, json};

use crate::db::Db;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

pub struct WikiSkill {
    neo4j: Db,
    llm: Arc<dyn LlmProvider>,
    knowledge: Option<Arc<dyn Knowledge>>,
}

impl WikiSkill {
    pub fn new(
        neo4j: Db,
        llm: Arc<dyn LlmProvider>,
        knowledge: Option<Arc<dyn Knowledge>>,
    ) -> Self {
        Self {
            neo4j,
            llm,
            knowledge,
        }
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
                    "limit": { "type": "integer", "minimum": 0, "maximum": MAX_LIMIT,
                               "description": "Entities to look up this run (default 20); 0 only refreshes kinds." },
                    "entity_id": { "type": "string",
                                   "description": "Look up this one entity, even if it was looked up before." },
                    "on_demand": { "type": "boolean",
                                   "description": "The user asked: an entity named only in private notes may be looked up." }
                }
            }),
        }
    }
}

impl WikiSkill {
    fn background_def() -> ToolDefinition {
        ToolDefinition {
            name: "wiki_background".to_string(),
            description: "Read Wikipedia before searching the web: names the question's subjects \
                (people, places, organisations, events, laws, technologies), finds and picks each one's \
                English article (or none), and returns each article's first paragraphs, the section that \
                best matches the question, and the sources the article cites most, condensed by default to \
                the facts that answer the question. `text` is the \
                background, `query` a web query with the article titles added. With `session_id` the text \
                is also pushed to that working-memory session. Background only: Wikipedia lags recent \
                events, so search the web as well. Needs WIKIMEDIA_CONTACT."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question being researched." },
                    "session_id": { "type": "string",
                                    "description": "Working-memory session to push the background text to (role observation)." },
                    "condense": { "type": "boolean",
                                  "description": "Condense the articles to the facts that answer the question, each with its URL, in about 1,500 characters (default true). False returns the leads and sections as they are." }
                },
                "required": ["question"]
            }),
        }
    }

    async fn background(&self, args: Value) -> ToolCallResult {
        use crate::background as wb;
        let Some(q) = args["question"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        else {
            return ToolCallResult::error("`question` is required");
        };
        if crate::wiki::contact().is_none() {
            return ToolCallResult::error(
                "WIKIMEDIA_CONTACT is not set: Wikimedia asks automated clients for a contact",
            );
        }
        let condense = args["condense"].as_bool().unwrap_or(true);
        let (subjects, text, query) = match wb::background(self.llm.as_ref(), q, condense).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Wikipedia background failed: {e}")),
        };
        let mut banked = false;
        if let Some(sid) = args["session_id"].as_str().filter(|s| !s.trim().is_empty()) {
            let pushed = match &self.knowledge {
                Some(k) => k.push_session_entry(sid, &text, "observation").await,
                None => Err(anyhow::anyhow!("the host gave no working memory")),
            };
            match pushed {
                Ok(()) => banked = true,
                Err(e) => {
                    return ToolCallResult::error(format!(
                        "Wikipedia background found, but not pushed to {sid}: {e}"
                    ));
                }
            }
        }
        ToolCallResult::success_json(wb::result_json(&subjects, &text, &query, banked))
    }
}

#[async_trait]
impl Skill for WikiSkill {
    fn name(&self) -> &str {
        "Wiki"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def(), Self::background_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name == "wiki_background" {
            return Some(self.background(arguments.unwrap_or_default()).await);
        }
        if tool_name != "link_entities" {
            return None;
        }
        let args = arguments.unwrap_or_default();
        let limit = args["limit"]
            .as_i64()
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(0, MAX_LIMIT);
        let entity_id = args["entity_id"].as_str().filter(|s| !s.trim().is_empty());
        let on_demand = args["on_demand"].as_bool().unwrap_or(false);
        Some(
            match crate::wiki::link_entities(
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
