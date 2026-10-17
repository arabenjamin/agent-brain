//! Programs (PROGRAMS_PLAN.md Phase 1): the brain's work, organized.
//!
//! A `(:Program {name})` owns work (workflows, the agents and APIs that work
//! relies on) and names the topics it covers. Programs nest by `parent` (the
//! NIA over its verticals). They are seeded from every loaded bundle's
//! `programs/*.yaml` and edited through `manage_program` or REST; both go
//! through [`save`], which checks the whole result first.
//!
//! - **Ownership** is the topics rule: a file owns its program (`managed_by:
//!   yaml`) until an edit makes it `runtime`; `reset` puts the file back. A
//!   deleted seeded program leaves a `(:RetiredProgram)` so the seed does not
//!   bring it back.
//! - **`status` is graph-owned**, like a workflow's `enabled`: a file sets it
//!   only when the program is created, and a status change keeps the owner.
//! - **A program's status reaches its workflows** ([`reconcile`]): a paused or
//!   closed program (or one under such a program) disables its workflows with
//!   a [`PROGRAM_PAUSE`] reason, except those in its `keep_running`; when it is
//!   active again, exactly those come back. A workflow paused for any other
//!   reason is never touched.
//! - **One program per workflow**, so someone is accountable for it. On-demand
//!   chains every program uses belong to a `services` program.
//! - `(:Program)-[:UNDER]->(:Program)` and `(:Program)-[:OWNS]->(:Workflow)` are
//!   derived from the properties after every write ([`relink`]), for views.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use agent_brain_repository::Neo4jClient;

/// What a program is: standing work, work with a deliverable, a question
/// (investigation, case), or on-demand workflows every program uses.
pub const PROGRAM_KINDS: &[&str] = &["department", "project", "investigation", "case", "services"];

pub const PROGRAM_STATUSES: &[&str] = &["active", "paused", "closed"];

/// The `paused_reason` prefix a program's pause writes on its workflows; only
/// a workflow carrying it is resumed by a program.
pub const PROGRAM_PAUSE: &str = "PROGRAM_PAUSE";

/// Kinds that end with an answer: closing one needs a `finding`.
const QUESTION_KINDS: &[&str] = &["investigation", "case"];

/// A charter, success line, label or finding: a manager reads all of them in
/// one prompt, so each stays short.
const TEXT_MAX: usize = 1500;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// The manager's review period (`1d`, `1w`, `30d`).
    pub every: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// A monthly cap for the program's work, in US dollars. Shown against
    /// the scorecard's cost; the routing policy does not enforce it yet.
    pub monthly_usd: f64,
}

fn active() -> String {
    "active".into()
}

/// One program, as a `programs/<name>.yaml` file or a save body writes it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgramDef {
    /// A slug, equal to the file stem.
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// One of [`PROGRAM_KINDS`].
    pub kind: String,
    /// Another program's name; absent = top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// One of [`PROGRAM_STATUSES`].
    #[serde(default = "active")]
    pub status: String,
    /// What it is for, in a few sentences.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub charter: String,
    /// What good output looks like; the manager judges against it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub success: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    /// Workflows of this program that a pause or close leaves running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep_running: Vec<String>,
    /// Agent profiles its work relies on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// API contexts and MCP servers its work relies on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// What a closed investigation or case found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
}

impl ProgramDef {
    pub fn label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            &self.label
        }
    }
}

/// What a program may name, and the other programs it is checked against.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub workflows: HashSet<String>,
    pub topics: HashSet<String>,
    pub agents: HashSet<String>,
    pub apis: HashSet<String>,
    /// Every program as the write would leave them (the one checked may be
    /// among them; it is skipped by name).
    pub programs: Vec<ProgramDef>,
}

