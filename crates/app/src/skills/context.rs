//! ContextSkill — runtime management of context profiles.
//!
//! 1 tool: context (action: assign | build)

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::services::context_builder::ContextBuilderService;
use crate::skills::Skill;

pub struct ContextSkill {
    builder: Arc<ContextBuilderService>,
}

impl ContextSkill {
    pub fn new(builder: Arc<ContextBuilderService>) -> Self {
        Self { builder }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    // list_context_profiles and get_context_profile are served by
    // GET /api/contexts and GET /api/contexts/:name (REST API)

    fn context_def() -> ToolDefinition {
        ToolDefinition {
            name: "context".to_string(),
            description: "Manage agent context profiles. \
                action=assign: auto-assign the best profile for a goal using text overlap \
                (falls back to LLM classifier for ambiguous goals). \
                action=build: resolve a named profile and return its full context bundle \
                (tool allowlist, system prompt, pre-loaded notes)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["assign", "build"],
                        "description": "assign: pick best profile for a goal. build: inspect a profile's bundle."
                    },
                    "goal": {
                        "type": "string",
                        "description": "Goal string (required for action=assign)."
                    },
                    "profile": {
                        "type": "string",
                        "description": "Profile name (required for action=build)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_context(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            action: String,
            goal: Option<String>,
            profile: Option<String>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        match input.action.as_str() {
            "assign" => {
                let Some(goal) = input.goal else {
                    return ToolCallResult::error(
                        "`goal` is required for action=assign".to_string(),
                    );
                };
                let profile = self.builder.auto_assign(&goal).await;
                let method = if profile == "general" {
                    "fallback"
                } else {
                    "semantic"
                };
                ToolCallResult::success_json(json!({
                    "goal": goal,
                    "profile": profile,
                    "method": method,
                }))
            }
            "build" => {
                let Some(profile) = input.profile else {
                    return ToolCallResult::error(
                        "`profile` is required for action=build".to_string(),
                    );
                };
                match self.builder.build_bundle(&profile).await {
                    Ok(bundle) => ToolCallResult::success_json(json!({
                        "profile":               bundle.profile.name,
                        "tools":                 bundle.profile.tools,
                        "tool_count":            bundle.profile.tools.len(),
                        "system_prompt":         bundle.profile.system_prompt,
                        "token_budget":          bundle.profile.token_budget,
                        "pre_loaded_notes":      bundle.pre_loaded_notes,
                        "pre_loaded_note_count": bundle.pre_loaded_notes.len(),
                        "model_preference":      bundle.profile.model_preference,
                        "provider_hint":         bundle.profile.provider_hint,
                    })),
                    Err(e) => ToolCallResult::error(format!("Failed to build context bundle: {e}")),
                }
            }
            other => {
                ToolCallResult::error(format!("Unknown action `{other}`. Use assign or build."))
            }
        }
    }
}

#[async_trait]
impl Skill for ContextSkill {
    fn name(&self) -> &str {
        "Context Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::context_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "context" => Some(self.handle_context(args).await),
            _ => None,
        }
    }
}
