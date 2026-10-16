//! The host side of `plugin-api` (CORE_AND_PLUGINS_PLAN Phase 4): each
//! capability a plugin is given is an existing core service behind the
//! plugin API's narrow trait. Added capability by capability, as packs move.

use std::sync::Arc;

use agent_brain_plugin_api::{
    Activity, Entities, Feed, FeedItem, Graph, Introspection, Knowledge, Llm, Map, PluginContext,
    Provenance, Search, Telemetry, Value, Workflows, async_trait,
};

use agent_brain_knowledge::KnowledgeStore;
use agent_brain_llm::LlmProvider;
use agent_brain_repository::{Neo4jClient, TelemetryClient};
use agent_brain_work::WorkingMemoryStore;
use agent_brain_work::queue::{ChainStep, QueueService};

/// What the host has to hand out; `None` = that capability is not offered.
#[derive(Clone, Default)]
pub struct Host {
    pub neo4j: Option<Neo4jClient>,
    pub knowledge: Option<Arc<dyn KnowledgeStore>>,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub telemetry: Option<TelemetryClient>,
    pub queue: Option<Arc<QueueService>>,
    pub codebase_dir: Option<std::path::PathBuf>,
    pub workspace_dir: Option<std::path::PathBuf>,
    /// The HTTP transport's source (`mcp::ROUTER_SOURCE`), for introspection.
    pub router_source: &'static str,
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
            .with_background(crate::plugins::background_allowed())
            .with_files(Arc::new(|kind: &str| {
                crate::plugins::Plugins::global().files(kind, "yaml")
            }))
            .with_day_start(Arc::new(agent_brain_graph::clock::local_day_start_utc))
            .with_current_task(Arc::new(agent_brain_llm::ctx::current_owner_task));
        if let Some(n) = &self.neo4j {
            ctx.graph = Some(Arc::new(HostGraph(n.clone())));
            ctx.workflows = Some(Arc::new(HostWorkflows {
                neo4j: n.clone(),
                queue: self.queue.clone(),
            }));
            ctx.entities = Some(Arc::new(HostEntities(n.clone())));
            ctx.activity = Some(Arc::new(HostActivity(n.clone())));
            ctx.feed = Some(Arc::new(HostFeed(n.clone())));
        }
        ctx.introspection = Some(Arc::new(HostIntrospection {
            codebase_dir: self.codebase_dir.clone(),
            workspace_dir: self.workspace_dir.clone(),
            router_source: self.router_source,
        }));
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
        ctx.reader = Some(Arc::new(HostReader(
            agent_brain_retrieve::read::ReadSkill::new(self.telemetry.clone()),
        )));
        ctx.search = Some(Arc::new(HostSearch(
            agent_brain_retrieve::search::SearchSkill::new(
                self.telemetry.clone(),
                self.neo4j.clone(),
            ),
        )));
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

    async fn store_note_as(
        &self,
        content: &str,
        note_type: &str,
        source_context: Option<&str>,
        provenance: Provenance,
    ) -> anyhow::Result<String> {
        use agent_brain_models::ProvenanceFlag as P;
        let flag = match provenance {
            Provenance::UserInput => P::UserInput,
            Provenance::SynthesisInference => P::SynthesisInference,
            Provenance::SystemRecord => P::SystemRecord,
        };
        self.store
            .store_note(content, Some(note_type), source_context, None, Some(flag))
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
        agent_brain_llm::ctx::SELECTED_LLM
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

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        self.0.embed(text).await
    }
}

/// `search_web` as the claim verifier called it from inside the core.
struct HostReader(agent_brain_retrieve::read::ReadSkill);

#[async_trait]
impl agent_brain_plugin_api::Reader for HostReader {
    async fn read(&self, url: &str) -> Result<agent_brain_plugin_api::ReadPage, String> {
        self.0
            .read(url, None)
            .await
            .map(|p| agent_brain_plugin_api::ReadPage {
                reader: p.reader,
                title: p.title,
                text: p.text,
            })
    }
}

struct HostSearch(agent_brain_retrieve::search::SearchSkill);

#[async_trait]
impl Search for HostSearch {
    async fn web(&self, query: &str, count: u8) -> Result<String, String> {
        use agent_brain_protocol::Content;
        use agent_brain_protocol::Skill;
        // External evidence only: with the graph-first block on, the judge
        // read our own note restating a claim and called it corroboration
        // (2026-09-12..24: 41% → 84% corroborated).
        let args = serde_json::json!({ "query": query, "count": count, "prior_knowledge": false });
        let Some(r) = self.0.execute("search_web", Some(args)).await else {
            return Err("search unavailable".into());
        };
        let text = r
            .content
            .first()
            .and_then(|c| match c {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if r.is_error == Some(true) {
            Err(text)
        } else {
            Ok(text)
        }
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
        agent_brain_work::workflows::start_run(
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
        agent_brain_graph::entity_resolution::candidates(&self.0, entity, limit).await
    }

    fn demonym_words(&self) -> Vec<String> {
        agent_brain_graph::entity_resolution::Rules::global().demonym_words()
    }
}

struct HostActivity(Neo4jClient);

#[async_trait]
impl Activity for HostActivity {
    async fn busy(&self) -> bool {
        agent_brain_graph::topics::busy(&self.0).await
    }
}

struct HostFeed(Neo4jClient);

#[async_trait]
impl Feed for HostFeed {
    async fn create_item(&self, item: FeedItem) -> anyhow::Result<String> {
        let new = agent_brain_scheduler::items::NewItem {
            title: item.title,
            description: Some(item.body),
            priority: Some(item.priority),
            patch: agent_brain_repository::TodoItemPatch {
                assignee: Some(Some("user".into())),
                origin_kind: Some(Some(item.origin_kind)),
                origin_ref: Some(Some(item.origin_ref)),
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(agent_brain_scheduler::items::create_item(&self.0, new)
            .await?
            .0
            .id)
    }
}

struct HostIntrospection {
    codebase_dir: Option<std::path::PathBuf>,
    workspace_dir: Option<std::path::PathBuf>,
    router_source: &'static str,
}

impl Introspection for HostIntrospection {
    fn router_source(&self) -> &'static str {
        self.router_source
    }

    fn providers(&self) -> Value {
        agent_brain_llm::provider_registry::snapshot_json()
    }

    fn components(&self) -> Value {
        serde_json::to_value(agent_brain_graph::architecture::components()).unwrap_or_default()
    }

    fn declared_tools(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = crate::plugins::declared_status()
            .into_iter()
            .flat_map(|(p, c)| c.tools.into_iter().map(move |t| (t, p.clone())))
            .collect();
        v.sort();
        v
    }

    fn codebase_dir(&self) -> Option<std::path::PathBuf> {
        self.codebase_dir.clone()
    }

    fn workspace_dir(&self) -> Option<std::path::PathBuf> {
        self.workspace_dir.clone()
    }
}
