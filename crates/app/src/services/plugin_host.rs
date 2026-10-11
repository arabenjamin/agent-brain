//! The host side of `plugin-api` (CORE_AND_PLUGINS_PLAN Phase 4): each
//! capability a plugin is given is an existing core service behind the
//! plugin API's narrow trait. Added capability by capability, as packs move.

use std::sync::Arc;

use agent_brain_plugin_api::{
    Graph, Knowledge, Llm, Map, PluginContext, Telemetry, Value, Workflows, async_trait,
};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::traits::{KnowledgeStore, LlmProvider};

/// What the host has to hand out; `None` = that capability is not offered.
#[derive(Clone, Default)]
pub struct Host {
    pub neo4j: Option<Neo4jClient>,
    pub knowledge: Option<Arc<dyn KnowledgeStore>>,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub telemetry: Option<TelemetryClient>,
}

impl Host {
    /// The context for one plugin.
    pub fn context(&self, plugin: &str, env: agent_brain_plugin_api::EnvFn) -> PluginContext {
        let mut ctx = PluginContext::new(plugin, env)
            .with_files(Arc::new(|kind: &str| {
                crate::services::plugins::Plugins::global().files(kind, "yaml")
            }))
            .with_day_start(Arc::new(crate::services::clock::local_day_start_utc));
        if let Some(n) = &self.neo4j {
            ctx.graph = Some(Arc::new(HostGraph(n.clone())));
            ctx.workflows = Some(Arc::new(HostWorkflows(n.clone())));
        }
        ctx.knowledge = self
            .knowledge
            .clone()
            .map(|k| Arc::new(HostKnowledge(k)) as Arc<dyn Knowledge>);
        ctx.llm = self
            .llm
            .clone()
            .map(|l| Arc::new(HostLlm(l)) as Arc<dyn Llm>);
        ctx.telemetry = self
            .telemetry
            .clone()
            .map(|t| Arc::new(HostTelemetry(t)) as Arc<dyn Telemetry>);
        ctx
    }
}

struct HostGraph(Neo4jClient);

#[async_trait]
impl Graph for HostGraph {
    async fn query(&self, cypher: &str, params: Value) -> anyhow::Result<Vec<Value>> {
        let mut q = neo4rs::query(cypher);
        match params {
            Value::Object(m) => {
                for (k, v) in m {
                    let bolt = neo4rs::BoltType::try_from(v)
                        .map_err(|e| anyhow::anyhow!("parameter ${k}: {e}"))?;
                    q = q.param(&k, bolt);
                }
            }
            Value::Null => {}
            _ => anyhow::bail!("graph parameters must be a JSON object"),
        }
        let rows = self.0.execute(q).await?;
        Ok(rows
            .iter()
            .map(|r| r.to::<Value>().unwrap_or(Value::Null))
            .collect())
    }
}

struct HostKnowledge(Arc<dyn KnowledgeStore>);

#[async_trait]
impl Knowledge for HostKnowledge {
    async fn store_note(
        &self,
        content: &str,
        note_type: &str,
        source_context: Option<&str>,
    ) -> anyhow::Result<String> {
        self.0
            .store_note(content, Some(note_type), source_context, None, None)
            .await
            .map(|(id, _)| id)
    }
}

struct HostLlm(Arc<dyn LlmProvider>);

#[async_trait]
impl Llm for HostLlm {
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String> {
        self.0.generate(prompt, system).await
    }

    async fn generate_json(
        &self,
        prompt: &str,
        system: Option<&str>,
        required_keys: &[&str],
        max_retries: u32,
    ) -> anyhow::Result<Value> {
        self.0
            .generate_json(prompt, system, required_keys, max_retries)
            .await
    }
}

struct HostWorkflows(Neo4jClient);

#[async_trait]
impl Workflows for HostWorkflows {
    async fn start_run(
        &self,
        workflow: &str,
        inputs: &Map<String, Value>,
        goal: Option<&str>,
        context: Option<&str>,
    ) -> Result<String, String> {
        crate::services::workflows::start_run(&self.0, Some(workflow), inputs, goal, context, None)
            .await
            .map(|r| r.task_id)
    }
}

struct HostTelemetry(TelemetryClient);

impl Telemetry for HostTelemetry {
    fn log_gap(&self, query: &str, tool: Option<&str>, reason: &str) {
        if let Err(e) = self.0.log_knowledge_gap(query, tool, reason) {
            tracing::warn!(error = %e, "plugin host: knowledge gap not logged");
        }
    }
}