fn sorted(set: &HashSet<String>) -> String {
    let mut v: Vec<&String> = set.iter().collect();
    v.sort();
    v.into_iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Check a program against what exists. `Ok` carries the soft warnings
/// (saved, but worth knowing); `Err` names every hard error and what would
/// fit instead.
pub fn check(def: &ProgramDef, inv: &Inventory) -> Result<Vec<String>, String> {
    let mut hard = Vec::new();
    if !(crate::workflows::is_slug(&def.name) && def.name.len() <= 64) {
        hard.push(format!(
            "name '{}' must be a slug of at most 64 characters: lowercase letters, digits and dashes",
            def.name
        ));
    }
    if !PROGRAM_KINDS.contains(&def.kind.as_str()) {
        hard.push(format!(
            "kind '{}' is not one of {}",
            def.kind,
            PROGRAM_KINDS.join(", ")
        ));
    }
    if !PROGRAM_STATUSES.contains(&def.status.as_str()) {
        hard.push(format!(
            "status '{}' is not one of {}",
            def.status,
            PROGRAM_STATUSES.join(", ")
        ));
    }
    for (field, text) in [
        ("label", def.label.as_str()),
        ("charter", def.charter.as_str()),
        ("success", def.success.as_str()),
        ("finding", def.finding.as_deref().unwrap_or("")),
    ] {
        if text.chars().count() > TEXT_MAX {
            hard.push(format!(
                "{field} is {} characters; keep it under {TEXT_MAX}",
                text.chars().count()
            ));
        }
    }

    let others: HashMap<&str, &ProgramDef> = inv
        .programs
        .iter()
        .filter(|p| p.name != def.name)
        .map(|p| (p.name.as_str(), p))
        .collect();
    if let Some(parent) = def.parent.as_deref().filter(|p| !p.is_empty()) {
        if parent == def.name {
            hard.push("a program cannot be its own parent".into());
        } else if !others.contains_key(parent) {
            let mut names: Vec<&str> = others.keys().copied().collect();
            names.sort();
            hard.push(format!(
                "parent '{parent}' is not a program (programs: {})",
                names.join(", ")
            ));
        } else {
            let mut chain = vec![def.name.as_str(), parent];
            let mut cur = parent;
            while let Some(next) = others.get(cur).and_then(|p| p.parent.as_deref()) {
                chain.push(next);
                if next == def.name || chain[..chain.len() - 1].contains(&next) {
                    hard.push(format!("the parent chain loops: {}", chain.join(" → ")));
                    break;
                }
                cur = next;
            }
        }
    }

    let claimed: HashMap<&str, &str> = others
        .values()
        .flat_map(|p| p.workflows.iter().map(|w| (w.as_str(), p.name.as_str())))
        .collect();
    let mut seen = HashSet::new();
    let mut unknown_workflow = false;
    for w in &def.workflows {
        if !seen.insert(w.as_str()) {
            hard.push(format!("workflow '{w}' is listed twice"));
        } else if !inv.workflows.contains(w) {
            hard.push(format!("workflow '{w}' does not exist"));
            unknown_workflow = true;
        } else if let Some(owner) = claimed.get(w.as_str()) {
            hard.push(format!(
                "workflow '{w}' belongs to program '{owner}' (a workflow has one program: \
                 take it out of '{owner}' first)"
            ));
        }
    }
    if unknown_workflow {
        let mut free: Vec<&String> = inv
            .workflows
            .iter()
            .filter(|w| !claimed.contains_key(w.as_str()) && !def.workflows.contains(w))
            .collect();
        free.sort();
        hard.push(format!(
            "workflows in no program: {}",
            free.into_iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for w in &def.keep_running {
        if !def.workflows.contains(w) {
            hard.push(format!(
                "keep_running names '{w}', which is not one of this program's workflows"
            ));
        }
    }
    for (what, names, known) in [
        ("topic", &def.topics, &inv.topics),
        ("agent", &def.agents, &inv.agents),
        ("API", &def.apis, &inv.apis),
    ] {
        let missing: Vec<&String> = names.iter().filter(|n| !known.contains(*n)).collect();
        if !missing.is_empty() {
            hard.push(format!(
                "no {what} named {} (known: {})",
                missing
                    .iter()
                    .map(|m| format!("'{m}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
                sorted(known)
            ));
        }
    }
    if let Some(r) = &def.review
        && let Err(e) = crate::workflow_seeder::parse_every(&r.every)
    {
        hard.push(format!("review.every: {e}"));
    }
    if let Some(b) = &def.budget
        && !(b.monthly_usd.is_finite() && b.monthly_usd >= 0.0)
    {
        hard.push("budget.monthly_usd must be a number of dollars, 0 or more".into());
    }
    if def.status == "closed"
        && QUESTION_KINDS.contains(&def.kind.as_str())
        && def.finding.as_deref().is_none_or(|f| f.trim().is_empty())
    {
        hard.push(format!(
            "closing an {} needs a `finding`: what it found, in a few sentences",
            def.kind
        ));
    }
    if !hard.is_empty() {
        return Err(hard.join("; "));
    }

    let mut soft = Vec::new();
    let has_children = others
        .values()
        .any(|p| p.parent.as_deref() == Some(def.name.as_str()));
    if def.workflows.is_empty() && !has_children {
        soft.push(format!(
            "'{}' has no workflows and no programs under it",
            def.name
        ));
    }
    if def.status != "active" && !def.keep_running.is_empty() {
        soft.push(format!(
            "'{}' is {} but keeps running: {}",
            def.name,
            def.status,
            def.keep_running.join(", ")
        ));
    }
    Ok(soft)
}

/// Take out what a seed file names that this brain does not have (a
/// workflow of a plugin never enabled here), so the rest of the file still
/// seeds. Returns what was dropped, for the warning.
fn prune_unknown(def: &mut ProgramDef, inv: &Inventory) -> Vec<String> {
    let mut dropped = Vec::new();
    for (what, names, known) in [
        ("workflow", &mut def.workflows, &inv.workflows),
        ("topic", &mut def.topics, &inv.topics),
        ("agent", &mut def.agents, &inv.agents),
        ("API", &mut def.apis, &inv.apis),
    ] {
        names.retain(|n| {
            let ok = known.contains(n);
            if !ok {
                dropped.push(format!("{what} '{n}'"));
            }
            ok
        });
    }
    let workflows = def.workflows.clone();
    def.keep_running.retain(|w| workflows.contains(w));
    dropped
}

// ── The effect of a program's status on its workflows ────────────────────

/// A workflow's run state, as [`plan_reconcile`] needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowState {
    pub name: String,
    pub enabled: bool,
    pub paused_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Pause {
        workflow: String,
        reason: String,
    },
    Resume {
        workflow: String,
    },
    /// Still paused by a program, but a different one now holds it.
    Retag {
        workflow: String,
        reason: String,
    },
}

impl Change {
    pub fn describe(&self) -> String {
        match self {
            Change::Pause { workflow, reason } | Change::Retag { workflow, reason } => {
                format!("{workflow}: paused ({reason})")
            }
            Change::Resume { workflow } => format!("{workflow}: resumed"),
        }
    }
}

pub fn is_program_pause(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r.starts_with(PROGRAM_PAUSE))
}

/// The program that holds `name`'s workflows off: itself or the nearest
/// program above it that is not active.
fn blocker<'a>(name: &str, by_name: &HashMap<&str, &'a ProgramDef>) -> Option<&'a ProgramDef> {
    let mut seen = HashSet::new();
    let mut cur = by_name.get(name).copied();
    while let Some(p) = cur {
        if !seen.insert(p.name.as_str()) {
            return None;
        }
        if p.status != "active" {
            return Some(p);
        }
        cur = p.parent.as_deref().and_then(|n| by_name.get(n).copied());
    }
    None
}

/// What has to change so each workflow's state follows its program: paused
/// under a paused or closed program (unless kept running), resumed when no
/// program holds it any more. Only workflows a program paused are resumed.
pub fn plan_reconcile(programs: &[ProgramDef], workflows: &[WorkflowState]) -> Vec<Change> {
    let by_name: HashMap<&str, &ProgramDef> =
        programs.iter().map(|p| (p.name.as_str(), p)).collect();
    let owner: HashMap<&str, &ProgramDef> = programs
        .iter()
        .flat_map(|p| p.workflows.iter().map(move |w| (w.as_str(), p)))
        .collect();
    let mut changes = Vec::new();
    for w in workflows {
        let held = owner.get(w.name.as_str()).and_then(|p| {
            if p.keep_running.contains(&w.name) {
                None
            } else {
                blocker(&p.name, &by_name)
            }
        });
        let ours = is_program_pause(w.paused_reason.as_deref());
        match held {
            Some(b) => {
                let reason = format!("{PROGRAM_PAUSE}: program '{}' is {}", b.name, b.status);
                if w.enabled {
                    changes.push(Change::Pause {
                        workflow: w.name.clone(),
                        reason,
                    });
                } else if ours && w.paused_reason.as_deref() != Some(reason.as_str()) {
                    changes.push(Change::Retag {
                        workflow: w.name.clone(),
                        reason,
                    });
                }
            }
            None if !w.enabled && ours => changes.push(Change::Resume {
                workflow: w.name.clone(),
            }),
            None => {}
        }
    }
    changes
}

// ── Storage ──────────────────────────────────────────────────────────────

/// A program as stored, with who owns it.
#[derive(Debug, Clone, Serialize)]
pub struct Stored {
    #[serde(flatten)]
    pub def: ProgramDef,
    /// `yaml` (its file is synced on every start) or `runtime` (edited; the
    /// seed leaves it alone).
    pub managed_by: String,
    pub plugin: Option<String>,
    /// The file it is seeded from, relative to `PLUGINS_DIR`.
    pub source: Option<String>,
    pub status_changed_at: Option<String>,
    pub updated_at: Option<String>,
}

pub async fn load_all(neo4j: &Neo4jClient) -> anyhow::Result<Vec<Stored>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (p:Program) \
             RETURN p.definition AS d, p.status AS status, p.finding AS finding, \
                    coalesce(p.managed_by, 'yaml') AS mb, p.plugin AS plugin, p.source AS source, \
                    toString(p.status_changed_at) AS changed, toString(p.updated_at) AS updated \
             ORDER BY p.name",
        ))
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let text: String = r.get("d").unwrap_or_default();
        let mut def: ProgramDef = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "programs: a stored definition does not parse; skipped");
                continue;
            }
        };
        // Graph-owned: what the node says, not the definition it was seeded with.
        if let Ok(s) = r.get::<String>("status") {
            def.status = s;
        }
        def.finding = r.get::<String>("finding").ok().filter(|f| !f.is_empty());
        let opt = |k: &str| r.get::<String>(k).ok().filter(|s| !s.is_empty());
        out.push(Stored {
            def,
            managed_by: r.get("mb").unwrap_or_else(|_| "yaml".into()),
            plugin: opt("plugin"),
            source: opt("source"),
            status_changed_at: opt("changed"),
            updated_at: opt("updated"),
        });
    }
    Ok(out)
}

