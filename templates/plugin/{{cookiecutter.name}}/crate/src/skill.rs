{%- set tools = cookiecutter.tools.split(',') | map('trim') | reject('equalto', '') | list -%}
//! The `{{ cookiecutter.name }}` plugin's tools. Each one's input schema is
//! the bundle's `schemas/tools/<tool>.json`, loaded by `tool_schema!`. A
//! tool's name is unique across the brain: dispatch is first match.

use serde_json::Value;

use agent_brain_plugin_api::protocol::tool_schema;
use agent_brain_plugin_api::{Skill, ToolCallResult, ToolDefinition, async_trait};

pub struct {{ cookiecutter.__struct }}Skill;

#[async_trait]
impl Skill for {{ cookiecutter.__struct }}Skill {
    fn name(&self) -> &str {
        "{{ cookiecutter.__struct }}"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
{%- for t in tools %}
            ToolDefinition {
                name: "{{ t }}".to_string(),
                description: "Not built yet: say what {{ t }} does, for a model choosing it."
                    .to_string(),
                input_schema: tool_schema!(plugin "{{ t }}"),
            },
{%- endfor %}
        ]
    }

    async fn execute(&self, tool_name: &str, _arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
{%- for t in tools %}
            "{{ t }}" => Some(ToolCallResult::error("{{ t }} is not built yet.")),
{%- endfor %}
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tools, in the order they are offered.
    const TOOLS: &[&str] = &[{% for t in tools %}"{{ t }}"{% if not loop.last %}, {% endif %}{% endfor %}];

    #[tokio::test]
    async fn every_tool_loads_its_schema_and_is_dispatched() {
        let defs = {{ cookiecutter.__struct }}Skill.tools();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, TOOLS);
        for t in TOOLS {
            let r = {{ cookiecutter.__struct }}Skill.execute(t, None).await;
            assert!(r.is_some(), "{t} is offered but not dispatched");
        }
    }
}
