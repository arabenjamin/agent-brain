//! The host side of `plugin-api` (CORE_AND_PLUGINS_PLAN Phase 4): each
//! capability a plugin is given is an existing core service behind the
//! plugin API's narrow trait. Added capability by capability, as packs move.

use std::sync::Arc;

use agent_brain_plugin_api::{
    Activity, Entities, Graph, Knowledge, Llm, Map, PluginContext, Telemetry, Value, Workflows,
    async_trait,
};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::queue::{ChainStep, QueueService};
use crate::services::traits::{KnowledgeStore, LlmProvider, WorkingMemoryStore};

/// What the host has to hand out; `None` = that capability is not offered.
#[derive(Clone, Default)]
pub struct Host {
    pub neo4j: Option<Neo4jClient>,
    pub knowledge: Option<Arc<dyn KnowledgeStore>>,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub telemetry: Option<TelemetryClient>,
    pub queue: Option<Arc<QueueService>>,
}

impl Host {
    /// The context for one plugin, whose bundle is at `root`.
    pub fn context(
        &self,
        plugin: &str,
        root: &std::path::Path,
        env: agent_brain_plugin_api::EnvFn,
    ) -> PluginContext {
        let mut ctx = PluginContext::new(plugin, env)
            .with_root(root.to_path_buf())
            .with_background(crate::services::plugins::background_allowed())
            .with_files(Arc::new(|kind: &str| {
                crate::services::plugins::Plugins::global().files(kind, "yaml")
            }))
            .with_day_start(Arc::new(crate::services::clock::local_day_start_utc));
        if let Some(n) = &self.neo4j {
            ctx.graph = Some(Arc::new(HostGraph(n.clone())));
            ctx.workflows = Some(Arc::new(HostWorkflows {
                neo4j: n.clone(),
                queue: self.queue.clone(),
            }));
            ctx.entities = Some(Arc::new(HostEntities(n.clone())));
            ctx.activity = Some(Arc::new(HostActivity(n.clone())));
        }
        ctx.knowledge = self.knowledge.clone().map(|k| {
            Arc::new(HostKnowledge {
                store: k,
                neo4j: self.neo4j.clone(),
            }) as Arc<dyn Knowledge>
        });
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

struct HostKnowledge {
    store: Arc<dyn KnowledgeStore>,
    neo4j: Option<Neo4jClient>,
}

#[async_trait]
impl Knowledge for HostKnowledge {
    async fn store_note(
        &self,
        content: &str,
        note_type: &str,
        source_context: Option<&str>,
    ) -> anyhow::Result<String> {
        self.store
            .store_note(content, Some(note_type), source_context, None, None)
            .await
            .map(|(id, _)| id)
    }

    async fn push_session_entry(
        &self,
        session_id: &str,
        text: &str,
        role: &str,
    ) -> anyhow::Result<()> {
        let neo4j = self
            .neo4j
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no graph for working memory"))?;
        neo4j
            .push_entry(
                &uuid::Uuid::new_v4().to_string(),
                session_id,
                text,
                role,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
            .map(|_| ())
    }
}

struct HostLlm(Arc<dyn LlmProvider>);

#[async_trait]
impl Llm for HostLlm {
    fn model_name(&self) -> String {
        crate::services::queue::SELECTED_LLM
            .try_with(|v| v.as_ref().map(|c| c.model.clone()))
            .ok()
            .flatten()
            .unwrap_or_else(|| self.0.model_name().to_string())
    }

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

struct HostWorkflows {
    neo4j: Neo4jClient,
    queue: Option<Arc<QueueService>>,
}

#[async_trait]
impl Workflows for HostWorkflows {
    async fn start_run(
        &self,
        workflow: &str,
        inputs: &Map<String, Value>,
        goal: Option<&str>,
        context: Option<&str>,
    ) -> Result<String, String> {
        crate::services::workflows::start_run(
            &self.neo4j,
            Some(workflow),
            inputs,
            goal,
            context,
            None,
        )
        .await
        .map(|r| r.task_id)
    }

    async fn enqueue_tool_step(
        &self,
        tool: &str,
        arguments: Value,
        description: &str,
    ) -> Result<String, String> {
        let queue = self
            .queue
            .as_ref()
            .ok_or_else(|| "the job queue is not running".to_string())?;
        // Disabled workflows count too: an on-demand call works while the
        // schedule is paused.
        let workflows = self
            .neo4j
            .list_workflows(false)
            .await
            .map_err(|e| e.to_string())?;
        let mut step = workflows
            .iter()
            .find_map(|w| {
                serde_json::from_str::<Vec<ChainStep>>(&w.steps)
                    .ok()?
                    .into_iter()
                    .find(|s| s.tool_name == tool)
            })
            .ok_or_else(|| format!("no workflow has a `{tool}` step to copy"))?;
        step.arguments = Some(arguments);
        step.description = Some(description.to_string());
        queue
            .enqueue_chain(&[step], None)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| "the queue returned no job id".to_string())
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

struct HostEntities(Neo4jClient);

#[async_trait]
impl Entities for HostEntities {
    async fn merge_candidates(&self, entity: Option<&str>, limit: usize) -> anyhow::Result<Value> {
        crate::services::entity_resolution::candidates(&self.0, entity, limit).await
    }

    fn demonym_words(&self) -> Vec<String> {
        crate::services::entity_resolution::Rules::global().demonym_words()
    }
}

struct HostActivity(Neo4jClient);

#[async_trait]
impl Activity for HostActivity {
    async fn busy(&self) -> bool {
        crate::services::topics::busy(&self.0).await
    }
}