/// Write one program. `status_owned`: the write sets `status` and `finding`
/// (a save); otherwise they are set only when the node is created (a seed).
async fn write(
    neo4j: &Neo4jClient,
    def: &ProgramDef,
    managed_by: &str,
    plugin: Option<&str>,
    source: Option<&str>,
    status_owned: bool,
) -> anyhow::Result<()> {
    let status_set = if status_owned {
        "SET p.status_changed_at = CASE WHEN coalesce(p.status, '') <> $status \
             THEN datetime() ELSE p.status_changed_at END, \
             p.status = $status, p.finding = $finding "
    } else {
        ""
    };
    let cypher = format!(
        "MERGE (p:Program {{name: $name}}) \
         ON CREATE SET p.created_at = datetime(), p.status = $status, p.finding = $finding, \
                       p.status_changed_at = datetime() \
         {status_set}\
         SET p.definition = $definition, p.label = $label, p.kind = $kind, p.parent = $parent, \
             p.workflows = $workflows, p.topics = $topics, p.managed_by = $managed_by, \
             p.budget_monthly_usd = $budget, \
             p.plugin = $plugin, p.source = $source, p.updated_at = datetime()"
    );
    let null = || neo4rs::BoltType::Null(neo4rs::BoltNull);
    let opt = |v: Option<&str>| {
        v.map(|s| neo4rs::BoltType::from(s.to_string()))
            .unwrap_or_else(null)
    };
    neo4j
        .run(
            neo4rs::query(&cypher)
                .param("name", def.name.clone())
                .param("status", def.status.clone())
                .param("finding", opt(def.finding.as_deref()))
                .param("definition", serde_json::to_string(def)?)
                .param("label", def.label().to_string())
                .param("kind", def.kind.clone())
                .param("parent", opt(def.parent.as_deref()))
                .param("workflows", def.workflows.clone())
                .param("topics", def.topics.clone())
                .param(
                    "budget",
                    def.budget
                        .as_ref()
                        .map(|b| neo4rs::BoltType::from(b.monthly_usd))
                        .unwrap_or_else(null),
                )
                .param("managed_by", managed_by.to_string())
                .param("plugin", opt(plugin))
                .param("source", opt(source)),
        )
        .await?;
    Ok(())
}

/// Rebuild the `UNDER` and `OWNS` edges from the programs' properties.
pub async fn relink(neo4j: &Neo4jClient) -> anyhow::Result<()> {
    for cypher in [
        "MATCH (:Program)-[r:UNDER|OWNS]->() DELETE r",
        "MATCH (c:Program) WHERE c.parent IS NOT NULL \
         MATCH (p:Program {name: c.parent}) MERGE (c)-[:UNDER]->(p)",
        "MATCH (p:Program) UNWIND coalesce(p.workflows, []) AS wn \
         MATCH (w:Workflow {name: wn}) MERGE (p)-[:OWNS]->(w)",
    ] {
        neo4j.run(neo4rs::query(cypher)).await?;
    }
    Ok(())
}

async fn names(neo4j: &Neo4jClient, cypher: &str) -> anyhow::Result<HashSet<String>> {
    Ok(neo4j
        .execute(neo4rs::query(cypher))
        .await?
        .first()
        .and_then(|r| r.get::<Vec<String>>("names").ok())
        .unwrap_or_default()
        .into_iter()
        .collect())
}

/// What exists now: workflows, topics, agents, APIs and the stored programs.
pub async fn inventory(neo4j: &Neo4jClient) -> anyhow::Result<Inventory> {
    Ok(Inventory {
        workflows: names(neo4j, "MATCH (w:Workflow) RETURN collect(w.name) AS names").await?,
        topics: agent_brain_graph::topics::Topics::global()
            .defs
            .iter()
            .map(|d| d.name.clone())
            .collect(),
        agents: names(
            neo4j,
            "MATCH (c:ContextProfile) RETURN collect(c.name) AS names",
        )
        .await?,
        apis: names(
            neo4j,
            "MATCH (a) WHERE a:ApiContext OR a:McpServer RETURN collect(DISTINCT a.name) AS names",
        )
        .await?,
        programs: load_all(neo4j).await?.into_iter().map(|s| s.def).collect(),
    })
}

pub async fn workflow_states(neo4j: &Neo4jClient) -> anyhow::Result<Vec<WorkflowState>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (w:Workflow) RETURN w.name AS name, coalesce(w.enabled, true) AS enabled, \
                    w.paused_reason AS reason ORDER BY name",
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(WorkflowState {
                name: r.get("name").ok()?,
                enabled: r.get("enabled").unwrap_or(true),
                paused_reason: r.get::<String>("reason").ok().filter(|s| !s.is_empty()),
            })
        })
        .collect())
}

/// Make every workflow's state follow its program, and say what changed.
pub async fn reconcile(neo4j: &Neo4jClient) -> anyhow::Result<Vec<Change>> {
    let programs: Vec<ProgramDef> = load_all(neo4j).await?.into_iter().map(|s| s.def).collect();
    let changes = plan_reconcile(&programs, &workflow_states(neo4j).await?);
    for c in &changes {
        let q = match c {
            Change::Pause { workflow, reason } => neo4rs::query(
                "MATCH (w:Workflow {name: $name}) SET w.enabled = false, w.paused_reason = $reason, \
                 w.paused_at = datetime(), w.updated_at = datetime()",
            )
            .param("name", workflow.clone())
            .param("reason", reason.clone()),
            Change::Retag { workflow, reason } => neo4rs::query(
                "MATCH (w:Workflow {name: $name}) SET w.paused_reason = $reason",
            )
            .param("name", workflow.clone())
            .param("reason", reason.clone()),
            Change::Resume { workflow } => neo4rs::query(
                "MATCH (w:Workflow {name: $name}) SET w.enabled = true, w.paused_reason = null, \
                 w.paused_at = null, w.updated_at = datetime()",
            )
            .param("name", workflow.clone()),
        };
        neo4j.run(q).await?;
        info!(change = %c.describe(), "programs: a workflow follows its program");
    }
    Ok(changes)
}

/// The program a workflow belongs to, if any.
pub async fn program_of(neo4j: &Neo4jClient, workflow: &str) -> Option<String> {
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (p:Program) WHERE $w IN coalesce(p.workflows, []) RETURN p.name AS name LIMIT 1",
            )
            .param("w", workflow.to_string()),
        )
        .await
        .ok()?
        .first()
        .and_then(|r| r.get::<String>("name").ok())
}

/// Whether a program of this name exists.
pub async fn exists(neo4j: &Neo4jClient, name: &str) -> anyhow::Result<bool> {
    Ok(!neo4j
        .execute(
            neo4rs::query("MATCH (p:Program {name: $n}) RETURN p.name AS n")
                .param("n", name.to_string()),
        )
        .await?
        .is_empty())
}

