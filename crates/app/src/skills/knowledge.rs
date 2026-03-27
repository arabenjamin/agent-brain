//! Knowledge Skill - Provides tools for managing notes and memories.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::repository::Neo4jClient;
use crate::services::{KnowledgeService, LlmClient, LlmConfig};
use crate::skills::Skill;

/// Knowledge Skill implementation.
pub struct KnowledgeSkill {
    neo4j: Neo4jClient,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
}

impl KnowledgeSkill {
    /// Create a new knowledge skill.
    pub fn new(neo4j: Neo4jClient, llm_config: Arc<RwLock<Option<LlmConfig>>>) -> Self {
        Self { neo4j, llm_config }
    }

    async fn make_service(&self) -> KnowledgeService {
        let config = self.llm_config.read().await.clone();
        let llm = config.and_then(|c| LlmClient::with_config(c).ok());
        KnowledgeService::new(self.neo4j.clone(), llm)
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
                        "description": "Type of note: semantic (default), episodic, reflection, consolidated"
                    },
                    "source_context": {
                        "type": "string",
                        "description": "Optional source context (e.g. session ID, document URL)"
                    },
                    "event_at": {
                        "type": "string",
                        "description": "Optional ISO-8601 timestamp of the event this note describes"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn search_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_notes".to_string(),
            description: "Search stored notes using hybrid BM25 + semantic search with optional \
                         multi-hop graph expansion. Falls back to keyword matching if no index available. \
                         Enable entity_expansion to also surface notes that share named entities with the \
                         primary results."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of results (default: 5)"
                    },
                    "graph_hops": {
                        "type": "integer",
                        "description": "Number of RELATES_TO hops to expand results (default: 2, 0 to disable)"
                    },
                    "entity_expansion": {
                        "type": "boolean",
                        "description": "Also surface notes that share named entities with the primary results (default: false)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn export_graph_visualization_def() -> ToolDefinition {
        ToolDefinition {
            name: "export_graph_visualization".to_string(),
            description: "Export the knowledge graph as a node/edge structure for visualisation. \
                         Returns Note, Entity, and Task nodes with RELATES_TO, MENTIONS, PART_OF, \
                         SUMMARIZED_BY, REFLECTS_ON, SUBTASK_OF, and DERIVED_FROM edges."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "max_nodes": {
                        "type": "integer",
                        "description": "Maximum total nodes to include (default: 200). Notes are \
                                       prioritised by recency; entities by mention count; tasks by recency."
                    }
                }
            }),
        }
    }

    fn find_related_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "find_related_notes".to_string(),
            description: "Find notes that are semantically related to a given note via \
                         graph edges (RELATES_TO). Returns related notes ordered by similarity."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note_id": {
                        "type": "string",
                        "description": "ID of the note to find related notes for"
                    }
                },
                "required": ["note_id"]
            }),
        }
    }

    fn prune_old_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "prune_old_notes".to_string(),
            description: "Delete stale notes. Use score_threshold/lambda for adaptive decay scoring \
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

    fn review_due_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "review_due_notes".to_string(),
            description: "Return notes whose spaced-repetition review interval has elapsed. \
                         Reviewing a note (via search_notes) doubles its review interval."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max number of notes to return (default: 10)"
                    }
                }
            }),
        }
    }

    fn reason_def() -> ToolDefinition {
        ToolDefinition {
            name: "reason".to_string(),
            description: "Retrieve relevant notes and derive new inferences via LLM reasoning. \
                         Optionally stores the inference as a Note with DERIVED_FROM edges to sources."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question or topic to reason about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of knowledge notes to retrieve (default: 8)"
                    },
                    "store_inference": {
                        "type": "boolean",
                        "description": "Whether to persist the inference as a Note (default: true)"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    fn audit_action_def() -> ToolDefinition {
        ToolDefinition {
            name: "audit_action".to_string(),
            description: "Check a proposed action against the brain's stored values and principles. \
                         Retrieves ethical guidelines from the knowledge graph and evaluates alignment."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "The proposed action to evaluate"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional context about why the action is being considered"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn explain_reasoning_def() -> ToolDefinition {
        ToolDefinition {
            name: "explain_reasoning".to_string(),
            description: "Narrate a human-readable explanation of why a decision or action occurred, \
                         citing the knowledge sources that drove it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "decision": {
                        "type": "string",
                        "description": "The decision or action to explain"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task ID to include task-linked reflection notes"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of knowledge notes to retrieve (default: 10)"
                    }
                },
                "required": ["decision"]
            }),
        }
    }

    fn ask_clarification_def() -> ToolDefinition {
        ToolDefinition {
            name: "ask_clarification".to_string(),
            description: "Analyze a request for ambiguity and generate specific clarifying \
                         questions before acting. Use this when a goal is underspecified, has \
                         multiple reasonable interpretations, or when acting on wrong assumptions \
                         would waste significant effort. Returns whether clarification is needed, \
                         what questions to ask, and what assumptions would be made if proceeding \
                         without clarification."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "request": {
                        "type": "string",
                        "description": "The request or instruction to analyze for ambiguity"
                    },
                    "context": {
                        "type": "string",
                        "description": "Additional context about the current situation"
                    },
                    "available_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of tools available to fulfill the request"
                    }
                },
                "required": ["request"]
            }),
        }
    }

    fn get_note_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_note".to_string(),
            description: "Fetch a single note by its ID. Returns full content, type, timestamps, \
                         and access stats. Updates the note's access count and last_accessed_at."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The UUID of the note to retrieve"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    fn search_by_entity_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_by_entity".to_string(),
            description: "Find notes that mention a named entity (API, technology, organisation, \
                         concept, person). Entities are extracted automatically when notes are stored."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "entity_name": {
                        "type": "string",
                        "description": "Entity name to search for (case-insensitive partial match)"
                    },
                    "entity_type": {
                        "type": "string",
                        "description": "Optional entity type filter (e.g. technology, organisation)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of results (default: 5)"
                    }
                },
                "required": ["entity_name"]
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

        let service = self.make_service().await;
        match service.store_note(
            &input.content,
            input.note_type.as_deref(),
            input.source_context.as_deref(),
            input.event_at.as_deref(),
        ).await {
            Ok((id, links_created)) => {
                let response = json!({
                    "success": true,
                    "note_id": id,
                    "links_created": links_created,
                    "message": "Note stored successfully"
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to store note: {}", e)),
        }
    }

    async fn handle_search_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchNotesInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(query = %input.query, "Searching notes");

        let service = self.make_service().await;
        let results = if input.entity_expansion {
            service.search_notes_with_entity_expansion(&input.query, input.limit, input.graph_hops).await
        } else {
            service.search_notes(&input.query, input.limit, input.graph_hops).await
        };
        match results {
            Ok(results) => {
                let response = json!({
                    "count": results.len(),
                    "notes": results
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Search failed: {}", e)),
        }
    }

    async fn handle_find_related_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: FindRelatedNotesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(note_id = %input.note_id, "Finding related notes");

        let service = self.make_service().await;
        match service.find_related_notes(&input.note_id).await {
            Ok(related) => {
                let notes: Vec<Value> = related
                    .into_iter()
                    .map(|(content, score)| json!({ "content": content, "similarity": score }))
                    .collect();
                let response = json!({
                    "count": notes.len(),
                    "related_notes": notes
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to find related notes: {}", e)),
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

        let service = self.make_service().await;
        match service.prune_old_notes(
            input.days_stale,
            input.min_accesses,
            input.score_threshold,
            input.lambda,
            input.dry_run,
        ).await {
            Ok(count) => {
                let response = if input.dry_run {
                    json!({
                        "would_delete": count,
                        "dry_run": true,
                        "message": format!("Would delete {} stale note(s) (dry run)", count)
                    })
                } else {
                    json!({
                        "deleted": count,
                        "message": format!("Deleted {} stale note(s)", count)
                    })
                };
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
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

        let service = self.make_service().await;
        match service.consolidate_memories(&input.topic, input.limit).await {
            Ok((id, source_count, preview)) => {
                let response = json!({
                    "consolidated_note_id": id,
                    "source_count": source_count,
                    "preview": preview
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Consolidation failed: {}", e)),
        }
    }

    async fn handle_review_due_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReviewDueNotesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(limit = input.limit, "Fetching due notes for review");

        let service = self.make_service().await;
        match service.review_due_notes(input.limit).await {
            Ok(notes) => {
                let response = json!({
                    "count": notes.len(),
                    "notes": notes
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to fetch due notes: {}", e)),
        }
    }

    async fn handle_reason(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReasonInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(question = %input.question, limit = input.limit, "Reasoning");

        let service = self.make_service().await;
        match service.reason(&input.question, input.limit, input.store_inference).await {
            Ok((answer, inferences, confidence, gaps, note_id)) => {
                let mut response = json!({
                    "answer": answer,
                    "inferences": inferences,
                    "confidence": confidence,
                    "gaps": gaps,
                });
                if let Some(nid) = note_id {
                    response["inference_note_id"] = json!(nid);
                }
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Reasoning failed: {}", e)),
        }
    }

    async fn handle_audit_action(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: AuditActionInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(action = %input.action, "Auditing action");

        let service = self.make_service().await;
        match service.audit_action(&input.action, input.context.as_deref()).await {
            Ok((aligned, confidence, concerns, suggestions, reasoning)) => {
                let response = json!({
                    "aligned": aligned,
                    "confidence": confidence,
                    "concerns": concerns,
                    "suggestions": suggestions,
                    "reasoning": reasoning,
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Audit failed: {}", e)),
        }
    }

    async fn handle_explain_reasoning(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExplainReasoningInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(decision = %input.decision, "Explaining reasoning");

        let service = self.make_service().await;
        match service.explain_reasoning(&input.decision, input.task_id.as_deref(), input.limit).await {
            Ok((explanation, sources)) => {
                let response = json!({
                    "explanation": explanation,
                    "knowledge_sources": sources,
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Explanation failed: {}", e)),
        }
    }

    async fn handle_ask_clarification(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: AskClarificationInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(request = %input.request, "Analyzing request for clarification");

        let config = self.llm_config.read().await.clone();
        let llm = match config.and_then(|c| LlmClient::with_config(c).ok()) {
            Some(l) => l,
            None => return ToolCallResult::error("LLM not configured for clarification analysis".to_string()),
        };

        let mut prompt = format!(
            "Analyze the following request for ambiguity. Determine if clarification is needed before acting.\n\nREQUEST: {}\n",
            input.request
        );
        if let Some(ctx) = &input.context {
            prompt.push_str(&format!("\nCONTEXT: {}\n", ctx));
        }
        if let Some(tools) = &input.available_tools {
            if !tools.is_empty() {
                prompt.push_str(&format!("\nAVAILABLE TOOLS: {}\n", tools.join(", ")));
            }
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
}"#
        );

        match llm.generate(&prompt).await {
            Ok(resp) => {
                let text = resp.text.trim();
                let json_start = text.find('{').unwrap_or(0);
                let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
                let parsed: Value = serde_json::from_str(&text[json_start..json_end])
                    .unwrap_or_else(|_| json!({
                        "needs_clarification": true,
                        "ambiguities": [],
                        "clarifying_questions": [text],
                        "assumptions": [],
                        "recommended_approach": "Seek clarification before proceeding"
                    }));
                ToolCallResult::success_text(serde_json::to_string_pretty(&parsed).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Clarification analysis failed: {}", e)),
        }
    }

    async fn handle_export_graph_visualization(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            #[serde(default = "default_max_nodes")]
            max_nodes: usize,
        }
        fn default_max_nodes() -> usize { 200 }

        let input: Input = arguments
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        let service = self.make_service().await;
        match service.export_graph_visualization(input.max_nodes).await {
            Ok((nodes, edges)) => {
                let response = json!({
                    "node_count": nodes.len(),
                    "edge_count": edges.len(),
                    "nodes": nodes,
                    "edges": edges
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Graph export failed: {}", e)),
        }
    }

    async fn handle_get_note(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input { id: String }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let service = self.make_service().await;
        match service.get_note(&input.id).await {
            Ok(Some(note)) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&note).unwrap()
            ),
            Ok(None) => ToolCallResult::error(format!("Note '{}' not found", input.id)),
            Err(e) => ToolCallResult::error(format!("Failed to get note: {}", e)),
        }
    }

    async fn handle_search_by_entity(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchByEntityInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(entity = %input.entity_name, "Searching by entity");

        let service = self.make_service().await;
        match service.search_by_entity(
            &input.entity_name,
            input.entity_type.as_deref(),
            input.limit,
        ).await {
            Ok(notes) => {
                let response = json!({
                    "entity_name": input.entity_name,
                    "count": notes.len(),
                    "notes": notes
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Entity search failed: {}", e)),
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
            Self::get_note_def(),
            Self::find_related_notes_def(),
            Self::prune_old_notes_def(),
            Self::consolidate_memories_def(),
            Self::review_due_notes_def(),
            Self::search_by_entity_def(),
            Self::reason_def(),
            Self::audit_action_def(),
            Self::explain_reasoning_def(),
            Self::ask_clarification_def(),
            Self::get_note_def(),
            Self::export_graph_visualization_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "store_note" => Some(self.handle_store_note(arguments).await),
            "search_notes" => Some(self.handle_search_notes(arguments).await),
            "get_note" => Some(self.handle_get_note(arguments).await),
            "find_related_notes" => Some(self.handle_find_related_notes(arguments).await),
            "prune_old_notes" => Some(self.handle_prune_old_notes(arguments).await),
            "consolidate_memories" => Some(self.handle_consolidate_memories(arguments).await),
            "review_due_notes" => Some(self.handle_review_due_notes(arguments).await),
            "search_by_entity" => Some(self.handle_search_by_entity(arguments).await),
            "reason" => Some(self.handle_reason(arguments).await),
            "audit_action" => Some(self.handle_audit_action(arguments).await),
            "explain_reasoning" => Some(self.handle_explain_reasoning(arguments).await),
            "ask_clarification" => Some(self.handle_ask_clarification(arguments).await),
            "get_note" => Some(self.handle_get_note(arguments).await),
            "export_graph_visualization" => Some(self.handle_export_graph_visualization(arguments).await),
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
}

#[derive(Debug, Deserialize)]
struct SearchNotesInput {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_graph_hops")]
    graph_hops: usize,
    #[serde(default)]
    entity_expansion: bool,
}

#[derive(Debug, Deserialize)]
struct FindRelatedNotesInput {
    note_id: String,
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
struct ReviewDueNotesInput {
    #[serde(default = "default_review_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct SearchByEntityInput {
    entity_name: String,
    #[serde(default)]
    entity_type: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct ReasonInput {
    question: String,
    #[serde(default = "default_reason_limit")]
    limit: usize,
    #[serde(default = "default_store_inference")]
    store_inference: bool,
}

#[derive(Debug, Deserialize)]
struct AuditActionInput {
    action: String,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExplainReasoningInput {
    decision: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default = "default_explain_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct AskClarificationInput {
    request: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    available_tools: Option<Vec<String>>,
}

fn default_limit() -> usize { 5 }
fn default_graph_hops() -> usize { 2 }
fn default_days_stale() -> i64 { 30 }
fn default_min_accesses() -> i64 { 2 }
fn default_consolidate_limit() -> usize { 10 }
fn default_review_limit() -> usize { 10 }
fn default_reason_limit() -> usize { 8 }
fn default_store_inference() -> bool { true }
fn default_explain_limit() -> usize { 10 }

fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Option<Value>,
) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
