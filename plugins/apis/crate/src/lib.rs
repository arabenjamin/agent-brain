//! The `apis` plugin (CODEBASE_SPLIT_PLAN Phase 3): registering and
//! understanding outside APIs. The core keeps `http_request`, which calls
//! them through a stored `(:ApiContext)`, and the `apis/*.yaml` seeds.
//!
//! - `ingest.rs`: `ingest_api`, from an OpenAPI spec, a docs page or a source
//!   tree (a model reads the page or the routes only when no spec is found).
//! - `context.rs`: `define_api_context`, one API by hand, its GET endpoints
//!   probed through the core's `http_request` (`ctx.http`).
//! - `tags.rs`: the kind and topics an API is tagged with (API_USE_PLAN).
//! - `skill.rs`: the skill, and `list_apis` / `list_endpoints`.
//!
//! Stamped from `templates/plugin` (`scripts/plugin_template.sh`).

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill};

mod context;
mod ingest;
mod skill;
mod tags;

pub struct Apis;

impl Plugin for Apis {
    fn name(&self) -> &'static str {
        "apis"
    }

    /// No tools without a graph: an API is its graph nodes.
    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        let Some(graph) = ctx.graph.clone() else {
            return vec![];
        };
        vec![Arc::new(skill::ApisSkill::new(
            graph,
            ctx.llm.clone(),
            ctx.http.clone(),
        )) as Arc<dyn Skill>]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_plugin_yaml() {
        let want = format!("name: {}", Apis.name());
        assert!(
            include_str!("../../plugin.yaml").lines().any(|l| l == want),
            "plugin.yaml has no `{want}` line"
        );
    }
}