// ── Seeding ──────────────────────────────────────────────────────────────

/// Parse one `programs/*.yaml`; its name must equal the file stem.
pub fn load_file(path: &Path) -> Result<ProgramDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let def: ProgramDef = serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if def.name != stem {
        return Err(format!(
            "name '{}' must equal the file name '{stem}'",
            def.name
        ));
    }
    Ok(def)
}

/// Every loaded bundle's program files, core first; a name an earlier
/// bundle declared is skipped. `(plugin, source, def)`.
pub fn file_defs(
    plugins: &agent_brain_graph::bundles::Plugins,
) -> Vec<(String, String, ProgramDef)> {
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for (plugin, path) in plugins.files("programs", "yaml") {
        let def = match load_file(&path) {
            Ok(d) => d,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_programs: file skipped");
                continue;
            }
        };
        if let Some(first) = seen.get(&def.name) {
            warn!(name = %def.name, %plugin, %first, "seed_programs: name already declared by an earlier plugin — file skipped");
            continue;
        }
        seen.insert(def.name.clone(), plugin.clone());
        // Relative to the folder it was found in: `personal/programs/nia.yaml`.
        let source = std::iter::once(plugins.dir.clone())
            .chain(agent_brain_graph::bundles::extra_dirs())
            .find_map(|d| path.strip_prefix(&d).ok().map(|p| p.display().to_string()))
            .unwrap_or_else(|| path.display().to_string());
        out.push((plugin, source, def));
    }
    out
}

async fn retired(neo4j: &Neo4jClient) -> anyhow::Result<HashSet<String>> {
    names(
        neo4j,
        "MATCH (r:RetiredProgram) RETURN collect(r.name) AS names",
    )
    .await
}

/// Seed programs from the loaded bundles, after workflows (a program names
/// them). A file's unknown references are dropped with a warning; a file
/// with a hard error (bad kind, unknown parent, a loop) is skipped. Then the
/// edges are rebuilt and the workflows follow their programs. Returns the
/// number seeded.
pub async fn seed_programs(
    neo4j: &Neo4jClient,
    plugins: &agent_brain_graph::bundles::Plugins,
) -> anyhow::Result<usize> {
    let stored = load_all(neo4j).await?;
    let runtime: HashMap<String, ProgramDef> = stored
        .iter()
        .filter(|s| s.managed_by == "runtime")
        .map(|s| (s.def.name.clone(), s.def.clone()))
        .collect();
    let gone = retired(neo4j).await?;
    let mut inv = inventory(neo4j).await?;

    let mut files: Vec<(String, String, ProgramDef)> = Vec::new();
    for (plugin, source, mut def) in file_defs(plugins) {
        if runtime.contains_key(&def.name) {
            warn!(name = %def.name, "Program is runtime-owned — file skipped (manage_program reset puts the file back)");
            continue;
        }
        if gone.contains(&def.name) {
            continue;
        }
        let dropped = prune_unknown(&mut def, &inv);
        if !dropped.is_empty() {
            warn!(name = %def.name, dropped = %dropped.join(", "), "seed_programs: the file names what this brain does not have; left out");
        }
        // A workflow an edited program or an earlier file holds stays there.
        let held: HashSet<String> = runtime
            .values()
            .flat_map(|p| p.workflows.clone())
            .chain(files.iter().flat_map(|(_, _, d)| d.workflows.clone()))
            .collect();
        def.workflows.retain(|w| {
            let free = !held.contains(w);
            if !free {
                warn!(name = %def.name, workflow = %w, "seed_programs: workflow already in another program; left out");
            }
            free
        });
        let workflows = def.workflows.clone();
        def.keep_running.retain(|w| workflows.contains(w));
        // A stored status wins: it is graph-owned.
        if let Some(s) = stored.iter().find(|s| s.def.name == def.name) {
            def.status = s.def.status.clone();
            def.finding = s.def.finding.clone();
        }
        files.push((plugin, source, def));
    }

    // Check against the whole set the seed would leave; drop a failing file
    // and check again, since a child may have leaned on it.
    loop {
        inv.programs = runtime
            .values()
            .cloned()
            .chain(files.iter().map(|(_, _, d)| d.clone()))
            .collect();
        let bad = files.iter().position(|(_, _, d)| check(d, &inv).is_err());
        match bad {
            Some(i) => {
                let (_, source, d) = files.remove(i);
                let e = check(&d, &inv).err().unwrap_or_default();
                warn!(%source, error = %e, "seed_programs: file skipped");
            }
            None => break,
        }
    }

    for (plugin, source, def) in &files {
        write(neo4j, def, "yaml", Some(plugin), Some(source), false).await?;
    }
    // A yaml-owned program whose file is gone goes too, unless its plugin
    // is only off (it comes back with the plugin).
    let names_now: Vec<String> = files.iter().map(|(_, _, d)| d.name.clone()).collect();
    let loaded: Vec<String> = plugins.loaded().map(|b| b.name.clone()).collect();
    neo4j
        .run(
            neo4rs::query(
                "MATCH (p:Program) WHERE NOT p.name IN $names \
                   AND coalesce(p.managed_by, 'yaml') = 'yaml' \
                   AND (p.plugin IS NULL OR p.plugin IN $loaded) \
                 DETACH DELETE p",
            )
            .param("names", names_now)
            .param("loaded", loaded),
        )
        .await?;
    relink(neo4j).await?;
    reconcile(neo4j).await?;
    Ok(files.len())
}

// ── Saving (manage_program, REST) ────────────────────────────────────────

#[derive(Debug)]
pub enum SaveError {
    Invalid(String),
    NotFound(String),
    Storage(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Invalid(e) => write!(f, "Program rejected — nothing was saved: {e}"),
            SaveError::NotFound(e) | SaveError::Storage(e) => write!(f, "{e}"),
        }
    }
}

fn storage(e: impl std::fmt::Display) -> SaveError {
    SaveError::Storage(e.to_string())
}

/// What a save did.
#[derive(Debug, Serialize)]
pub struct Saved {
    pub program: ProgramDef,
    pub created: bool,
    pub managed_by: String,
    pub warnings: Vec<String>,
    /// Workflows paused or resumed because of it.
    pub changes: Vec<String>,
}

/// The brain acting on its own (a queued job, an agent run), not a person
/// in the HBI or in chat.
fn autonomous() -> bool {
    use agent_brain_llm::routing::{Purpose, ROUTE_CONTEXT};
    matches!(
        ROUTE_CONTEXT.try_with(|c| c.purpose),
        Ok(Purpose::Background | Purpose::Subagent)
    )
}

/// The status rules a person decides (PROGRAMS_PLAN §8): the brain may close
/// an investigation or case it answered, but it does not close standing work
/// and does not reopen a cold case; it proposes that as a todo.
fn check_status_change(
    kind: &str,
    from: Option<&str>,
    to: &str,
    autonomous: bool,
) -> Result<(), String> {
    if !autonomous || from == Some(to) {
        return Ok(());
    }
    if from == Some("closed") {
        return Err(
            "a closed program is reopened by the owner: propose it as a todo \
                    (manage_todo) with the reason, instead"
                .into(),
        );
    }
    if to == "closed" && !QUESTION_KINDS.contains(&kind) {
        return Err(format!(
            "the brain closes only an investigation or case it has answered; closing a {kind} \
             is the owner's decision: propose it as a todo"
        ));
    }
    Ok(())
}

