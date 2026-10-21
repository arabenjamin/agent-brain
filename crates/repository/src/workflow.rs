//! `(:Workflow)` storage. See `agent_brain_models::Workflow`.

use chrono::{Duration, Utc};
use neo4rs::{BoltNull, BoltType, query};
use tracing::info;
use uuid::Uuid;

use crate::temporal::node_ts;
use crate::{Neo4jClient, RepositoryError};
use agent_brain_models::{Workflow, WorkflowInput};

fn row_to_workflow(row: &neo4rs::Row) -> Result<Workflow, RepositoryError> {
    let node: neo4rs::Node = row
        .get("w")
        .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
    let req = |k: &str| RepositoryError::InvalidData(format!("Workflow is missing {k}"));
    let inputs: Vec<WorkflowInput> = node
        .get::<String>("inputs")
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    Ok(Workflow {
        id: node.get("id").map_err(|_| req("id"))?,
        name: node.get("name").map_err(|_| req("name"))?,
        description: node.get("description").ok(),
        enabled: node.get("enabled").unwrap_or(true),
        interval_seconds: node.get("interval_seconds").ok(),
        steps: node.get("steps").map_err(|_| req("steps"))?,
        inputs,
        checks: node.get("checks").unwrap_or_default(),
        evaluation_rubric: node.get("evaluation_rubric").ok(),
        is_default: node.get("is_default").unwrap_or(false),
        last_run_at: node_ts(&node, "last_run_at"),
        next_run_at: node_ts(&node, "next_run_at"),
        created_at: node_ts(&node, "created_at").unwrap_or_default(),
        updated_at: node_ts(&node, "updated_at").unwrap_or_default(),
        managed_by: node.get("managed_by").ok(),
        paused_reason: node.get("paused_reason").ok(),
        topic: node.get("topic").ok(),
        kind: node.get("kind").ok(),
        plugin: node.get("plugin").ok(),
        unavailable: node.get("unavailable").ok(),
    })
}

/// What a workflow is, as a seed file or an upsert declares it — everything
/// but its runtime state (enabled, timers, ownership).
#[derive(Debug, Clone, Default)]
pub struct WorkflowDef {
    pub name: String,
    pub description: Option<String>,
    pub interval_seconds: Option<i64>,
    /// JSON array of steps.
    pub steps: String,
    pub inputs: Vec<WorkflowInput>,
    pub checks: Vec<String>,
    pub evaluation_rubric: Option<String>,
    pub is_default: bool,
    pub topic: Option<String>,
    pub kind: Option<String>,
    /// The bundle a seed file came from; `None` for one created at runtime.
    pub plugin: Option<String>,
}

/// A partial update; `None` leaves a field unchanged, `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct WorkflowPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub interval_seconds: Option<Option<i64>>,
    pub steps: Option<String>,
    pub inputs: Option<Vec<WorkflowInput>>,
    pub checks: Option<Vec<String>>,
    pub evaluation_rubric: Option<Option<String>>,
    pub is_default: Option<bool>,
    pub next_run_at: Option<Option<String>>,
    pub managed_by: Option<String>,
    pub paused_reason: Option<Option<String>>,
    pub topic: Option<Option<String>>,
    pub kind: Option<Option<String>>,
}

/// Outcome of syncing a seed file onto the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YamlSyncOutcome {
    Created,
    Updated,
    /// A workflow with this name is runtime-owned — left untouched.
    RuntimeOwned,
}

fn opt_str(v: Option<&str>) -> BoltType {
    match v {
        Some(v) => BoltType::from(v),
        None => BoltType::Null(BoltNull),
    }
}

fn opt_i64(v: Option<i64>) -> BoltType {
    match v {
        Some(v) => BoltType::from(v),
        None => BoltType::Null(BoltNull),
    }
}

fn inputs_json(inputs: &[WorkflowInput]) -> String {
    serde_json::to_string(inputs).unwrap_or_else(|_| "[]".into())
}

