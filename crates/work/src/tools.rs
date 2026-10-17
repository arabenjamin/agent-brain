//! MCP tool definitions and handlers.

use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

use agent_brain_protocol::Skill;
use agent_brain_protocol::{Content, ToolCallResult, ToolDefinition, ToolHandlerTrait};
use agent_brain_repository::TelemetryClient;
use async_trait::async_trait;

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

    /// Return each skill's name and the tool names it owns.
    pub fn list_skills(&self) -> Vec<serde_json::Value> {
        self.skills
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name":  s.name(),
                    "tools": s.tools().iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    /// Return each skill's name with its full tool definitions.
    /// Used by the self-model sync to attribute ToolDef nodes to skills.
    pub fn skills_with_tools(&self) -> Vec<(String, Vec<ToolDefinition>)> {
        self.skills
            .iter()
            .map(|s| (s.name().to_string(), s.tools()))
            .collect()
    }

    /// Run a tool through the first skill that claims it (dispatch order),
    /// for REST handlers that serve what a tool computes (a lookup view).
    /// `None` when no skill offers it.
    pub async fn call(&self, name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        for skill in &self.skills {
            if skill.tools().iter().any(|t| t.name == name) {
                return skill.execute(name, arguments).await;
            }
        }
        None
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

    /// Telemetry handle for callers that need catalog access at execution
    /// time (the queue's per-step model router).
    pub fn telemetry(&self) -> Option<&TelemetryClient> {
        self.telemetry.as_ref()
    }

    /// Attach a telemetry client to log every tool call.
    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Whether some skill offers `name`.
    pub fn has_tool(&self, name: &str) -> bool {
        self.skills
            .iter()
            .any(|s| s.tools().iter().any(|t| t.name == name))
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        debug!(tool = %name, "Executing tool");

        let mode = crate::validation::ArgCheckMode::from_env();
        if let Some(refusal) = self.check_arguments(mode, name, arguments.as_ref()) {
            return refusal;
        }

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

impl ToolHandler {
    /// The dispatch check: the call's arguments against the schema of the
    /// tool that will run it (the first skill that declares the name, the
    /// same skill dispatch picks). Every caller passes here: chat, agents,
    /// workflow steps, MCP. A misfit is recorded in `arg_checks`; in
    /// `enforce` mode the call is refused with what fits, in `shadow` it runs.
    fn check_arguments(
        &self,
        mode: crate::validation::ArgCheckMode,
        name: &str,
        arguments: Option<&Value>,
    ) -> Option<ToolCallResult> {
        if mode == crate::validation::ArgCheckMode::Off {
            return None;
        }
        let schema = self
            .skills
            .iter()
            .find_map(|s| s.tools().into_iter().find(|t| t.name == name))?
            .input_schema;
        let problems = crate::validation::check_tool_arguments(name, &schema, arguments);
        if problems.is_empty() {
            return None;
        }
        let refused = mode == crate::validation::ArgCheckMode::Enforce;
        let purpose = agent_brain_llm::routing::RouteContext::current().purpose;
        tracing::warn!(
            tool = %name,
            purpose = purpose.as_str(),
            mode = mode.as_str(),
            problems = %problems.join("; "),
            "tool arguments do not fit the schema"
        );
        if let Some(tel) = &self.telemetry {
            let args = arguments.map(Value::to_string).unwrap_or_default();
            let _ = tel.log_arg_check(
                name,
                purpose.as_str(),
                mode.as_str(),
                refused,
                &problems,
                &args,
                agent_brain_llm::ctx::current_agent().as_deref(),
            );
        }
        refused.then(|| {
            ToolCallResult::error(format!(
                "Arguments rejected; {name} did not run. {}",
                problems.join(". ")
            ))
        })
    }
}

#[async_trait]
impl ToolHandlerTrait for ToolHandler {
    async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        // Delegate to the inherent method.
        ToolHandler::execute(self, name, arguments).await
    }
}

fn retrieval_cell() -> &'static std::sync::RwLock<Vec<&'static str>> {
    static C: std::sync::OnceLock<std::sync::RwLock<Vec<&'static str>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::RwLock::new(Vec::new()))
}

/// Tools the accepted plugins say count as retrieval (the chat write guard).
pub fn plugin_retrieval_tools() -> Vec<&'static str> {
    retrieval_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Set once plugins are accepted (`services::plugins::plugin_skills`).
pub fn set_plugin_retrieval_tools(tools: Vec<&'static str>) {
    *retrieval_cell().write().unwrap_or_else(|e| e.into_inner()) = tools;
}

#[cfg(test)]
mod arg_check_tests {
    use super::*;
    use crate::validation::ArgCheckMode;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl Skill for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn tools(&self) -> Vec<agent_brain_protocol::ToolDefinition> {
            vec![agent_brain_protocol::ToolDefinition {
                name: "list_things".into(),
                description: "d".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(&self, tool: &str, _a: Option<Value>) -> Option<ToolCallResult> {
            (tool == "list_things").then(|| ToolCallResult::success_text("ran".to_string()))
        }
    }

    fn handler() -> ToolHandler {
        ToolHandler::new(vec![Box::new(Echo)])
    }

    #[test]
    fn shadow_lets_a_misfit_run() {
        let h = handler();
        let a = json!({"status": "failed"});
        assert!(
            h.check_arguments(ArgCheckMode::Shadow, "list_things", Some(&a))
                .is_none()
        );
    }

    #[test]
    fn enforce_refuses_a_misfit_and_says_what_fits() {
        let h = handler();
        let a = json!({"status": "failed"});
        let r = h
            .check_arguments(ArgCheckMode::Enforce, "list_things", Some(&a))
            .expect("refused");
        assert_eq!(r.is_error, Some(true));
        let text = format!("{:?}", r.content);
        assert!(
            text.contains("did not run") && text.contains("takes no arguments"),
            "{text}"
        );
    }

    #[test]
    fn a_fitting_call_and_an_unknown_tool_pass_untouched() {
        let h = handler();
        assert!(
            h.check_arguments(ArgCheckMode::Enforce, "list_things", None)
                .is_none()
        );
        let a = json!({"x": 1});
        assert!(
            h.check_arguments(ArgCheckMode::Enforce, "no_such_tool", Some(&a))
                .is_none()
        );
    }
}