/// Create or edit the program `body.name`: the body is laid over the stored
/// program (a key set to null goes back to its default), the result is
/// checked whole, and nothing is written on a hard error. An edit of anything
/// but `status`/`finding` makes a seeded program runtime-owned.
pub async fn save(
    neo4j: &Neo4jClient,
    body: &Value,
    create_only: bool,
) -> Result<Saved, SaveError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SaveError::Invalid("`name` is required".into()))?
        .to_string();
    let stored = load_all(neo4j).await.map_err(storage)?;
    let current = stored.iter().find(|s| s.def.name == name).cloned();
    if current.is_some() && create_only {
        return Err(SaveError::Invalid(format!(
            "a program named '{name}' already exists; pick another name or edit that one"
        )));
    }

    let mut cand = current
        .as_ref()
        .map(|s| serde_json::to_value(&s.def).unwrap_or_default())
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "action" | "managed_by") {
                continue;
            }
            if v.is_null() {
                if let Some(c) = cand.as_object_mut() {
                    c.remove(k);
                }
            } else {
                cand[k] = v.clone();
            }
        }
    }
    let def: ProgramDef =
        serde_json::from_value(cand).map_err(|e| SaveError::Invalid(e.to_string()))?;

    let mut inv = inventory(neo4j).await.map_err(storage)?;
    inv.programs.retain(|p| p.name != def.name);
    inv.programs.push(def.clone());
    let mut warnings = check(&def, &inv).map_err(SaveError::Invalid)?;
    check_status_change(
        &def.kind,
        current.as_ref().map(|c| c.def.status.as_str()),
        &def.status,
        autonomous(),
    )
    .map_err(SaveError::Invalid)?;

    let status_only = current.as_ref().is_some_and(|c| {
        let mut a = c.def.clone();
        a.status = def.status.clone();
        a.finding = def.finding.clone();
        a == def
    });
    let managed_by = match &current {
        Some(c) if status_only => c.managed_by.clone(),
        Some(c) => {
            if c.managed_by == "yaml" {
                warnings.push(format!(
                    "'{name}' is now runtime-owned: {} no longer applies to it. \
                     `reset` puts the file back.",
                    c.source.as_deref().unwrap_or("its file")
                ));
            }
            "runtime".to_string()
        }
        None => "runtime".to_string(),
    };
    write(
        neo4j,
        &def,
        &managed_by,
        current.as_ref().and_then(|c| c.plugin.as_deref()),
        current.as_ref().and_then(|c| c.source.as_deref()),
        true,
    )
    .await
    .map_err(storage)?;
    relink(neo4j).await.map_err(storage)?;
    let changes = reconcile(neo4j).await.map_err(storage)?;
    Ok(Saved {
        program: def,
        created: current.is_none(),
        managed_by,
        warnings,
        changes: changes.iter().map(Change::describe).collect(),
    })
}

