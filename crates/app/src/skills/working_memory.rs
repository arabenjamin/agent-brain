//! Working Memory Skill - Session-scoped scratchpad for agent context.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use crate::services::traits::{KnowledgeStore, LlmProvider, WorkingMemoryStore};
use crate::skills::Skill;

/// Working Memory Skill — push/retrieve session context and summarise into long-term memory.
pub struct WorkingMemorySkill {
    store: Arc<dyn WorkingMemoryStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    llm: Arc<dyn LlmProvider>,
}

impl WorkingMemorySkill {
    pub fn new(
        store: Arc<dyn WorkingMemoryStore>,
        knowledge: Arc<dyn KnowledgeStore>,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        Self { store, knowledge, llm }
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

        match self.store.push_entry(&entry_id, &input.session_id, &input.content, role, &ts).await {
            Ok(turn_index) => {
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

        match self.store.get_entries(&input.session_id, input.limit).await {
            Ok(entries) => {
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

        match self.store.list_sessions(limit).await {
            Ok(sessions) => {
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

        info!(session_id = %input.session_id, "Summarising session into long-term memory");

        // 1. Fetch all entries via the store trait
        let rows = match self.store.get_all_entries(&input.session_id).await {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(format!("Failed to fetch session entries: {}", e));
            }
        };

        if rows.is_empty() {
            return ToolCallResult::error(format!(
                "No entries found for session '{}'",
                input.session_id
            ));
        }

        let entries_text: String = rows.iter().enumerate().map(|(i, entry)| {
            let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
            format!("[Turn {i} | {role}] {content}")
        }).collect::<Vec<_>>().join("\n");

        let entries_summarised = rows.len();

        // 2. LLM summarise
        let prompt = format!(
            "Summarise this agent session into a compact memory note:\n{}",
            entries_text
        );

        let summary = match self.llm.generate(&prompt, None).await {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("LLM summarisation failed: {}", e)),
        };

        // 3. Store in long-term memory as a consolidated note
        let note_id = match self.knowledge.store_note(
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
            let _ = self.store.delete_session(&input.session_id).await;
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
            "push_context" => Some(self.handle_push_context(arguments).await),
            "get_context" => Some(self.handle_get_context(arguments).await),
            "summarise_session" => Some(self.handle_summarise_session(arguments).await),
            "list_sessions" => Some(self.handle_list_sessions(arguments).await),
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

fn default_get_limit() -> usize {
    20
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
