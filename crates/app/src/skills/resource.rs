//! ResourceSkill — 1 tool for cross-agent named resource sharing.
//!
//! Agents running in parallel background jobs can register, look up, list,
//! and release named resources via a shared in-process `ResourceRegistry`.
//! The canonical use-case is sharing a WebSocket `connection_id` so that
//! an agent established a connection that a later agent in the chain can reuse.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::services::resource_registry::ResourceRegistry;
use crate::skills::Skill;

pub struct ResourceSkill {
    registry: Arc<ResourceRegistry>,
}

impl ResourceSkill {
    pub fn new(registry: Arc<ResourceRegistry>) -> Self {
        Self { registry }
    }

    fn resource_def() -> ToolDefinition {
        ToolDefinition {
            name: "resource".to_string(),
            description: "Manage named resources in the shared in-process registry. \
                Other concurrent agents can retrieve them by key. Useful for sharing \
                WebSocket connection IDs, auth tokens, or API sessions across jobs. \
                Actions: register, get, list, release."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["register", "get", "list", "release"],
                        "description": "Operation: register (store), get (retrieve), list (all live), release (remove)."
                    },
                    "key": {
                        "type": "string",
                        "description": "Unique resource name. Required for register, get, release."
                    },
                    "value": {
                        "type": "string",
                        "description": "Resource value (e.g. connection ID, token). Required for register."
                    },
                    "resource_type": {
                        "type": "string",
                        "description": "Logical type tag — e.g. \"ws_connection\", \"auth_token\". Default: \"generic\". Used as filter for list."
                    },
                    "ttl_secs": {
                        "type": "integer",
                        "description": "Optional TTL in seconds for register. Removed on next access after expiry."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Optional extra metadata for register."
                    }
                }
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_register(&self, args: &Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            key: String,
            value: String,
            #[serde(default = "default_resource_type")]
            resource_type: String,
            ttl_secs: Option<u64>,
            metadata: Option<Value>,
        }
        fn default_resource_type() -> String {
            "generic".to_string()
        }

        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        self.registry
            .register(
                &input.key,
                &input.value,
                &input.resource_type,
                input.ttl_secs,
                input.metadata,
            )
            .await;

        ToolCallResult::success_json(json!({
            "key": input.key,
            "resource_type": input.resource_type,
            "status": "registered",
            "ttl_secs": input.ttl_secs,
        }))
    }

    async fn handle_get(&self, args: &Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            key: String,
        }
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        match self.registry.get(&input.key).await {
            Some(entry) => ToolCallResult::success_json(entry),
            None => ToolCallResult::success_json(json!({
                "key": input.key,
                "found": false,
                "reason": "not found or expired"
            })),
        }
    }

    async fn handle_list(&self, args: &Value) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            resource_type: Option<String>,
        }
        let input: Input = serde_json::from_value(args.clone()).unwrap_or_default();

        let entries = self.registry.list(input.resource_type.as_deref()).await;

        ToolCallResult::success_json(json!({
            "count": entries.len(),
            "resources": entries,
        }))
    }

    async fn handle_release(&self, args: &Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            key: String,
        }
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        let removed = self.registry.release(&input.key).await;

        ToolCallResult::success_json(json!({
            "key": input.key,
            "released": removed,
        }))
    }

    async fn handle_resource(&self, args: Option<Value>) -> ToolCallResult {
        let args = args.unwrap_or_default();
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => return ToolCallResult::error("Missing required field: action".to_string()),
        };

        match action.as_str() {
            "register" => self.handle_register(&args).await,
            "get" => self.handle_get(&args).await,
            "list" => self.handle_list(&args).await,
            "release" => self.handle_release(&args).await,
            other => ToolCallResult::error(format!(
                "Unknown action '{other}'. Valid actions: register, get, list, release"
            )),
        }
    }
}

#[async_trait]
impl Skill for ResourceSkill {
    fn name(&self) -> &str {
        "resource"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::resource_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "resource" => Some(self.handle_resource(args).await),
            _ => None,
        }
    }
}