impl Neo4jClient {
    pub async fn get_workflow(&self, id: &str) -> Result<Option<Workflow>, RepositoryError> {
        let rows = self
            .execute(query("MATCH (w:Workflow {id: $id}) RETURN w").param("id", id))
            .await?;
        rows.first().map(row_to_workflow).transpose()
    }

    pub async fn get_workflow_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let rows = self
            .execute(query("MATCH (w:Workflow {name: $name}) RETURN w").param("name", name))
            .await?;
        rows.first().map(row_to_workflow).transpose()
    }

    /// The workflow a Task with no named workflow runs.
    pub async fn get_default_workflow(&self) -> Result<Option<Workflow>, RepositoryError> {
        let rows = self
            .execute(query(
                "MATCH (w:Workflow {is_default: true}) RETURN w ORDER BY w.name LIMIT 1",
            ))
            .await?;
        rows.first().map(row_to_workflow).transpose()
    }

    /// Every workflow, by name.
    pub async fn list_workflows(
        &self,
        enabled_only: bool,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let cypher = if enabled_only {
            "MATCH (w:Workflow {enabled: true}) RETURN w ORDER BY w.name"
        } else {
            "MATCH (w:Workflow) RETURN w ORDER BY w.name"
        };
        self.execute(query(cypher))
            .await?
            .iter()
            .map(row_to_workflow)
            .collect()
    }

    /// Enabled, scheduled workflows whose `next_run_at` has passed.
    pub async fn get_due_workflows(&self) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = self
            .execute(query(
                "MATCH (w:Workflow {enabled: true}) \
                 WHERE w.interval_seconds IS NOT NULL AND w.next_run_at <= datetime() \
                   AND w.unavailable IS NULL \
                 RETURN w ORDER BY w.next_run_at ASC",
            ))
            .await?;
        rows.iter().map(row_to_workflow).collect()
    }

    /// A run of workflow `name` that has not finished (created, in progress
    /// or blocked) and was created within `within_hours`: the id of the
    /// newest, if any. An older unfinished run is presumed stuck (run repair's
    /// business), so it never blocks a schedule for good.
    pub async fn live_run_of(
        &self,
        name: &str,
        within_hours: i64,
    ) -> Result<Option<String>, RepositoryError> {
        let since = (Utc::now() - Duration::hours(within_hours)).to_rfc3339();
        let rows = self
            .execute(
                query(
                    "MATCH (t:Task {workflow: $name}) \
                     WHERE t.status IN ['created', 'in_progress', 'blocked'] \
                       AND t.created_at >= datetime($since) \
                     RETURN t.id AS id ORDER BY t.created_at DESC LIMIT 1",
                )
                .param("name", name)
                .param("since", since),
            )
            .await?;
        Ok(rows.first().and_then(|r| r.get::<String>("id").ok()))
    }

    /// Create a workflow. A scheduled one is first due now.
    pub async fn create_workflow(
        &self,
        def: &WorkflowDef,
        enabled: bool,
        managed_by: &str,
    ) -> Result<Workflow, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let q = query(
            "CREATE (w:Workflow {id: $id, name: $name, enabled: $enabled, steps: $steps, \
               managed_by: $managed_by, \
               created_at: datetime($now), updated_at: datetime($now)}) \
             SET w.description = $description, w.interval_seconds = $interval, \
                 w.inputs = $inputs, w.checks = $checks, \
                 w.evaluation_rubric = $rubric, w.is_default = $is_default, \
                 w.topic = $topic, w.kind = $kind, w.plugin = $plugin, \
                 w.next_run_at = CASE WHEN $interval IS NULL THEN null ELSE datetime($now) END \
             RETURN w",
        )
        .param("id", id.clone())
        .param("name", def.name.as_str())
        .param("enabled", enabled)
        .param("steps", def.steps.as_str())
        .param("managed_by", managed_by)
        .param("now", now)
        .param("description", opt_str(def.description.as_deref()))
        .param("interval", opt_i64(def.interval_seconds))
        .param("inputs", inputs_json(&def.inputs))
        .param("checks", def.checks.clone())
        .param("rubric", opt_str(def.evaluation_rubric.as_deref()))
        .param("is_default", def.is_default)
        .param("topic", opt_str(def.topic.as_deref()))
        .param("kind", opt_str(def.kind.as_deref()))
        .param("plugin", opt_str(def.plugin.as_deref()));
        let rows = self.execute(q).await?;
        let wf = rows
            .first()
            .map(row_to_workflow)
            .transpose()?
            .ok_or_else(|| RepositoryError::InvalidData("No row returned from CREATE".into()))?;
        if def.is_default {
            self.clear_other_defaults(&wf.id).await?;
        }
        info!(id = %id, name = %def.name, "Created Workflow");
        Ok(wf)
    }

    /// Seed a workflow from its file: create it when absent (with `enabled`,
    /// which the file only sets at creation), overwrite its definition when
    /// yaml-owned, leave it alone when runtime-owned. A schedule added to a
    /// workflow is first due now; a schedule removed clears `next_run_at`.
    pub async fn sync_yaml_workflow(
        &self,
        def: &WorkflowDef,
        enabled_on_create: bool,
    ) -> Result<YamlSyncOutcome, RepositoryError> {
        let Some(existing) = self.get_workflow_by_name(&def.name).await? else {
            self.create_workflow(def, enabled_on_create, "yaml").await?;
            return Ok(YamlSyncOutcome::Created);
        };
        if existing.managed_by.as_deref() == Some("runtime") {
            return Ok(YamlSyncOutcome::RuntimeOwned);
        }
        let now = Utc::now().to_rfc3339();
        self.run(
            query(
                "MATCH (w:Workflow {id: $id}) \
                 SET w.steps = $steps, w.description = $description, \
                     w.next_run_at = CASE WHEN $interval IS NULL THEN null \
                                          WHEN w.next_run_at IS NULL THEN datetime($now) \
                                          ELSE w.next_run_at END, \
                     w.interval_seconds = $interval, \
                     w.inputs = $inputs, w.checks = $checks, w.evaluation_rubric = $rubric, \
                     w.is_default = $is_default, w.topic = $topic, w.kind = $kind, \
                     w.plugin = $plugin, w.managed_by = 'yaml', w.updated_at = datetime($now)",
            )
            .param("id", existing.id.as_str())
            .param("steps", def.steps.as_str())
            .param("description", opt_str(def.description.as_deref()))
            .param("interval", opt_i64(def.interval_seconds))
            .param("inputs", inputs_json(&def.inputs))
            .param("checks", def.checks.clone())
            .param("rubric", opt_str(def.evaluation_rubric.as_deref()))
            .param("is_default", def.is_default)
            .param("topic", opt_str(def.topic.as_deref()))
            .param("kind", opt_str(def.kind.as_deref()))
            .param("plugin", opt_str(def.plugin.as_deref()))
            .param("now", now),
        )
        .await?;
        if def.is_default {
            self.clear_other_defaults(&existing.id).await?;
        }
        Ok(YamlSyncOutcome::Updated)
    }

    /// Record why a workflow cannot run (`None` = it can). Returns the reason
    /// it had before, so a caller can tell a change from a repeat.
    pub async fn set_workflow_unavailable(
        &self,
        id: &str,
        reason: Option<&str>,
    ) -> Result<Option<String>, RepositoryError> {
        let rows = self
            .execute(
                query(
                    "MATCH (w:Workflow {id: $id}) WITH w, w.unavailable AS before \
                     SET w.unavailable = $reason RETURN before",
                )
                .param("id", id)
                .param("reason", opt_str(reason)),
            )
            .await?;
        Ok(rows.first().and_then(|r| r.get::<String>("before").ok()))
    }

    async fn clear_other_defaults(&self, id: &str) -> Result<(), RepositoryError> {
        self.run(
            query(
                "MATCH (w:Workflow {is_default: true}) WHERE w.id <> $id SET w.is_default = false",
            )
            .param("id", id),
        )
        .await
    }

    /// Apply a partial update. `None` if no workflow has this id.
    pub async fn update_workflow(
        &self,
        id: &str,
        patch: &WorkflowPatch,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let mut sets: Vec<String> = vec!["w.updated_at = datetime($now)".into()];
        let mut params: Vec<(&str, BoltType)> = vec![];
        let mut set = |prop: &'static str, v: BoltType, datetime: bool| {
            sets.push(if datetime {
                format!("w.{prop} = CASE WHEN ${prop} IS NULL THEN null ELSE datetime(${prop}) END")
            } else {
                format!("w.{prop} = ${prop}")
            });
            params.push((prop, v));
        };
        if let Some(v) = &patch.name {
            set("name", v.as_str().into(), false);
        }
        if let Some(v) = &patch.description {
            set("description", opt_str(v.as_deref()), false);
        }
        if let Some(v) = patch.enabled {
            set("enabled", v.into(), false);
        }
        if let Some(v) = patch.interval_seconds {
            set("interval_seconds", opt_i64(v), false);
        }
        if let Some(v) = &patch.steps {
            set("steps", v.as_str().into(), false);
        }
        if let Some(v) = &patch.inputs {
            set("inputs", inputs_json(v).into(), false);
        }
        if let Some(v) = &patch.checks {
            set("checks", v.clone().into(), false);
        }
        if let Some(v) = &patch.evaluation_rubric {
            set("evaluation_rubric", opt_str(v.as_deref()), false);
        }
        if let Some(v) = patch.is_default {
            set("is_default", v.into(), false);
        }
        if let Some(v) = &patch.next_run_at {
            set("next_run_at", opt_str(v.as_deref()), true);
        }
        if let Some(v) = &patch.managed_by {
            set("managed_by", v.as_str().into(), false);
        }
        if let Some(v) = &patch.paused_reason {
            set("paused_reason", opt_str(v.as_deref()), false);
        }
        if let Some(v) = &patch.topic {
            set("topic", opt_str(v.as_deref()), false);
        }
        if let Some(v) = &patch.kind {
            set("kind", opt_str(v.as_deref()), false);
        }
        let cypher = format!(
            "MATCH (w:Workflow {{id: $id}}) SET {} RETURN w",
            sets.join(", ")
        );
        let mut q = query(&cypher)
            .param("id", id)
            .param("now", Utc::now().to_rfc3339());
        for (k, v) in params {
            q = q.param(k, v);
        }
        let rows = self.execute(q).await?;
        let wf = rows.first().map(row_to_workflow).transpose()?;
        if patch.is_default == Some(true) && wf.is_some() {
            self.clear_other_defaults(id).await?;
        }
        Ok(wf)
    }

    /// Stamp a scheduled run: `last_run_at = now`, `next_run_at = next`.
    pub async fn record_workflow_run(
        &self,
        id: &str,
        now: &str,
        next_run_at: &str,
    ) -> Result<(), RepositoryError> {
        self.run(
            query(
                "MATCH (w:Workflow {id: $id}) \
                 SET w.last_run_at = datetime($now), w.next_run_at = datetime($next), \
                     w.updated_at = datetime($now)",
            )
            .param("id", id)
            .param("now", now)
            .param("next", next_run_at),
        )
        .await
    }

    /// Delete a workflow. Its past runs (Tasks) keep their `workflow` name.
    pub async fn delete_workflow(&self, id: &str) -> Result<bool, RepositoryError> {
        let rows = self
            .execute(
                query(
                    "MATCH (w:Workflow {id: $id}) WITH w, w.id AS gone DETACH DELETE w RETURN gone",
                )
                .param("id", id),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// `now + interval_seconds`, RFC3339.
    pub fn compute_next_run_at(interval_seconds: i64) -> String {
        (Utc::now() + Duration::seconds(interval_seconds)).to_rfc3339()
    }
}