/// Delete a program. Refused while programs sit under it. A seeded one
/// leaves a `(:RetiredProgram)` so the next start does not bring it back.
/// Its workflows are released, and those its pause held come back.
pub async fn delete(neo4j: &Neo4jClient, name: &str) -> Result<Vec<String>, SaveError> {
    let stored = load_all(neo4j).await.map_err(storage)?;
    let Some(cur) = stored.iter().find(|s| s.def.name == name) else {
        return Err(SaveError::NotFound(format!(
            "No program '{name}'. Programs: {}",
            stored
                .iter()
                .map(|s| s.def.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    let children: Vec<&str> = stored
        .iter()
        .filter(|s| s.def.parent.as_deref() == Some(name))
        .map(|s| s.def.name.as_str())
        .collect();
    if !children.is_empty() {
        return Err(SaveError::Invalid(format!(
            "programs sit under '{name}': {}. Move or delete them first.",
            children.join(", ")
        )));
    }
    if cur.source.is_some() {
        neo4j
            .run(
                neo4rs::query("MERGE (r:RetiredProgram {name: $n}) SET r.retired_at = datetime()")
                    .param("n", name.to_string()),
            )
            .await
            .map_err(storage)?;
    }
    neo4j
        .run(
            neo4rs::query("MATCH (p:Program {name: $n}) DETACH DELETE p")
                .param("n", name.to_string()),
        )
        .await
        .map_err(storage)?;
    let changes = reconcile(neo4j).await.map_err(storage)?;
    Ok(changes.iter().map(Change::describe).collect())
}

/// Put a seeded program's file back: its definition from the file, owned by
/// the file again. The status stays what it is.
pub async fn reset(
    neo4j: &Neo4jClient,
    plugins: &agent_brain_graph::bundles::Plugins,
    name: &str,
) -> Result<Saved, SaveError> {
    let Some((plugin, source, mut def)) = file_defs(plugins)
        .into_iter()
        .find(|(_, _, d)| d.name == name)
    else {
        return Err(SaveError::NotFound(format!(
            "no programs/*.yaml file declares '{name}'"
        )));
    };
    let stored = load_all(neo4j).await.map_err(storage)?;
    if let Some(s) = stored.iter().find(|s| s.def.name == name) {
        def.status = s.def.status.clone();
        def.finding = s.def.finding.clone();
    }
    let mut inv = inventory(neo4j).await.map_err(storage)?;
    let dropped = prune_unknown(&mut def, &inv);
    inv.programs.retain(|p| p.name != name);
    inv.programs.push(def.clone());
    let mut warnings = check(&def, &inv).map_err(SaveError::Invalid)?;
    if !dropped.is_empty() {
        warnings.push(format!(
            "left out (not on this brain): {}",
            dropped.join(", ")
        ));
    }
    neo4j
        .run(
            neo4rs::query("MATCH (r:RetiredProgram {name: $n}) DELETE r")
                .param("n", name.to_string()),
        )
        .await
        .map_err(storage)?;
    write(neo4j, &def, "yaml", Some(&plugin), Some(&source), false)
        .await
        .map_err(storage)?;
    relink(neo4j).await.map_err(storage)?;
    let changes = reconcile(neo4j).await.map_err(storage)?;
    Ok(Saved {
        program: def,
        created: false,
        managed_by: "yaml".into(),
        warnings,
        changes: changes.iter().map(Change::describe).collect(),
    })
}

// ── Reading ──────────────────────────────────────────────────────────────

/// The tree, as rows in depth-first order: each program with its depth, its
/// workflows' counts, and what is wrong with it. Plus the enabled workflows
/// in no program.
pub async fn tree(neo4j: &Neo4jClient) -> anyhow::Result<Value> {
    let stored = load_all(neo4j).await?;
    let states = workflow_states(neo4j).await?;
    let mut inv = inventory(neo4j).await?;
    inv.programs = stored.iter().map(|s| s.def.clone()).collect();
    let enabled: HashSet<&str> = states
        .iter()
        .filter(|w| w.enabled)
        .map(|w| w.name.as_str())
        .collect();
    let mut rows = Vec::new();
    fn walk<'a>(
        parent: Option<&str>,
        depth: usize,
        stored: &'a [Stored],
        out: &mut Vec<(usize, &'a Stored)>,
        seen: &mut HashSet<&'a str>,
    ) {
        for s in stored.iter().filter(|s| s.def.parent.as_deref() == parent) {
            if seen.insert(s.def.name.as_str()) {
                out.push((depth, s));
                walk(Some(&s.def.name), depth + 1, stored, out, seen);
            }
        }
    }
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    walk(None, 0, &stored, &mut ordered, &mut seen);
    // A program whose parent is missing still shows, at the top.
    for s in &stored {
        if !seen.contains(s.def.name.as_str()) {
            ordered.push((0, s));
        }
    }
    for (depth, s) in ordered {
        let issues = match check(&s.def, &inv) {
            Ok(soft) => soft,
            Err(hard) => vec![hard],
        };
        rows.push(json!({
            "name": s.def.name,
            "label": s.def.label(),
            "kind": s.def.kind,
            "status": s.def.status,
            "parent": s.def.parent,
            "depth": depth,
            "workflows": s.def.workflows.len(),
            "workflow_names": s.def.workflows,
            "enabled_workflows": s.def.workflows.iter().filter(|w| enabled.contains(w.as_str())).count(),
            "managed_by": s.managed_by,
            "issues": issues,
        }));
    }
    let claimed: HashSet<&str> = stored
        .iter()
        .flat_map(|s| s.def.workflows.iter().map(String::as_str))
        .collect();
    let mut unassigned: Vec<&str> = enabled
        .iter()
        .copied()
        .filter(|w| !claimed.contains(w))
        .collect();
    unassigned.sort();
    Ok(json!({
        "count": rows.len(),
        "programs": rows,
        "unassigned_enabled_workflows": unassigned,
    }))
}

/// One program in full: its definition, owner, the programs under it, its
/// workflows' state, and its open todos.
pub async fn detail(neo4j: &Neo4jClient, name: &str) -> anyhow::Result<Option<Value>> {
    let stored = load_all(neo4j).await?;
    let Some(s) = stored.iter().find(|s| s.def.name == name) else {
        return Ok(None);
    };
    let states = workflow_states(neo4j).await?;
    let workflows: Vec<Value> = s
        .def
        .workflows
        .iter()
        .map(|w| match states.iter().find(|x| &x.name == w) {
            Some(x) => json!({"name": w, "enabled": x.enabled, "paused_reason": x.paused_reason}),
            None => json!({"name": w, "missing": true}),
        })
        .collect();
    let children: Vec<&str> = stored
        .iter()
        .filter(|c| c.def.parent.as_deref() == Some(name))
        .map(|c| c.def.name.as_str())
        .collect();
    let todos = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Todo {program: $n}) WHERE t.status <> 'done' \
                 RETURN t.id AS id, t.title AS title, t.status AS status ORDER BY t.priority, t.created_at",
            )
            .param("n", name.to_string()),
        )
        .await?
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String>("id").unwrap_or_default(),
                "title": r.get::<String>("title").unwrap_or_default(),
                "status": r.get::<String>("status").unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let mut inv = inventory(neo4j).await?;
    inv.programs = stored.iter().map(|x| x.def.clone()).collect();
    let issues = match check(&s.def, &inv) {
        Ok(soft) => soft,
        Err(hard) => vec![hard],
    };
    let mut out = serde_json::to_value(s)?;
    out["label"] = json!(s.def.label());
    out["children"] = json!(children);
    out["workflow_state"] = json!(workflows);
    out["open_todos"] = json!(todos);
    out["issues"] = json!(issues);
    Ok(Some(out))
}

// ── Reviews (Phase 3) ────────────────────────────────────────────────────

/// The workflow that reviews one program (input `program`).
pub const REVIEW_WORKFLOW: &str = "program-review";

/// What the manager's review must say about a program, in one word.
pub const VERDICTS: &[&str] = &["healthy", "attention", "failing"];

/// At most this many proposals a review turns into todos.
pub const MAX_PROPOSALS: usize = 3;

/// What `program-manager` returns: a verdict, what it found, and proposals
/// the owner approves or not. It changes nothing itself.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ProgramReview {
    pub verdict: String,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub proposals: Vec<ReviewProposal>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReviewProposal {
    /// A todo title: the change, in a few words.
    pub title: String,
    /// The evidence, from the facts it was given.
    #[serde(default)]
    pub why: String,
    /// What the owner decides, in one or two sentences.
    #[serde(default)]
    pub ask: String,
    /// The workflow, agent or program it is about.
    #[serde(default)]
    pub target: Option<String>,
}

/// Parse the manager's answer. `Err` says what is missing, so the run fails
/// visibly instead of writing nothing.
pub fn parse_review(text: &str) -> Result<ProgramReview, String> {
    let v = agent_brain_llm::llm::first_json_object(text, &["verdict", "summary"]).ok_or_else(
        || {
            format!(
                "the review has no JSON object with `verdict` and `summary`; it began: {}",
                text.chars().take(300).collect::<String>()
            )
        },
    )?;
    let mut r: ProgramReview =
        serde_json::from_value(v).map_err(|e| format!("the review's JSON does not fit: {e}"))?;
    r.verdict = r.verdict.trim().to_lowercase();
    if !VERDICTS.contains(&r.verdict.as_str()) {
        return Err(format!(
            "verdict '{}' is not one of {}",
            r.verdict,
            VERDICTS.join(", ")
        ));
    }
    if r.summary.trim().is_empty() {
        return Err("the review's summary is empty".into());
    }
    r.proposals.retain(|p| !p.title.trim().is_empty());
    Ok(r)
}

/// The review as a note: what a person and the parent's next review read.
pub fn review_note(
    label: &str,
    name: &str,
    date: &str,
    r: &ProgramReview,
    filed: &[String],
) -> String {
    let mut out = format!(
        "# Program review: {label} ({date})\n\nProgram `{name}` · verdict: **{}**\n\n{}\n",
        r.verdict,
        r.summary.trim()
    );
    if !r.findings.is_empty() {
        out.push_str("\n## Findings\n\n");
        for f in &r.findings {
            out.push_str(&format!("- {}\n", f.trim()));
        }
    }
    if !r.proposals.is_empty() {
        out.push_str("\n## Proposals (todos for the owner)\n\n");
        for p in &r.proposals {
            out.push_str(&format!("- **{}**", p.title.trim()));
            if !p.why.trim().is_empty() {
                out.push_str(&format!(": {}", p.why.trim()));
            }
            out.push('\n');
        }
    }
    if !filed.is_empty() {
        out.push_str(&format!("\n{}\n", filed.join("\n")));
    }
    out
}

/// Stamp the program with its latest review, which its parent's review reads.
pub async fn mark_reviewed(
    neo4j: &Neo4jClient,
    name: &str,
    r: &ProgramReview,
) -> anyhow::Result<()> {
    neo4j
        .run(
            neo4rs::query(
                "MATCH (p:Program {name: $n}) SET p.last_review_at = datetime(), \
                 p.last_review_verdict = $v, p.last_review_summary = $s",
            )
            .param("n", name.to_string())
            .param("v", r.verdict.clone())
            .param("s", r.summary.chars().take(1500).collect::<String>()),
        )
        .await?;
    Ok(())
}

/// Everything the manager is given about one program besides the workflow
/// scorecard: its definition, its workflows' state, the programs under it
/// with their latest reviews, its own last review, its open todos (so it
/// does not propose what is already waiting) and its issues.
pub async fn review_context(neo4j: &Neo4jClient, name: &str) -> anyhow::Result<Option<Value>> {
    let Some(d) = detail(neo4j, name).await? else {
        return Ok(None);
    };
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (p:Program) RETURN p.name AS name, p.label AS label, p.status AS status, \
                    p.parent AS parent, toString(p.last_review_at) AS at, \
                    p.last_review_verdict AS verdict, p.last_review_summary AS summary",
        ))
        .await?;
    let review_of = |r: &neo4rs::Row| {
        json!({
            "name": r.get::<String>("name").unwrap_or_default(),
            "label": r.get::<String>("label").unwrap_or_default(),
            "status": r.get::<String>("status").unwrap_or_default(),
            "last_review_at": r.get::<String>("at").ok(),
            "verdict": r.get::<String>("verdict").ok(),
            "summary": r.get::<String>("summary").ok(),
        })
    };
    let children: Vec<Value> = rows
        .iter()
        .filter(|r| r.get::<String>("parent").ok().as_deref() == Some(name))
        .map(review_of)
        .collect();
    let own = rows
        .iter()
        .find(|r| r.get::<String>("name").ok().as_deref() == Some(name))
        .map(review_of);
    Ok(Some(json!({
        "program": {
            "name": d["name"], "label": d["label"], "kind": d["kind"], "status": d["status"],
            "parent": d["parent"], "charter": d["charter"], "success": d["success"],
            "topics": d["topics"], "agents": d["agents"], "apis": d["apis"],
            "keep_running": d["keep_running"], "budget": d["budget"], "review": d["review"],
        },
        "workflows": d["workflow_state"],
        "programs_under_it": children,
        "previous_review": own,
        "open_todos": d["open_todos"],
        "issues": d["issues"],
    })))
}

