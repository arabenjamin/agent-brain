//! ContextSkill — runtime management of context profiles.
//!
//! 4 tools: list_context_profiles, get_context_profile,
//!          auto_assign_context, build_agent_context

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
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

    fn list_context_profiles_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_context_profiles".to_string(),
            description: "List all loaded context profiles. Each profile defines a tool allowlist, \
                system prompt, token budget, and optional pre-load query for focused sub-agent contexts."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn get_context_profile_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_context_profile".to_string(),
            description: "Fetch the full details of a context profile by name, including the \
                complete tools list, system prompt, token budget, and model hints."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Profile name (e.g. 'knowledge-worker', 'task-manager')."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn auto_assign_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "auto_assign_context".to_string(),
            description: "Auto-assign a context profile to a goal string using keyword matching. \
                Returns the best-matching profile name and the assignment method."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The goal or task description to assign a profile to."
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    fn build_agent_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "build_agent_context".to_string(),
            description: "Build a runtime context bundle for a profile: resolves the profile, \
                fetches pre-loaded notes via the profile's query, and returns the assembled context. \
                Use this to inspect what context a sub-agent would receive."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "profile": {
                        "type": "string",
                        "description": "Profile name to build the bundle for."
                    }
                },
                "required": ["profile"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_list_context_profiles(&self) -> ToolCallResult {
        let profiles = self.builder.list_profiles().await;
        let items: Vec<Value> = profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "description": p.description,
                    "tool_count": p.tools.len(),
                    "model_preference": p.model_preference,
                    "provider_hint": p.provider_hint,
                })
            })
            .collect();

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "count": items.len(),
                "profiles": items,
            }))
            .unwrap(),
        )
    }

    async fn handle_get_context_profile(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input { name: String }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        match self.builder.get_profile(&input.name).await {
            Some(profile) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "name": profile.name,
                    "description": profile.description,
                    "tools": profile.tools,
                    "system_prompt": profile.system_prompt,
                    "token_budget": profile.token_budget,
                    "pre_load_query": profile.pre_load_query,
                    "model_preference": profile.model_preference,
                    "provider_hint": profile.provider_hint,
                }))
                .unwrap(),
            ),
            None => ToolCallResult::error(format!("Context profile '{}' not found.", input.name)),
        }
    }

    async fn handle_auto_assign_context(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input { goal: String }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        let profile = self.builder.auto_assign(&input.goal).await;
        let method = if profile == "general" { "fallback" } else { "keyword" };

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "goal": input.goal,
                "profile": profile,
                "method": method,
            }))
            .unwrap(),
        )
    }

    async fn handle_build_agent_context(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input { profile: String }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        match self.builder.build_bundle(&input.profile).await {
            Ok(bundle) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "profile": bundle.profile.name,
                    "tools": bundle.profile.tools,
                    "tool_count": bundle.profile.tools.len(),
                    "system_prompt": bundle.profile.system_prompt,
                    "token_budget": bundle.profile.token_budget,
                    "pre_loaded_notes": bundle.pre_loaded_notes,
                    "pre_loaded_note_count": bundle.pre_loaded_notes.len(),
                    "model_preference": bundle.profile.model_preference,
                    "provider_hint": bundle.profile.provider_hint,
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to build context bundle: {e}")),
        }
    }
}

#[async_trait]
impl Skill for ContextSkill {
    fn name(&self) -> &str {
        "Context Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::list_context_profiles_def(),
            Self::get_context_profile_def(),
            Self::auto_assign_context_def(),
            Self::build_agent_context_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "list_context_profiles"  => Some(self.handle_list_context_profiles().await),
            "get_context_profile"    => Some(self.handle_get_context_profile(args).await),
            "auto_assign_context"    => Some(self.handle_auto_assign_context(args).await),
            "build_agent_context"    => Some(self.handle_build_agent_context(args).await),
            _ => None,
        }
    }
}
