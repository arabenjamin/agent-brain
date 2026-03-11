//! Working Memory Skill - Session-scoped scratchpad for agent context.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;
use uuid::Uuid;
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::repository::Neo4jClient;
use crate::services::{KnowledgeService, LlmClient, LlmConfig};
use crate::skills::Skill;

/// Working Memory Skill — push/retrieve session context and summarise into long-term memory.
pub struct WorkingMemorySkill {
    neo4j: Neo4jClient,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
}

impl WorkingMemorySkill {
    pub fn new(neo4j: Neo4jClient, llm_config: Arc<RwLock<Option<LlmConfig>>>) -> Self {
        Self { neo4j, llm_config }
    }

    async fn make_llm(&self) -> Option<LlmClient> {
        let config = self.llm_config.read().await.clone();
        config.and_then(|c| LlmClient::with_config(c).ok())
    }

    async fn make_knowledge_service(&self) -> KnowledgeService {
        let config = self.llm_config.read().await.clone();
        let llm = config.and_then(|c| LlmClient::with_config(c).ok());
        KnowledgeService::new(self.neo4j.clone(), llm)
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn push_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "push_context".to_string(),
            description: "Append an entry to the session working-memory scratchpad. \
                         Entries are ordered by turn_index within a session."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session identifier"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to store"
                    },
                    "role": {
                        "type": "string",
                        "description": "Entry role: observation (default), plan, result, error"
                    }
                },
                "required": ["session_id", "content"]
            }),
        }
    }

    fn get_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_context".to_string(),
            description: "Retrieve all working-memory entries for a session, ordered by turn."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session identifier"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max entries to return (default: 20)"
                    }
                },
                "required": ["session_id"]
            }),
        }
    }

    fn list_sessions_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_sessions".to_string(),
            description: "List all working-memory sessions, ordered by most recent first. \
                         Returns session IDs, message counts, start time, and the title \
                         (first user message in the session)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum sessions to return (default: 50)"
                    }
                }
            }),
        }
    }

    fn summarise_session_def() -> ToolDefinition {
        ToolDefinition {
            name: "summarise_session".to_string(),
            description: "Use the LLM to summarise all working-memory entries for a session into \
                         a consolidated Note in long-term memory. Optionally deletes the raw \
                         working-memory entries after summarising."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session identifier"
                    },
                    "delete_after_summarise": {
                        "type": "boolean",
                        "description": "If true, delete all WorkingMemory entries for the session \
                                        after storing the consolidated note (default: false)"
                    }
                },
                "required": ["session_id"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_push_context(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: PushContextInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let entry_id = Uuid::new_v4().to_string();
        let ts = Utc::now().to_rfc3339();
        let role = input.role.as_deref().unwrap_or("observation");

        info!(session_id = %input.session_id, role = %role, "Pushing working-memory entry");

        // Compute next turn_index atomically via Cypher
        let cypher = r#"
        OPTIONAL MATCH (w:WorkingMemory {session_id: $session_id})
        WITH COALESCE(max(w.turn_index), -1) + 1 AS next_turn
        CREATE (wm:WorkingMemory {
            id: $id,
            session_id: $session_id,
            content: $content,
            role: $role,
            turn_index: next_turn,
            created_at: datetime($ts)
        })
        RETURN wm.turn_index AS turn_index
        "#;

        let q = neo4rs::query(cypher)
            .param("id", entry_id.clone())
            .param("session_id", input.session_id.clone())
            .param("content", input.content)
            .param("role", role)
            .param("ts", ts);

        match self.neo4j.execute(q).await {
            Ok(rows) => {
                let turn_index = rows.first()
                    .and_then(|r| r.get::<i64>("turn_index").ok())
                    .unwrap_or(0);
                let response = json!({
                    "id": entry_id,
                    "turn_index": turn_index,
                    "session_id": input.session_id
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to push context: {}", e)),
        }
    }

    async fn handle_get_context(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: GetContextInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(session_id = %input.session_id, "Retrieving working-memory context");

        let cypher = r#"
        MATCH (w:WorkingMemory {session_id: $session_id})
        RETURN w.turn_index AS turn, w.role AS role, w.content AS content
        ORDER BY w.turn_index ASC LIMIT $limit
        "#;

        let q = neo4rs::query(cypher)
            .param("session_id", input.session_id.clone())
            .param("limit", input.limit as i64);

        match self.neo4j.execute(q).await {
            Ok(rows) => {
                let entries: Vec<Value> = rows.iter().map(|row| {
                    json!({
                        "turn": row.get::<i64>("turn").unwrap_or(0),
                        "role": row.get::<String>("role").unwrap_or_default(),
                        "content": row.get::<String>("content").unwrap_or_default()
                    })
                }).collect();

                let response = json!({
                    "session_id": input.session_id,
                    "count": entries.len(),
                    "entries": entries
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to retrieve context: {}", e)),
        }
    }

    async fn handle_list_sessions(&self, arguments: Option<Value>) -> ToolCallResult {
        let limit = arguments
            .as_ref()
            .and_then(|v| v.get("limit"))
            .and_then(|v| v.as_i64())
            .unwrap_or(50);

        info!("Listing working-memory sessions");

        let cypher = r#"
        MATCH (w:WorkingMemory)
        WITH w.session_id AS sid,
             toString(min(w.created_at)) AS started_at,
             count(w) AS msg_count
        OPTIONAL MATCH (first:WorkingMemory {session_id: sid, turn_index: 0})
        RETURN sid AS session_id, started_at, msg_count,
               COALESCE(first.content, sid) AS title
        ORDER BY started_at DESC
        LIMIT $limit
        "#;

        let q = neo4rs::query(cypher).param("limit", limit);

        match self.neo4j.execute(q).await {
            Ok(rows) => {
                let sessions: Vec<Value> = rows.iter().map(|row| {
                    json!({
                        "session_id": row.get::<String>("session_id").unwrap_or_default(),
                        "started_at": row.get::<String>("started_at").unwrap_or_default(),
                        "msg_count":  row.get::<i64>("msg_count").unwrap_or(0),
                        "title":      row.get::<String>("title").unwrap_or_default()
                    })
                }).collect();

                let response = json!({
                    "count": sessions.len(),
                    "sessions": sessions
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to list sessions: {}", e)),
        }
    }

    async fn handle_summarise_session(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SummariseSessionInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let llm = match self.make_llm().await {
            Some(l) => l,
            None => return ToolCallResult::error("LLM not configured for session summarisation".to_string()),
        };

        info!(session_id = %input.session_id, "Summarising session into long-term memory");

        // 1. Fetch all entries
        let cypher = r#"
        MATCH (w:WorkingMemory {session_id: $session_id})
        RETURN w.turn_index AS turn, w.role AS role, w.content AS content
        ORDER BY w.turn_index ASC
        "#;

        let rows = match self.neo4j.execute(
            neo4rs::query(cypher).param("session_id", input.session_id.clone()),
        ).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Failed to fetch session entries: {}", e)),
        };

        if rows.is_empty() {
            return ToolCallResult::error(format!("No entries found for session '{}'", input.session_id));
        }

        let entries_text: String = rows.iter().map(|row| {
            let turn = row.get::<i64>("turn").unwrap_or(0);
            let role = row.get::<String>("role").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            format!("[Turn {turn} | {role}] {content}")
        }).collect::<Vec<_>>().join("\n");

        let entries_summarised = rows.len();

        // 2. LLM summarise
        let prompt = format!(
            "Summarise this agent session into a compact memory note:\n{}",
            entries_text
        );

        let summary = match llm.generate(&prompt).await {
            Ok(r) => r.text,
            Err(e) => return ToolCallResult::error(format!("LLM summarisation failed: {}", e)),
        };

        // 3. Store in long-term memory as a consolidated note
        let knowledge = self.make_knowledge_service().await;
        let note_id = match knowledge.store_note(
            &summary,
            Some("consolidated"),
            Some(&input.session_id),
            None,
        ).await {
            Ok((id, _)) => id,
            Err(e) => return ToolCallResult::error(format!("Failed to store summary note: {}", e)),
        };

        // 4. Optionally delete session entries
        let deleted = if input.delete_after_summarise {
            let delete_q = neo4rs::query(
                "MATCH (w:WorkingMemory {session_id: $session_id}) DETACH DELETE w",
            )
            .param("session_id", input.session_id.clone());
            let _ = self.neo4j.run(delete_q).await;
            true
        } else {
            false
        };

        let response = json!({
            "id": note_id,
            "session_id": input.session_id,
            "entries_summarised": entries_summarised,
            "deleted": deleted
        });
        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }
}

#[async_trait]
impl Skill for WorkingMemorySkill {
    fn name(&self) -> &str {
        "Working Memory"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::push_context_def(),
            Self::get_context_def(),
            Self::summarise_session_def(),
            Self::list_sessions_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "push_context"      => Some(self.handle_push_context(arguments).await),
            "get_context"       => Some(self.handle_get_context(arguments).await),
            "summarise_session" => Some(self.handle_summarise_session(arguments).await),
            "list_sessions"     => Some(self.handle_list_sessions(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct PushContextInput {
    session_id: String,
    content: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetContextInput {
    session_id: String,
    #[serde(default = "default_get_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct SummariseSessionInput {
    session_id: String,
    #[serde(default)]
    delete_after_summarise: bool,
}

fn default_get_limit() -> usize { 20 }

fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Option<Value>,
) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
