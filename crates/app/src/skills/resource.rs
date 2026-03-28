//! ResourceSkill — 4 tools for cross-agent named resource sharing.
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

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn resource_register_def() -> ToolDefinition {
        ToolDefinition {
            name: "resource_register".to_string(),
            description: "Register a named resource in the shared in-process registry. \
                Other concurrent agents can retrieve it by key. Useful for sharing \
                WebSocket connection IDs, auth tokens, or API sessions across jobs."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["key", "value"],
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Unique name for this resource."
                    },
                    "value": {
                        "type": "string",
                        "description": "Resource value (e.g. a connection ID, token, or URL)."
                    },
                    "resource_type": {
                        "type": "string",
                        "description": "Logical type tag — e.g. \"ws_connection\", \"auth_token\", \"api_session\". Default: \"generic\"."
                    },
                    "ttl_secs": {
                        "type": "integer",
                        "description": "Optional time-to-live in seconds. The resource is removed on next access after expiry."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Optional extra metadata to store alongside the resource value."
                    }
                }
            }),
        }
    }

    fn resource_get_def() -> ToolDefinition {
        ToolDefinition {
            name: "resource_get".to_string(),
            description: "Retrieve a named resource from the shared registry by key. \
                Returns the entry if found and not expired, otherwise reports not found."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Resource key to look up."
                    }
                }
            }),
        }
    }

    fn resource_list_def() -> ToolDefinition {
        ToolDefinition {
            name: "resource_list".to_string(),
            description: "List all live resources in the shared registry. \
                Optionally filter by resource_type. Expired entries are pruned during the scan."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "resource_type": {
                        "type": "string",
                        "description": "Filter by resource type (e.g. \"ws_connection\"). Omit to list all."
                    }
                }
            }),
        }
    }

    fn resource_release_def() -> ToolDefinition {
        ToolDefinition {
            name: "resource_release".to_string(),
            description: "Remove a named resource from the shared registry.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Resource key to release."
                    }
                }
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_register(&self, args: Option<Value>) -> ToolCallResult {
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

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
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

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "key": input.key,
                "resource_type": input.resource_type,
                "status": "registered",
                "ttl_secs": input.ttl_secs,
            }))
            .unwrap(),
        )
    }

    async fn handle_get(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            key: String,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        match self.registry.get(&input.key).await {
            Some(entry) => {
                ToolCallResult::success_text(serde_json::to_string_pretty(&entry).unwrap())
            }
            None => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "key": input.key,
                    "found": false,
                    "reason": "not found or expired"
                }))
                .unwrap(),
            ),
        }
    }

    async fn handle_list(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            resource_type: Option<String>,
        }
        let input: Input = serde_json::from_value(args.unwrap_or_default()).unwrap_or_default();

        let entries = self.registry.list(input.resource_type.as_deref()).await;

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "count": entries.len(),
                "resources": entries,
            }))
            .unwrap(),
        )
    }

    async fn handle_release(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            key: String,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        let removed = self.registry.release(&input.key).await;

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "key": input.key,
                "released": removed,
            }))
            .unwrap(),
        )
    }
}

#[async_trait]
impl Skill for ResourceSkill {
    fn name(&self) -> &str {
        "resource"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::resource_register_def(),
            Self::resource_get_def(),
            Self::resource_list_def(),
            Self::resource_release_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "resource_register" => Some(self.handle_register(args).await),
            "resource_get" => Some(self.handle_get(args).await),
            "resource_list" => Some(self.handle_list(args).await),
            "resource_release" => Some(self.handle_release(args).await),
            _ => None,
        }
    }
}
