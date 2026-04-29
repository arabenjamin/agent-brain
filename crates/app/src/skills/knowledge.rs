//! Knowledge Skill - Provides tools for managing notes and memories.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;

use crate::services::traits::{KnowledgeStore, LlmProvider};
use crate::skills::Skill;
use agent_brain_models::ProvenanceFlag;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Knowledge Skill implementation.
pub struct KnowledgeSkill {
    svc: Arc<dyn KnowledgeStore>,
    llm: Arc<dyn LlmProvider>,
}

impl KnowledgeSkill {
    /// Create a new knowledge skill.
    pub fn new(svc: Arc<dyn KnowledgeStore>, llm: Arc<dyn LlmProvider>) -> Self {
        Self { svc, llm }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn store_note_def() -> ToolDefinition {
        ToolDefinition {
            name: "store_note".to_string(),
            description: "Store a text note or memory in the knowledge graph. \
                         The note will be embedded for semantic search, automatically \
                         linked to similar notes, and entities extracted when LLM is available. \
                         Long notes (>1500 chars) are semantically chunked into sub-notes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Content of the note"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for categorization (stored but not indexed)"
                    },
                    "note_type": {
                        "type": "string",
                        "description": "Type of note: semantic (default), episodic, reflection, consolidated, news"
                    },
                    "source_context": {
                        "type": "string",
                        "description": "Optional source context (e.g. session ID, document URL)"
                    },
                    "event_at": {
                        "type": "string",
                        "description": "Optional ISO-8601 timestamp of the event this note describes"
                    },
                    "provenance": {
                        "type": "string",
                        "enum": ["user_input", "synthesis_inference", "core_training"],
                        "description": "Source authority flag: who/what produced this note (default: user_input)"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn search_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_notes".to_string(),
            description: "Search stored notes. Provide `query` for hybrid BM25+semantic search with \
                         optional graph expansion. Provide `entity_name` instead to find notes that \
                         mention a specific named entity (case-insensitive). At least one of `query` \
                         or `entity_name` must be supplied."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text search query (BM25 + semantic)"
                    },
                    "entity_name": {
                        "type": "string",
                        "description": "Find notes that mention this named entity (case-insensitive)"
                    },
                    "entity_type": {
                        "type": "string",
                        "description": "Optional entity type filter when using entity_name (e.g. technology, organisation)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of results (default: 5)"
                    },
                    "graph_hops": {
                        "type": "integer",
                        "description": "RELATES_TO hops to expand text-search results (default: 2, 0 to disable)"
                    },
                    "entity_expansion": {
                        "type": "boolean",
                        "description": "Also surface notes that share named entities with text-search results (default: false)"
                    }
                }
            }),
        }
    }

    // export_graph_visualization is served by GET /api/graph (REST API)

    fn prune_old_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "prune_old_notes".to_string(),
            description:
                "Delete stale notes. Use score_threshold/lambda for adaptive decay scoring \
                         (recommended), or days_stale/min_accesses for simple time-based pruning. \
                         Protected types (consolidated, reflection) are never deleted."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "days_stale": {
                        "type": "integer",
                        "description": "Legacy: delete notes not accessed in this many days (default: 30)"
                    },
                    "min_accesses": {
                        "type": "integer",
                        "description": "Legacy: delete notes accessed fewer than this many times (default: 2)"
                    },
                    "score_threshold": {
                        "type": "number",
                        "description": "Adaptive decay: delete notes with decay score below this value (default: 0.1)"
                    },
                    "lambda": {
                        "type": "number",
                        "description": "Adaptive decay: exponential decay rate (default: 0.1)"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, return count of stale notes without deleting (default: false)"
                    }
                }
            }),
        }
    }

    fn consolidate_memories_def() -> ToolDefinition {
        ToolDefinition {
            name: "consolidate_memories".to_string(),
            description: "Use the LLM to synthesize multiple notes on a topic into a single \
                         consolidated summary note. Creates SUMMARIZED_BY edges from source \
                         notes to the consolidated note. Requires LLM."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "Topic to consolidate memories about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of source notes to include (default: 10)"
                    }
                },
                "required": ["topic"]
            }),
        }
    }

    fn reason_def() -> ToolDefinition {
        ToolDefinition {
            name: "reason".to_string(),
            description: "Multi-mode LLM reasoning tool. \
                         action=\"infer\" (default): derive new inferences from stored knowledge; \
                         action=\"explain\": narrate why a decision occurred, citing sources; \
                         action=\"clarify\": analyse a request for ambiguity and generate questions; \
                         action=\"audit\": check a proposed action against stored values and principles; \
                         action=\"structured\": full structured output with sources, caveats, follow-up questions, \
                         gaps, inferences, and optional adversarial critic pass."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Mode: infer (default), explain, clarify, audit, or structured"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question/decision/request/action to reason about (required)"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional raw context text. When provided in infer mode, bypasses RAG and reasons directly over this text — use for live data like search results passed via {{_prev}}. Also used by clarify and audit."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max knowledge notes to retrieve (default: 8 for infer/explain)"
                    },
                    "store_inference": {
                        "type": "boolean",
                        "description": "Persist the result as a Note (default: true, infer mode only)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Include task-linked reflection notes (explain mode)"
                    },
                    "available_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tools available to the agent (clarify mode)"
                    },
                    "run_critic": {
                        "type": "boolean",
                        "description": "Run adversarial critic pass to stress-test the answer and adjust confidence (structured mode only, default: false)"
                    },
                    "create_gap_tasks": {
                        "type": "boolean",
                        "description": "Create Task nodes for each identified knowledge gap (structured mode only, requires store_inference=true, default: false)"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_store_note(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: StoreNoteInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(content_len = input.content.len(), "Storing note");

        let provenance = input
            .provenance
            .as_deref()
            .and_then(|s| s.parse::<ProvenanceFlag>().ok())
            .unwrap_or(ProvenanceFlag::UserInput);

        match self
            .svc
            .store_note(
                &input.content,
                input.note_type.as_deref(),
                input.source_context.as_deref(),
                input.event_at.as_deref(),
                Some(provenance),
            )
            .await
        {
            Ok((id, links_created)) => {
                let response = json!({
                    "success": true,
                    "id": id,
                    "links_created": links_created,
                    "message": "Note stored successfully"
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to store note: {}", e)),
        }
    }

    async fn handle_search_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchNotesInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        // entity_name path — direct named-entity lookup
        if let Some(ref entity_name) = input.entity_name {
            info!(entity = %entity_name, "Searching notes by entity");
            return match self
                .svc
                .search_by_entity(entity_name, input.entity_type.as_deref(), input.limit)
                .await
            {
                Ok(notes) => ToolCallResult::success_json(json!({
                    "entity_name": entity_name,
                    "count": notes.len(),
                    "notes": notes
                })),
                Err(e) => ToolCallResult::error(format!("Entity search failed: {}", e)),
            };
        }

        let query = match input.query.as_deref() {
            Some(q) if !q.is_empty() => q,
            _ => return ToolCallResult::error("Provide `query` or `entity_name`".to_string()),
        };

        info!(query = %query, "Searching notes");

        let results = if input.entity_expansion {
            self.svc
                .search_notes_with_entity_expansion(query, input.limit, input.graph_hops)
                .await
        } else {
            self.svc
                .search_notes(query, input.limit, input.graph_hops)
                .await
        };
        match results {
            Ok(results) => ToolCallResult::success_json(json!({
                "count": results.len(),
                "notes": results
            })),
            Err(e) => ToolCallResult::error(format!("Search failed: {}", e)),
        }
    }

    async fn handle_prune_old_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: PruneOldNotesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(
            days_stale = input.days_stale,
            min_accesses = input.min_accesses,
            dry_run = input.dry_run,
            "Pruning stale notes"
        );

        match self
            .svc
            .prune_old_notes(
                input.days_stale,
                input.min_accesses,
                input.score_threshold,
                input.lambda,
                input.dry_run,
            )
            .await
        {
            Ok(count) => {
                let response = if input.dry_run {
                    json!({
                        "count": count,
                        "dry_run": true,
                        "message": format!("Would delete {} stale note(s) (dry run)", count)
                    })
                } else {
                    json!({
                        "count": count,
                        "message": format!("Deleted {} stale note(s)", count)
                    })
                };
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to prune notes: {}", e)),
        }
    }

    async fn handle_consolidate_memories(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ConsolidateMemoriesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Consolidating memories");

        match self
            .svc
            .consolidate_memories(&input.topic, input.limit)
            .await
        {
            Ok((id, source_count, preview)) => {
                let response = json!({
                    "id": id,
                    "source_count": source_count,
                    "preview": preview
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Consolidation failed: {}", e)),
        }
    }

    async fn handle_reason(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReasonInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match input.action.as_deref().unwrap_or("infer") {
            "explain" => {
                info!(decision = %input.question, "Explaining reasoning");
                match self
                    .svc
                    .explain_reasoning(&input.question, input.task_id.as_deref(), input.limit)
                    .await
                {
                    Ok((explanation, sources)) => ToolCallResult::success_json(json!({
                        "explanation": explanation,
                        "knowledge_sources": sources,
                    })),
                    Err(e) => ToolCallResult::error(format!("Explanation failed: {}", e)),
                }
            }
            "clarify" => {
                info!(request = %input.question, "Analyzing request for clarification");
                let mut prompt = format!(
                    "Analyze the following request for ambiguity. Determine if clarification is needed before acting.\n\nREQUEST: {}\n",
                    input.question
                );
                if let Some(ctx) = &input.context {
                    prompt.push_str(&format!("\nCONTEXT: {}\n", ctx));
                }
                if let Some(tools) = &input.available_tools
                    && !tools.is_empty()
                {
                    prompt.push_str(&format!("\nAVAILABLE TOOLS: {}\n", tools.join(", ")));
                }
                prompt.push_str(
                    r#"
Respond with a JSON object only (no markdown, no explanation):
{
  "needs_clarification": true,
  "ambiguities": ["specific ambiguous aspect 1", "..."],
  "clarifying_questions": ["question to ask 1", "..."],
  "assumptions": ["assumption that would be made if proceeding 1", "..."],
  "recommended_approach": "brief description of how to proceed"
}"#,
                );
                match self.llm.generate(&prompt, None).await {
                    Ok(text_resp) => {
                        let text = text_resp.trim();
                        let json_start = text.find('{').unwrap_or(0);
                        let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
                        let parsed: Value = serde_json::from_str(&text[json_start..json_end])
                            .unwrap_or_else(|_| {
                                json!({
                                    "needs_clarification": true,
                                    "ambiguities": [],
                                    "clarifying_questions": [text],
                                    "assumptions": [],
                                    "recommended_approach": "Seek clarification before proceeding"
                                })
                            });
                        ToolCallResult::success_json(parsed)
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Clarification analysis failed: {}", e))
                    }
                }
            }
            "audit" => {
                info!(action = %input.question, "Auditing action");
                match self
                    .svc
                    .audit_action(&input.question, input.context.as_deref())
                    .await
                {
                    Ok((aligned, confidence, concerns, suggestions, reasoning)) => {
                        ToolCallResult::success_json(json!({
                            "aligned": aligned,
                            "confidence": confidence,
                            "concerns": concerns,
                            "suggestions": suggestions,
                            "reasoning": reasoning,
                        }))
                    }
                    Err(e) => ToolCallResult::error(format!("Audit failed: {}", e)),
                }
            }
            "structured" => {
                info!(question = %input.question, run_critic = input.run_critic, "Structured reasoning");
                match self
                    .svc
                    .reason_structured(
                        &input.question,
                        input.limit,
                        input.store_inference,
                        input.run_critic,
                    )
                    .await
                {
                    Ok(output) => {
                        let sources_json: Vec<Value> = output
                            .sources
                            .iter()
                            .map(|s| json!({ "note_id": s.note_id, "preview": s.preview }))
                            .collect();

                        let mut response = json!({
                            "answer": output.answer,
                            "sources": sources_json,
                            "confidence": output.confidence,
                            "caveats": output.caveats,
                            "follow_up_questions": output.follow_up_questions,
                            "inferences": output.inferences,
                            "gaps": output.gaps,
                            "critic_counter_arguments": output.critic_counter_arguments,
                        });

                        if let Some(nid) = &output.inference_note_id {
                            response["inference_note_id"] = json!(nid);
                        }

                        if input.create_gap_tasks {
                            if let Some(ref note_id) = output.inference_note_id {
                                if !output.gaps.is_empty() {
                                    match self.svc.create_gap_tasks(&output.gaps, note_id).await {
                                        Ok(ids) => {
                                            response["gap_task_ids"] = json!(ids);
                                        }
                                        Err(e) => {
                                            response["gap_task_error"] = json!(e.to_string());
                                        }
                                    }
                                }
                            }
                        }

                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Structured reasoning failed: {}", e)),
                }
            }
            _ => {
                // "infer" (default)
                info!(question = %input.question, limit = input.limit, "Reasoning");

                // When external context is provided, bypass RAG and reason directly over it.
                // This prevents contamination from stale notes when the caller has live data
                // (e.g. freshly-fetched search results in a job chain).
                if let Some(ctx) = &input.context {
                    let prompt = format!(
                        "You are a reasoning engine. Using the provided context, answer the question \
                         clearly. Distinguish what is directly stated vs inferred.\n\
                         Output ONLY valid JSON. The \"answer\" field may contain markdown formatting \
                         (headers, bullets, bold) when the question requests a structured report:\n\
                         {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\"gaps\":[\"...\"]}}\n\n\
                         QUESTION: {}\n\
                         CONTEXT:\n{}",
                        input.question, ctx
                    );
                    return match self.llm.generate(&prompt, None).await {
                        Ok(text_resp) => {
                            let text = text_resp.trim();
                            let json_start = text.find('{').unwrap_or(0);
                            let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
                            let parsed: Value = serde_json::from_str(&text[json_start..json_end])
                                .unwrap_or_else(|_| {
                                    json!({
                                        "answer": text,
                                        "inferences": [],
                                        "confidence": 0.5,
                                        "gaps": []
                                    })
                                });
                            ToolCallResult::success_json(parsed)
                        }
                        Err(e) => ToolCallResult::error(format!("Reasoning failed: {}", e)),
                    };
                }

                match self
                    .svc
                    .reason(&input.question, input.limit, input.store_inference)
                    .await
                {
                    Ok((answer, inferences, confidence, gaps, note_id)) => {
                        let mut response = json!({
                            "answer": answer,
                            "inferences": inferences,
                            "confidence": confidence,
                            "gaps": gaps,
                        });
                        if let Some(nid) = note_id {
                            response["id"] = json!(nid);
                        }
                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Reasoning failed: {}", e)),
                }
            }
        }
    }

    fn synthesize_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "synthesize_knowledge".to_string(),
            description: "Search for recent notes and inferences related to a topic, then use \
                         the LLM to distill durable facts, patterns, and insights into a new \
                         semantic knowledge note. Use this at the end of a reasoning or research \
                         chain to persist what was actually learned."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "The topic or goal to synthesize knowledge about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of source notes to draw from (default: 10)"
                    }
                },
                "required": ["topic"]
            }),
        }
    }

    async fn handle_synthesize_knowledge(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SynthesizeKnowledgeInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Synthesizing semantic knowledge");

        match self
            .svc
            .synthesize_knowledge(&input.topic, input.limit)
            .await
        {
            Ok((note_id, preview)) => ToolCallResult::success_json(json!({
                "success": true,
                "note_id": note_id,
                "note_type": "semantic",
                "preview": preview
            })),
            Err(e) => ToolCallResult::error(format!("Knowledge synthesis failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for KnowledgeSkill {
    fn name(&self) -> &str {
        "Knowledge Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::store_note_def(),
            Self::search_notes_def(),
            Self::prune_old_notes_def(),
            Self::consolidate_memories_def(),
            Self::synthesize_knowledge_def(),
            Self::reason_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "store_note" => Some(self.handle_store_note(arguments).await),
            "search_notes" => Some(self.handle_search_notes(arguments).await),
            "prune_old_notes" => Some(self.handle_prune_old_notes(arguments).await),
            "consolidate_memories" => Some(self.handle_consolidate_memories(arguments).await),
            "synthesize_knowledge" => Some(self.handle_synthesize_knowledge(arguments).await),
            "reason" => Some(self.handle_reason(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct StoreNoteInput {
    content: String,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    #[serde(default)]
    note_type: Option<String>,
    #[serde(default)]
    source_context: Option<String>,
    #[serde(default)]
    event_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchNotesInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    entity_name: Option<String>,
    #[serde(default)]
    entity_type: Option<String>,
    #[serde(default = "agent_brain_protocol::default_limit_5")]
    limit: usize,
    #[serde(default = "agent_brain_protocol::default_graph_hops")]
    graph_hops: usize,
    #[serde(default)]
    entity_expansion: bool,
}

#[derive(Debug, Deserialize)]
struct PruneOldNotesInput {
    #[serde(default = "default_days_stale")]
    days_stale: i64,
    #[serde(default = "default_min_accesses")]
    min_accesses: i64,
    #[serde(default)]
    score_threshold: Option<f64>,
    #[serde(default)]
    lambda: Option<f64>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct ConsolidateMemoriesInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct ReasonInput {
    question: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default = "default_reason_limit")]
    limit: usize,
    #[serde(default = "default_store_inference")]
    store_inference: bool,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    available_tools: Option<Vec<String>>,
    #[serde(default)]
    run_critic: bool,
    #[serde(default)]
    create_gap_tasks: bool,
}

#[derive(Debug, Deserialize)]
struct SynthesizeKnowledgeInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

fn default_days_stale() -> i64 {
    30
}
fn default_min_accesses() -> i64 {
    2
}
fn default_consolidate_limit() -> usize {
    10
}
fn default_reason_limit() -> usize {
    8
}
fn default_store_inference() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::test_helpers::*;
    use std::sync::Arc;

    fn skill_ok() -> KnowledgeSkill {
        KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("{}")),
        )
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill_ok().tools().len(), 6);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let skill = skill_ok();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(skill.execute("nonexistent_tool", None));
        assert!(result.is_none());
    }

    // -- store_note -----------------------------------------------------------

    #[tokio::test]
    async fn store_note_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"content": "hello world"});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["id"], "note-id-1");
        assert_eq!(r["links_created"], 2);
    }

    #[tokio::test]
    async fn store_note_missing_content_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("store_note", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn store_note_propagates_store_error() {
        let mut store = MockKnowledgeStore::default();
        store.store_result = Err("db down".into());
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"content": "test"});
        let msg = result_error(skill.execute("store_note", Some(args)).await.unwrap());
        assert!(msg.contains("db down"));
    }

    // -- search_notes ---------------------------------------------------------

    #[tokio::test]
    async fn search_notes_success() {
        let mut store = MockKnowledgeStore::default();
        store.search_result = Ok(vec![serde_json::json!({"id": "n1", "content": "c1"})]);
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "something"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_with_entity_expansion() {
        let mut store = MockKnowledgeStore::default();
        store.search_result = Ok(vec![serde_json::json!({"id": "n1"})]);
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "rust", "entity_expansion": true});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_empty_query_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({"query": ""})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn search_notes_no_query_or_entity_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    // -- search_notes entity_name path ----------------------------------------

    #[tokio::test]
    async fn search_notes_by_entity_name_success() {
        let mut store = MockKnowledgeStore::default();
        store.search_result = Ok(vec![serde_json::json!({"id": "n2"})]);
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"entity_name": "Rust"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
        assert_eq!(r["entity_name"], "Rust");
    }

    // -- prune_old_notes ------------------------------------------------------

    #[tokio::test]
    async fn prune_old_notes_dry_run() {
        let skill = skill_ok();
        let args = serde_json::json!({"dry_run": true});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["dry_run"], true);
        assert_eq!(r["count"], 3);
    }

    #[tokio::test]
    async fn prune_old_notes_actual() {
        let skill = skill_ok();
        let args = serde_json::json!({"days_stale": 14});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 3);
        assert!(r.get("dry_run").is_none());
    }

    // -- consolidate_memories -------------------------------------------------

    #[tokio::test]
    async fn consolidate_memories_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "rust async"});
        let r = result_json(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["id"], "consolidated-id");
        assert_eq!(r["source_count"], 5);
    }

    #[tokio::test]
    async fn consolidate_memories_propagates_error() {
        let mut store = MockKnowledgeStore::default();
        store.consolidate_result = Err("consolidation failed".into());
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"topic": "test"});
        let msg = result_error(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert!(msg.contains("consolidation failed"));
    }

    // -- synthesize_knowledge -------------------------------------------------

    #[tokio::test]
    async fn synthesize_knowledge_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "Rust async"});
        let r = result_json(
            skill
                .execute("synthesize_knowledge", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["note_id"], "synth-id");
        assert_eq!(r["success"], true);
    }

    // -- reason ---------------------------------------------------------------

    #[tokio::test]
    async fn reason_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"question": "What is async?"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["answer"], "answer text");
        assert!((r["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-9);
    }

    // -- reason action=audit --------------------------------------------------

    #[tokio::test]
    async fn reason_audit_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "audit", "question": "deploy to prod"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["aligned"], true);
    }

    // -- reason action=explain ------------------------------------------------

    #[tokio::test]
    async fn reason_explain_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "explain", "question": "chose Rust"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["explanation"], "explanation text");
    }

    // -- reason action=clarify ------------------------------------------------

    #[tokio::test]
    async fn reason_clarify_parses_llm_json() {
        let llm_resp = r#"{"needs_clarification": true, "ambiguities": ["scope"], "clarifying_questions": ["Which env?"], "assumptions": [], "recommended_approach": "ask first"}"#;
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok(llm_resp)),
        );
        let args = serde_json::json!({"action": "clarify", "question": "deploy the thing"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["needs_clarification"], true);
    }

    #[tokio::test]
    async fn reason_clarify_llm_error_returns_error() {
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::err("llm unavailable")),
        );
        let args = serde_json::json!({"action": "clarify", "question": "do something"});
        let msg = result_error(skill.execute("reason", Some(args)).await.unwrap());
        assert!(msg.contains("llm unavailable"));
    }
}
