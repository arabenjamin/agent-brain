//! MCP tool definitions and handlers.

use std::sync::Arc;
use serde_json::Value;
use tracing::debug;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::skills::Skill;

/// Tool registry containing all available tools.
pub struct ToolRegistry {
    skills: Vec<Box<dyn Skill>>,
}

impl ToolRegistry {
    /// Create a new tool registry.
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
        }
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
}

impl ToolHandler {
    /// Create a new tool handler.
    pub fn new(skills: Vec<Box<dyn Skill>>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        debug!(tool = %name, "Executing tool");

        for skill in self.skills.iter() {
            if let Some(result) = skill.execute(name, arguments.clone()).await {
                return result;
            }
        }

        ToolCallResult::error(format!("Unknown tool: {}", name))
    }
}
