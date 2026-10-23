//! Working Memory Skill - Session-scoped scratchpad for agent context.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::info;
use uuid::Uuid;

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use agent_brain_repository::Neo4jClient;
use agent_brain_work::WorkingMemoryStore;
use agent_brain_work::events::BrainEvent;

/// Working Memory Skill — push session context; `notify_user`.
pub struct WorkingMemorySkill {
    store: Arc<dyn WorkingMemoryStore>,
    /// Optional Neo4j client for persisting `AgentNotification` nodes.
    neo4j: Option<Arc<Neo4jClient>>,
    /// Optional event bus for broadcasting `AgentChatInitiated` events.
    event_tx: Option<broadcast::Sender<BrainEvent>>,
}

impl WorkingMemorySkill {
    pub fn new(store: Arc<dyn WorkingMemoryStore>) -> Self {
        Self {
            store,
            neo4j: None,
            event_tx: None,
        }
    }

    /// Enable the `notify_user` tool by providing Neo4j + the brain event bus.
    pub fn with_notification_support(
        mut self,
        neo4j: Arc<Neo4jClient>,
        event_tx: broadcast::Sender<BrainEvent>,
    ) -> Self {
        self.neo4j = Some(neo4j);
        self.event_tx = Some(event_tx);
        self
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

    fn notify_user_def() -> ToolDefinition {
        ToolDefinition {
            name: "notify_user".to_string(),
            description: "Send a proactive message to the user — opens or continues a chat \
                         conversation so the user sees your message when they next check the UI. \
                         Use when you need human input, want to report a long-running task result, \
                         or have a follow-up question that requires user action."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message to show the user in chat. Empty (or only whitespace) sends nothing, so a workflow can notify only when an earlier step found a change"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional short context label (e.g. 'Task: deploy app', 'Follow-up on research')"
                    },
                    "related_session_id": {
                        "type": "string",
                        "description": "Optional session ID to resume an existing conversation"
                    },
                    "needs": {
                        "type": "string",
                        "enum": ["action"],
                        "description": "Set \"action\" when the user must do or decide something (an API needs a key, apply to this job?). It also creates an item for the user in their Feed, with this conversation as its thread. Leave it out for something to read (a brief, a report)."
                    },
                    "ask": {
                        "type": "string",
                        "description": "Required with needs: \"action\". What exactly the user must do or decide, and what is blocked until they do, in one or two sentences — e.g. 'Sign up at https://x.dev and add X_API_KEY to docker-compose.yml; I can ingest the API after that.' The item records which run, workflow or agent sent it by itself."
                    }
                },
                "required": ["message"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_notify_user(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: NotifyUserInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        // A workflow step that has nothing to say sends nothing: a code step
        // before it prints a message only when something changed (e.g.
        // slm-benchmark-watch notifies only when its Tier 2 list moves).
        if input.message.trim().is_empty() {
            return ToolCallResult::success_text(
                json!({"sent": false, "reason": "empty message: nothing to send"}).to_string(),
            );
        }

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("notify_user is not available: no database configured");
        };
        let mut input = input;
        input.message = without_relay_header(&input.message);

        if let Some(other) = input
            .needs
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != "action")
        {
            return ToolCallResult::error(format!(
                "needs must be \"action\" or left out, not '{other}'; nothing was sent"
            ));
        }

        let needs_action = input.needs.as_deref().map(str::trim) == Some("action");
        let ask = input
            .ask
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(String::from);
        if needs_action && ask.is_none() {
            return ToolCallResult::error(
                "needs: \"action\" requires `ask`: what exactly the user must do or decide, and \
                 what is blocked until they do. Nothing was sent.",
            );
        }

        let notification_id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();

        info!(
            notification_id = %notification_id,
            session = ?input.related_session_id,
            "Brain notifying user"
        );

        if let Err(e) = neo4j
            .create_notification(
                &notification_id,
                &input.message,
                input.context.as_deref(),
                input.related_session_id.as_deref(),
                &created_at,
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to create notification: {e}"));
        }

        // An ask, not a read: it becomes an item the user acts on, linked back
        // to this notification and threaded in its conversation.
        let mut item_id: Option<String> = None;
        if needs_action {
            let title = input
                .context
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from)
                .unwrap_or_else(|| agent_brain_scheduler::items::title_from(&input.message));
            let item = agent_brain_scheduler::items::NewItem {
                title,
                description: Some(input.message.clone()),
                patch: agent_brain_repository::TodoItemPatch {
                    assignee: Some(Some("user".into())),
                    origin_kind: Some(Some("agent".into())),
                    origin_ref: Some(Some(notification_id.clone())),
                    session_id: Some(input.related_session_id.clone()),
                    ask: Some(ask.clone()),
                    source: Some(Some(sender_label(neo4j).await)),
                    ..Default::default()
                },
                ..Default::default()
            };
            match agent_brain_scheduler::items::create_item(neo4j, item).await {
                Ok((t, _)) => item_id = Some(t.id),
                Err(e) => {
                    return ToolCallResult::error(format!(
                        "The notification was sent, but its item was not created: {e}"
                    ));
                }
            }
            let _ = neo4j
                .run(
                    neo4rs::query(
                        "MATCH (n:AgentNotification {id: $id}) SET n.needs_action = true, n.item_id = $item",
                    )
                    .param("id", notification_id.as_str())
                    .param("item", item_id.clone().unwrap_or_default()),
                )
                .await;
        }

        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(BrainEvent::AgentChatInitiated {
                notification_id: notification_id.clone(),
                message: input.message.clone(),
                related_session_id: input.related_session_id.clone(),
            });
        }

        // "answer" field makes extract_result_text return the brief text (not JSON)
        // so a downstream {{_prev}} step (e.g. store_note) receives clean content.
        ToolCallResult::success_json(json!({
            "notification_id": notification_id,
            "answer": input.message,
            "status": "delivered",
            "item_id": item_id,
        }))
    }

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
                // "answer" echoes the pushed content so downstream {{_prev}} steps
                // (e.g. store_note) receive the original text instead of metadata JSON.
                let response = json!({
                    "id": entry_id,
                    "turn_index": turn_index,
                    "session_id": input.session_id,
                    "answer": input.content
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to push context: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for WorkingMemorySkill {
    fn name(&self) -> &str {
        "Working Memory"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![Self::push_context_def()];
        if self.neo4j.is_some() {
            tools.push(Self::notify_user_def());
        }
        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "notify_user" => Some(self.handle_notify_user(arguments).await),
            "push_context" => Some(self.handle_push_context(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

/// Who is sending a notification, in words: the agent when one runs this
/// (`subagent:<name>` on the usage label), in the run its task records.
async fn sender_label(neo4j: &agent_brain_repository::Neo4jClient) -> String {
    let agent = agent_brain_chat::chat::USAGE_TOOL_LABEL
        .try_with(|l| l.strip_prefix("subagent:").map(|a| format!("agent {a}")))
        .ok()
        .flatten();
    let task = agent_brain_llm::ctx::current_owner_task();
    agent_brain_scheduler::items::source_label(neo4j, task.as_deref(), agent.as_deref()).await
}

#[derive(Debug, Deserialize)]
struct NotifyUserInput {
    message: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    related_session_id: Option<String>,
    #[serde(default)]
    needs: Option<String>,
    #[serde(default)]
    ask: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PushContextInput {
    session_id: String,
    content: String,
    #[serde(default)]
    role: Option<String>,
}

/// A message for a person, without the "[FINDINGS FROM …]" header
/// `run_agent` writes for the model that relays an agent's answer: a hand-off
/// step sends an agent's answer straight to the owner's chat. The tool
/// receipt and sources stay.
fn without_relay_header(message: &str) -> String {
    let Some(start) = message.find("[FINDINGS FROM ") else {
        return message.to_string();
    };
    let Some(len) = message[start..].find("]\n") else {
        return message.to_string();
    };
    let rest = message[start + len + 2..].trim_start_matches('\n');
    format!("{}{rest}", &message[..start])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use std::sync::Arc;

    fn skill(wm: MockWorkingMemoryStore) -> WorkingMemorySkill {
        WorkingMemorySkill::new(Arc::new(wm))
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill(MockWorkingMemoryStore::default()).tools().len(), 1);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let s = skill(MockWorkingMemoryStore::default());
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(s.execute("not_a_tool", None));
        assert!(r.is_none());
    }

    // -- notify_user ----------------------------------------------------------

    #[test]
    fn a_message_to_a_person_loses_the_relay_header_only() {
        let m = "Read x.\n\n[FINDINGS FROM THE `intake` SUB-AGENT — relay these faithfully.]\n\nWhat: y\n\n[TOOLS `intake` ACTUALLY RAN: manage_todo ✗×1]";
        assert_eq!(
            without_relay_header(m),
            "Read x.\n\nWhat: y\n\n[TOOLS `intake` ACTUALLY RAN: manage_todo ✗×1]"
        );
        assert_eq!(without_relay_header("plain"), "plain");
    }

    #[tokio::test]
    async fn notify_user_empty_message_sends_nothing() {
        let r = result_json(
            skill(MockWorkingMemoryStore::default())
                .execute("notify_user", Some(serde_json::json!({"message": "  \n"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["sent"], false);
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
        let wm = MockWorkingMemoryStore {
            push_result: Err("store down".into()),
            ..Default::default()
        };
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
}
