{%- set tools = cookiecutter.tools.split(',') | map('trim') | reject('equalto', '') | list -%}
{%- set labels = cookiecutter.graph_labels.split(',') | map('trim') | reject('equalto', '') | list -%}
//! The `{{ cookiecutter.name }}` plugin: {{ cookiecutter.description }}
//!
//! Stamped from `templates/plugin` (`scripts/plugin_template.sh`). The
//! layout: `skill.rs` holds the tools, whose input schemas are the bundle's
//! `schemas/tools/<tool>.json`; graph access goes in `store.rs`, the bundle's
//! constraints in `schemas/graph/schema.cypher`.

use std::sync::Arc;

{% if labels -%}
use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};
{%- else -%}
use agent_brain_plugin_api::{Plugin, PluginContext, Skill};
{%- endif %}
{%- if tools %}

mod skill;
{%- endif %}

pub struct {{ cookiecutter.__struct }};
{% if labels %}
#[async_trait]
{%- endif %}
impl Plugin for {{ cookiecutter.__struct }} {
    fn name(&self) -> &'static str {
        "{{ cookiecutter.name }}"
    }

    fn tools(&self, _ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
{%- if tools %}
        vec![Arc::new(skill::{{ cookiecutter.__struct }}Skill) as Arc<dyn Skill>]
{%- else %}
        vec![]
{%- endif %}
    }
{%- if labels %}

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        let graph = ctx
            .graph
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no graph"))?;
        // Constraints and indexes: schemas/graph/schema.cypher.
        agent_brain_plugin_api::graph::apply_schema(
            graph.as_ref(),
            agent_brain_plugin_api::protocol::graph_schema!(plugin),
        )
        .await?;
        Ok(())
    }
{%- endif %}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_plugin_yaml() {
        let want = format!("name: {}", {{ cookiecutter.__struct }}.name());
        assert!(
            include_str!("../../plugin.yaml").lines().any(|l| l == want),
            "plugin.yaml has no `{want}` line"
        );
    }
}
