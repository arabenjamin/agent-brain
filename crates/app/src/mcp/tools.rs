//! MCP tool definitions and handlers.

use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

use agent_brain_protocol::{Content, ToolCallResult, ToolDefinition, ToolHandlerTrait};
use async_trait::async_trait;
use crate::repository::TelemetryClient;
use crate::skills::Skill;

/// Tool registry containing all available tools.
pub struct ToolRegistry {
    skills: Vec<Box<dyn Skill>>,
}

impl ToolRegistry {
    /// Create a new tool registry.
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Add a skill to the registry.
    pub fn register_skill(&mut self, skill: Box<dyn Skill>) {
        self.skills.push(skill);
    }

    /// Clear all registered skills (used before re-registration on reload).
    pub fn clear(&mut self) {
        self.skills.clear();
    }

    /// Get all tool definitions from all skills.
    pub fn list(&self) -> Vec<ToolDefinition> {
        self.skills.iter().flat_map(|s| s.tools()).collect()
    }

    /// Get a tool definition by name.
    pub fn get(&self, name: &str) -> Option<ToolDefinition> {
        for skill in &self.skills {
            if let Some(tool) = skill.tools().into_iter().find(|t| t.name == name) {
                return Some(tool);
            }
        }
        None
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Handler for executing MCP tools.
#[derive(Clone)]
pub struct ToolHandler {
    skills: Arc<Vec<Box<dyn Skill>>>,
    telemetry: Option<TelemetryClient>,
}

impl ToolHandler {
    /// Create a new tool handler.
    pub fn new(skills: Vec<Box<dyn Skill>>) -> Self {
        Self {
            skills: Arc::new(skills),
            telemetry: None,
        }
    }

    /// Attach a telemetry client to log every tool call.
    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        debug!(tool = %name, "Executing tool");

        let start = Instant::now();

        for skill in self.skills.iter() {
            if let Some(result) = skill.execute(name, arguments.clone()).await {
                if let Some(ref tel) = self.telemetry {
                    let prompt = format!(
                        "{}: {}",
                        name,
                        arguments
                            .as_ref()
                            .map(|a| a.to_string())
                            .unwrap_or_default()
                    );
                    let response = result
                        .content
                        .first()
                        .and_then(|c| {
                            if let Content::Text { text } = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .unwrap_or("");
                    let success = result.is_error.is_none();
                    let latency_ms = start.elapsed().as_millis() as u64;
                    let _ = tel.log_interaction(
                        &prompt,
                        response,
                        Some(&serde_json::json!([name])),
                        success,
                        latency_ms,
                        "agent-brain",
                    );
                }
                return result;
            }
        }

        ToolCallResult::error(format!("Unknown tool: {}", name))
    }
}

#[async_trait]
impl ToolHandlerTrait for ToolHandler {
    async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        // Delegate to the inherent method.
        ToolHandler::execute(self, name, arguments).await
    }
}
