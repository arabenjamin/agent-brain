//! Working Memory Skill - Session-scoped scratchpad for agent context.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use crate::services::traits::{KnowledgeStore, LlmProvider, WorkingMemoryStore};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

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
        Self {
            store,
            knowledge,
            llm,
        }
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

        match self
            .store
            .push_entry(&entry_id, &input.session_id, &input.content, role, &ts)
            .await
        {
            Ok(turn_index) => {
                let response = json!({
                    "id": entry_id,
                    "turn_index": turn_index,
                    "session_id": input.session_id
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to push context: {}", e)),
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

        let entries_text: String = rows
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let role = entry
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
                format!("[Turn {i} | {role}] {content}")
            })
            .collect::<Vec<_>>()
            .join("\n");

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
        let note_id = match self
            .knowledge
            .store_note(
                &summary,
                Some("consolidated"),
                Some(&input.session_id),
                None,
            )
            .await
        {
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
        ToolCallResult::success_json(response)
    }
}

#[async_trait]
impl Skill for WorkingMemorySkill {
    fn name(&self) -> &str {
        "Working Memory"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::push_context_def(), Self::summarise_session_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "push_context" => Some(self.handle_push_context(arguments).await),
            "summarise_session" => Some(self.handle_summarise_session(arguments).await),
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
struct SummariseSessionInput {
    session_id: String,
    #[serde(default)]
    delete_after_summarise: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::test_helpers::*;
    use std::sync::Arc;

    fn skill(wm: MockWorkingMemoryStore) -> WorkingMemorySkill {
        WorkingMemorySkill::new(
            Arc::new(wm),
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("session summary text")),
        )
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill(MockWorkingMemoryStore::default()).tools().len(), 2);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let s = skill(MockWorkingMemoryStore::default());
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(s.execute("not_a_tool", None));
        assert!(r.is_none());
    }

    // -- push_context ---------------------------------------------------------

    #[tokio::test]
    async fn push_context_success() {
        let r = result_json(
            skill(MockWorkingMemoryStore::default())
                .execute(
                    "push_context",
                    Some(serde_json::json!({
                        "session_id": "sess-1",
                        "content": "did a thing"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["session_id"], "sess-1");
        assert_eq!(r["turn_index"], 0);
        assert!(r["id"].as_str().is_some());
    }

    #[tokio::test]
    async fn push_context_with_role() {
        let r = result_json(
            skill(MockWorkingMemoryStore::default())
                .execute(
                    "push_context",
                    Some(serde_json::json!({
                        "session_id": "sess-1",
                        "content": "planning",
                        "role": "plan"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["turn_index"], 0);
    }

    #[tokio::test]
    async fn push_context_missing_fields_returns_error() {
        let r = skill(MockWorkingMemoryStore::default())
            .execute("push_context", Some(serde_json::json!({"session_id": "s"})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn push_context_store_error_propagates() {
        let mut wm = MockWorkingMemoryStore::default();
        wm.push_result = Err("store down".into());
        let msg = result_error(
            skill(wm)
                .execute(
                    "push_context",
                    Some(serde_json::json!({"session_id": "s", "content": "c"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("store down"));
    }

    // -- summarise_session ----------------------------------------------------

    #[tokio::test]
    async fn summarise_session_success() {
        let r = result_json(
            skill(MockWorkingMemoryStore::default())
                .execute(
                    "summarise_session",
                    Some(serde_json::json!({"session_id": "sess-1"})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["session_id"], "sess-1");
        assert_eq!(r["entries_summarised"], 2);
        assert_eq!(r["deleted"], false);
    }

    #[tokio::test]
    async fn summarise_session_with_delete() {
        let r = result_json(
            skill(MockWorkingMemoryStore::default())
                .execute(
                    "summarise_session",
                    Some(serde_json::json!({
                        "session_id": "sess-1",
                        "delete_after_summarise": true
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["deleted"], true);
    }

    #[tokio::test]
    async fn summarise_session_empty_returns_error() {
        let mut wm = MockWorkingMemoryStore::default();
        wm.get_all_result = Ok(vec![]);
        let msg = result_error(
            skill(wm)
                .execute(
                    "summarise_session",
                    Some(serde_json::json!({"session_id": "empty"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("No entries found"));
    }

    #[tokio::test]
    async fn summarise_session_llm_error() {
        let s = WorkingMemorySkill::new(
            Arc::new(MockWorkingMemoryStore::default()),
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::err("llm down")),
        );
        let msg = result_error(
            s.execute(
                "summarise_session",
                Some(serde_json::json!({"session_id": "sess-1"})),
            )
            .await
            .unwrap(),
        );
        assert!(msg.contains("llm down"));
    }
}