/// Whether a review that last started `last` (None: never) is due under a
/// review period of `every_secs`.
pub fn review_due(
    last: Option<chrono::DateTime<chrono::Utc>>,
    every_secs: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    last.is_none_or(|t| now - t >= chrono::Duration::seconds(every_secs))
}

/// Start a `program-review` run for every active program whose review is
/// due (one with `review.every`, not `services`), deepest programs first so
/// a parent's review reads fresh child reviews when the queue keeps order.
/// Returns `(program, task id)` for each run started.
pub async fn start_due_reviews(neo4j: &Neo4jClient) -> anyhow::Result<Vec<(String, String)>> {
    let stored = load_all(neo4j).await?;
    let started_rows = neo4j
        .execute(neo4rs::query(
            "MATCH (p:Program) RETURN p.name AS name, toString(p.review_started_at) AS at",
        ))
        .await?;
    let started: HashMap<String, chrono::DateTime<chrono::Utc>> = started_rows
        .iter()
        .filter_map(|r| {
            let at = r.get::<String>("at").ok()?;
            let t = chrono::DateTime::parse_from_rfc3339(&at).ok()?;
            Some((r.get::<String>("name").ok()?, t.with_timezone(&chrono::Utc)))
        })
        .collect();
    let by_name: HashMap<&str, &ProgramDef> = stored
        .iter()
        .map(|s| (s.def.name.as_str(), &s.def))
        .collect();
    let depth = |name: &str| {
        let mut d = 0;
        let mut cur = by_name.get(name).and_then(|p| p.parent.as_deref());
        while let Some(n) = cur {
            d += 1;
            if d > 32 {
                break;
            }
            cur = by_name.get(n).and_then(|p| p.parent.as_deref());
        }
        d
    };
    let now = chrono::Utc::now();
    let mut due: Vec<&ProgramDef> = stored
        .iter()
        .map(|s| &s.def)
        .filter(|p| p.status == "active" && p.kind != "services")
        .filter(|p| {
            p.review
                .as_ref()
                .and_then(|r| crate::workflow_seeder::parse_every(&r.every).ok())
                .is_some_and(|secs| review_due(started.get(&p.name).copied(), secs, now))
        })
        .collect();
    due.sort_by_key(|p| std::cmp::Reverse(depth(&p.name)));
    let mut out = Vec::new();
    for p in due {
        let mut inputs = serde_json::Map::new();
        inputs.insert("program".into(), json!(p.name));
        match crate::workflows::start_run(
            neo4j,
            Some(REVIEW_WORKFLOW),
            &inputs,
            Some(&format!("Review the program '{}'", p.label())),
            None,
            None,
        )
        .await
        {
            Ok(run) => {
                neo4j
                    .run(
                        neo4rs::query(
                            "MATCH (p:Program {name: $n}) SET p.review_started_at = datetime()",
                        )
                        .param("n", p.name.clone()),
                    )
                    .await?;
                out.push((p.name.clone(), run.task_id));
            }
            Err(e) => warn!(program = %p.name, error = %e, "programs: review not started"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, parent: Option<&str>, workflows: &[&str]) -> ProgramDef {
        ProgramDef {
            name: name.into(),
            label: String::new(),
            kind: "department".into(),
            parent: parent.map(Into::into),
            status: "active".into(),
            charter: String::new(),
            success: String::new(),
            topics: vec![],
            workflows: workflows.iter().map(|s| s.to_string()).collect(),
            keep_running: vec![],
            agents: vec![],
            apis: vec![],
            review: None,
            budget: None,
            finding: None,
        }
    }

    fn inv(programs: Vec<ProgramDef>) -> Inventory {
        Inventory {
            workflows: ["a", "b", "c"].iter().map(|s| s.to_string()).collect(),
            topics: ["nia"].iter().map(|s| s.to_string()).collect(),
            agents: ["researcher"].iter().map(|s| s.to_string()).collect(),
            apis: ["github"].iter().map(|s| s.to_string()).collect(),
            programs,
        }
    }

    #[test]
    fn a_good_program_passes_and_an_empty_one_is_flagged() {
        let nia = p("nia", None, &[]);
        let sec = p("nat-sec", Some("nia"), &["a"]);
        let i = inv(vec![nia.clone(), sec.clone()]);
        assert_eq!(check(&sec, &i), Ok(vec![]));
        // nia has a child, so no warning; a lone empty program gets one.
        assert_eq!(check(&nia, &i), Ok(vec![]));
        let lone = p("lone", None, &[]);
        assert_eq!(check(&lone, &i).unwrap().len(), 1);
    }

    #[test]
    fn a_bad_program_is_refused_with_what_fits() {
        let other = p("other", None, &["b"]);
        let mut bad = p("Bad Name", Some("ghost"), &["a", "b", "zzz"]);
        bad.kind = "team".into();
        bad.topics = vec!["sports".into()];
        bad.keep_running = vec!["c".into()];
        let e = check(&bad, &inv(vec![other])).unwrap_err();
        for want in [
            "must be a slug",
            "kind 'team'",
            "parent 'ghost' is not a program (programs: other)",
            "workflow 'b' belongs to program 'other'",
            "workflow 'zzz' does not exist",
            "workflows in no program: c",
            "keep_running names 'c'",
            "no topic named 'sports' (known: nia)",
        ] {
            assert!(e.contains(want), "missing {want:?} in: {e}");
        }
    }

    #[test]
    fn a_parent_loop_is_refused() {
        let a = p("a1", Some("b1"), &[]);
        let b = p("b1", Some("c1"), &[]);
        let c = p("c1", Some("a1"), &[]);
        let e = check(&a, &inv(vec![a.clone(), b, c])).unwrap_err();
        assert!(e.contains("loops: a1 → b1 → c1 → a1"), "{e}");
        let own = p("x", Some("x"), &[]);
        assert!(
            check(&own, &inv(vec![]))
                .unwrap_err()
                .contains("its own parent")
        );
    }

    #[test]
    fn closing_a_question_needs_a_finding() {
        let mut inq = p("inq", None, &["a"]);
        inq.kind = "investigation".into();
        inq.status = "closed".into();
        assert!(
            check(&inq, &inv(vec![]))
                .unwrap_err()
                .contains("needs a `finding`")
        );
        inq.finding = Some("It was the supplier.".into());
        assert!(check(&inq, &inv(vec![])).is_ok());
    }

    #[test]
    fn the_brain_may_close_an_answered_question_but_not_reopen_or_close_standing_work() {
        assert!(check_status_change("investigation", Some("active"), "closed", true).is_ok());
        assert!(check_status_change("department", Some("active"), "closed", true).is_err());
        assert!(check_status_change("case", Some("closed"), "active", true).is_err());
        // A person (HBI, chat) decides any of it.
        assert!(check_status_change("department", Some("closed"), "active", false).is_ok());
        assert!(check_status_change("department", Some("active"), "paused", true).is_ok());
    }

    fn wf(name: &str, enabled: bool, reason: Option<&str>) -> WorkflowState {
        WorkflowState {
            name: name.into(),
            enabled,
            paused_reason: reason.map(Into::into),
        }
    }

    #[test]
    fn a_pause_reaches_the_programs_below_and_lifts_only_its_own() {
        let mut nia = p("nia", None, &["a"]);
        nia.status = "paused".into();
        let mut sec = p("sec", Some("nia"), &["b", "c"]);
        sec.keep_running = vec!["c".into()];
        let workflows = [
            wf("a", true, None),
            wf("b", true, None),
            wf("c", true, None),
            wf("d", false, Some("PROGRAM_PAUSE: program 'gone' is paused")),
            wf("e", false, Some("quota spent")),
        ];
        let changes = plan_reconcile(&[nia.clone(), sec.clone()], &workflows);
        let reason = "PROGRAM_PAUSE: program 'nia' is paused".to_string();
        assert_eq!(
            changes,
            vec![
                Change::Pause {
                    workflow: "a".into(),
                    reason: reason.clone()
                },
                Change::Pause {
                    workflow: "b".into(),
                    reason: reason.clone()
                },
                // d's program no longer holds it; e was paused by a person.
                Change::Resume {
                    workflow: "d".into()
                },
            ]
        );

        // Resumed: what nia paused comes back, nothing else.
        nia.status = "active".into();
        let after = [
            wf("a", false, Some(&reason)),
            wf("b", false, Some(&reason)),
            wf("c", true, None),
            wf("e", false, Some("quota spent")),
        ];
        assert_eq!(
            plan_reconcile(&[nia, sec], &after),
            vec![
                Change::Resume {
                    workflow: "a".into()
                },
                Change::Resume {
                    workflow: "b".into()
                },
            ]
        );
    }

    #[test]
    fn a_seed_file_keeps_what_this_brain_has() {
        let mut d = p("x", None, &["a", "plugin-off-wf"]);
        d.topics = vec!["nia".into(), "gone-topic".into()];
        d.keep_running = vec!["plugin-off-wf".into()];
        let dropped = prune_unknown(&mut d, &inv(vec![]));
        assert_eq!(d.workflows, vec!["a"]);
        assert_eq!(d.topics, vec!["nia"]);
        assert!(d.keep_running.is_empty());
        assert_eq!(
            dropped,
            vec!["workflow 'plugin-off-wf'", "topic 'gone-topic'"]
        );
    }

    /// Every shipped program file loads and passes against what the repo's
    /// bundles declare (workflows, topics, agents by file stem).
    #[test]
    fn every_shipped_program_file_is_valid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let stems = |kind: &str| -> HashSet<String> {
            let mut out = HashSet::new();
            for bundle in std::fs::read_dir(&root).unwrap().flatten() {
                if let Ok(files) = std::fs::read_dir(bundle.path().join(kind)) {
                    for f in files.flatten() {
                        if let Some(s) = f.path().file_stem().and_then(|s| s.to_str()) {
                            out.insert(s.to_string());
                        }
                    }
                }
            }
            out
        };
        let mut defs = Vec::new();
        for f in std::fs::read_dir(root.join("core/programs"))
            .unwrap()
            .flatten()
        {
            defs.push(
                load_file(&f.path()).unwrap_or_else(|e| panic!("{}: {e}", f.path().display())),
            );
        }
        assert!(defs.len() >= 3, "only {} core programs", defs.len());
        let inv = Inventory {
            workflows: stems("workflows"),
            topics: stems("topics"),
            agents: stems("contexts"),
            apis: HashSet::new(),
            programs: defs.clone(),
        };
        for d in &defs {
            check(d, &inv).unwrap_or_else(|e| panic!("{}: {e}", d.name));
        }
    }

    #[test]
    fn a_review_parses_from_the_managers_answer() {
        let text = "Here is my review:\n```json\n{\"verdict\": \"Attention\", \"summary\": \"Reads are low.\", \
                    \"findings\": [\"69 notes, 2 reads\"], \"proposals\": [{\"title\": \"Pause x\", \"why\": \"nobody reads it\", \
                    \"ask\": \"Pause x?\", \"target\": \"x\"}, {\"title\": \"  \"}]}\n```";
        let r = parse_review(text).unwrap();
        assert_eq!(r.verdict, "attention");
        assert_eq!(r.proposals.len(), 1, "a proposal with no title is dropped");
        let note = review_note("NatSec", "nat-sec", "2026-10-17", &r, &[]);
        assert!(
            note.contains("verdict: **attention**")
                && note.contains("**Pause x**: nobody reads it")
        );
        assert!(
            parse_review("{\"verdict\": \"great\", \"summary\": \"x\"}")
                .unwrap_err()
                .contains("not one of")
        );
        assert!(
            parse_review("no json here")
                .unwrap_err()
                .contains("no JSON object")
        );
    }

    #[test]
    fn a_review_is_due_after_its_period() {
        let now = chrono::Utc::now();
        let week = 7 * 86_400;
        assert!(review_due(None, week, now));
        assert!(!review_due(
            Some(now - chrono::Duration::days(3)),
            week,
            now
        ));
        assert!(review_due(Some(now - chrono::Duration::days(7)), week, now));
    }

    #[test]
    fn unknown_keys_in_a_program_file_are_refused() {
        let e = serde_yaml::from_str::<ProgramDef>("name: x\nkind: department\nowner: me\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown field `owner`"), "{e}");
    }
}
