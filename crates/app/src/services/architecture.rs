//! The living architecture (HBI_REDESIGN_PLAN.md §4): what the brain is made
//! of, computed from what it already knows about itself, so it cannot drift.
//!
//! - **Inventory.** Skills and their tools come from `(:ToolDef)`, which is
//!   re-synced from the tool registry at every start. HTTP routes are parsed
//!   from the Axum route table in `mcp/http_transport.rs` (compiled in). Also
//!   agents, workflows, MCP servers, dynamic tools, providers, and the
//!   services in `components.yaml`.
//! - **Layout.** `architecture/layout.yaml` is the only hand-written part: the
//!   groups, the edges between them, and the container topology. Live items
//!   that no group claims come back as `unplaced`; selectors that match nothing
//!   come back as `stale`. Both are shown, so a gap is visible instead of
//!   silently dropped.
//! - **Snapshots.** `snapshot_architecture` (weekly on `brain-api-sync`, and
//!   once at the first start) stores an `(:ArchSnapshot)` with its diff against
//!   the previous one. The diff is set arithmetic, with no model in it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::repository::Neo4jClient;

// ── architecture/ (the core bundle's) ────────────────────────────────────────

pub fn architecture_dir() -> PathBuf {
    crate::services::plugins::Plugins::global().core_dir("architecture")
}

pub(crate) fn load_yaml<T: for<'de> Deserialize<'de> + Default>(file: &str) -> T {
    let p = architecture_dir().join(file);
    match std::fs::read_to_string(&p)
        .map_err(anyhow::Error::from)
        .and_then(|t| serde_yaml::from_str::<T>(&t).map_err(Into::into))
    {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %p.display(), error = %e, "architecture: file not loaded");
            T::default()
        }
    }
}

/// One service the brain depends on (`components.yaml`).
#[derive(Debug, Clone, Deserialize)]
pub struct Component {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub kind: String,
    /// `neo4j` | `duckdb` | `http`.
    pub probe: String,
    #[serde(default)]
    pub url_env: Option<String>,
    #[serde(default)]
    pub strip_suffix: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub purpose: String,
}

/// The shape of `components.yaml`.
#[derive(Debug, Default, Deserialize)]
pub struct ComponentsFile {
    #[serde(default)]
    pub components: Vec<Component>,
}

/// `components.yaml`, read once. Missing → no probes, and the health response
/// says the list is empty rather than claiming all is well.
pub fn components() -> &'static [Component] {
    static C: OnceLock<Vec<Component>> = OnceLock::new();
    C.get_or_init(|| load_yaml::<ComponentsFile>("components.yaml").components)
}

// ── Routes, parsed from the router source ────────────────────────────────────

const ROUTER_SOURCE: &str = include_str!("../mcp/http_transport.rs");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Route {
    pub path: String,
    pub method: String,
    /// The handler's module, e.g. `clients::search`.
    pub module: String,
}

impl Route {
    pub fn key(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// Every route the HTTP transport serves, read from the source this binary
/// was compiled from.
pub fn routes() -> &'static [Route] {
    static R: OnceLock<Vec<Route>> = OnceLock::new();
    R.get_or_init(|| parse_routes(ROUTER_SOURCE))
}

/// The `.route("/path", get(h).post(h2))` calls in `src`. Tests and
/// full-line comments are skipped.
pub fn parse_routes(src: &str) -> Vec<Route> {
    let code = src.split("#[cfg(test)]").next().unwrap_or(src);
    let code: String = code
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let aliases: HashMap<String, String> =
        Regex::new(r"use\s+crate::([A-Za-z0-9_:]+)\s+as\s+([A-Za-z0-9_]+)\s*;")
            .expect("alias regex")
            .captures_iter(&code)
            .map(|c| (c[2].to_string(), c[1].to_string()))
            .collect();
    let mods: HashSet<String> = Regex::new(r"(?m)^\s*(?:pub\s+)?mod\s+([A-Za-z0-9_]+)\s*;")
        .expect("mod regex")
        .captures_iter(&code)
        .map(|c| c[1].to_string())
        .collect();
    let method =
        Regex::new(r"(?:^|[^A-Za-z0-9_])(get|post|put|delete|patch)\(\s*([A-Za-z_][A-Za-z0-9_:]*)")
            .expect("method regex");

    let mut out = BTreeSet::new();
    let mut rest = code.as_str();
    while let Some(i) = rest.find(".route(") {
        let after = &rest[i + ".route(".len()..];
        let Some(len) = call_len(after) else { break };
        let span = &after[..len];
        rest = &after[len..];
        let Some((path, tail)) = first_string(span) else {
            continue;
        };
        for c in method.captures_iter(tail) {
            out.insert(Route {
                path: path.clone(),
                method: c[1].to_uppercase(),
                module: module_of(&c[2], &aliases, &mods),
            });
        }
    }
    out.into_iter().collect()
}

/// Length of a call's arguments, given the text just after its `(`.
fn call_len(s: &str) -> Option<usize> {
    let mut depth = 1;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if in_str {
            match (escaped, ch) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first string literal in `s`, and the text after it.
fn first_string(s: &str) -> Option<(String, &str)> {
    let start = s.find('"')? + 1;
    let end = start + s[start..].find('"')?;
    Some((s[start..end].to_string(), &s[end + 1..]))
}

fn module_of(handler: &str, aliases: &HashMap<String, String>, mods: &HashSet<String>) -> String {
    let segs: Vec<&str> = handler.split("::").collect();
    if segs.len() == 1 {
        return "mcp::http_transport".into();
    }
    let mut path: Vec<String> = segs[..segs.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if path[0] == "crate" {
        path.remove(0);
    } else if let Some(full) = aliases.get(&path[0]) {
        let mut p: Vec<String> = full.split("::").map(String::from).collect();
        p.extend(path.drain(1..));
        path = p;
    } else if mods.contains(&path[0]) {
        path.insert(0, "mcp".into());
    }
    path.join("::")
}

/// The area a route belongs to: `api/<first segment>` under `/api`, else
/// the first segment (`mcp`, `v1`, `todos`, …).
pub fn route_area(path: &str) -> String {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["api", x, ..] => format!("api/{x}"),
        [x, ..] => (*x).to_string(),
        [] => String::new(),
    }
}

// ── Inventory ────────────────────────────────────────────────────────────────

/// What the brain is made of, as sorted name sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    /// skill → its tools.
    #[serde(default)]
    pub skills: BTreeMap<String, BTreeSet<String>>,
    /// `"GET /api/x"`.
    #[serde(default)]
    pub routes: BTreeSet<String>,
    #[serde(default)]
    pub agents: BTreeSet<String>,
    #[serde(default)]
    pub workflows: BTreeSet<String>,
    #[serde(default)]
    pub mcp_servers: BTreeSet<String>,
    #[serde(default)]
    pub dynamic_tools: BTreeSet<String>,
    #[serde(default)]
    pub providers: BTreeSet<String>,
    #[serde(default)]
    pub components: BTreeSet<String>,
}

/// The categories a diff and the counts are reported in, in display order.
pub const CATEGORIES: [&str; 9] = [
    "skills",
    "tools",
    "routes",
    "agents",
    "workflows",
    "mcp_servers",
    "dynamic_tools",
    "providers",
    "components",
];

impl Inventory {
    pub fn category(&self, name: &str) -> BTreeSet<String> {
        match name {
            "skills" => self.skills.keys().cloned().collect(),
            "tools" => self.skills.values().flatten().cloned().collect(),
            "routes" => self.routes.clone(),
            "agents" => self.agents.clone(),
            "workflows" => self.workflows.clone(),
            "mcp_servers" => self.mcp_servers.clone(),
            "dynamic_tools" => self.dynamic_tools.clone(),
            "providers" => self.providers.clone(),
            "components" => self.components.clone(),
            _ => BTreeSet::new(),
        }
    }

    pub fn counts(&self) -> BTreeMap<String, usize> {
        CATEGORIES
            .iter()
            .map(|c| (c.to_string(), self.category(c).len()))
            .collect()
    }
}

/// Provider names from the live provider registry.
fn provider_names(providers: &Value) -> BTreeSet<String> {
    providers
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn names(neo4j: &Neo4jClient, cypher: &str) -> Result<BTreeSet<String>> {
    Ok(neo4j
        .execute(neo4rs::query(cypher))
        .await?
        .iter()
        .filter_map(|r| r.get::<String>("name").ok())
        .filter(|n| !n.is_empty())
        .collect())
}

/// The live inventory.
pub async fn collect(neo4j: &Neo4jClient) -> Result<Inventory> {
    let mut inv = Inventory::default();
    for r in neo4j
        .execute(neo4rs::query(
            "MATCH (d:ToolDef) RETURN coalesce(d.skill, '') AS skill, d.name AS name",
        ))
        .await?
    {
        if let (Ok(skill), Ok(name)) = (r.get::<String>("skill"), r.get::<String>("name")) {
            inv.skills.entry(skill).or_default().insert(name);
        }
    }
    inv.agents = names(neo4j, "MATCH (c:ContextProfile) RETURN c.name AS name").await?;
    inv.workflows = names(neo4j, "MATCH (w:Workflow) RETURN w.name AS name").await?;
    inv.mcp_servers = names(neo4j, "MATCH (m:McpServer) RETURN m.name AS name").await?;
    inv.dynamic_tools = names(neo4j, "MATCH (d:DynamicTool) RETURN d.name AS name").await?;
    inv.routes = routes().iter().map(Route::key).collect();
    inv.providers = provider_names(&crate::services::provider_registry::snapshot_json());
    inv.components = components().iter().map(|c| c.name.clone()).collect();
    Ok(inv)
}

// ── Diff ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Category → what was added and removed. Only categories that changed.
pub type Diff = BTreeMap<String, Change>;

pub fn diff(old: &Inventory, new: &Inventory) -> Diff {
    CATEGORIES
        .iter()
        .filter_map(|c| {
            let (a, b) = (old.category(c), new.category(c));
            let ch = Change {
                added: b.difference(&a).cloned().collect(),
                removed: a.difference(&b).cloned().collect(),
            };
            (!ch.added.is_empty() || !ch.removed.is_empty()).then(|| (c.to_string(), ch))
        })
        .collect()
}

fn category_label(c: &str) -> String {
    match c {
        "mcp_servers" => "MCP servers".into(),
        other => other.replace('_', " "),
    }
}

fn preview(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// One line a person can read: `tools: +2 (a, b); workflows: -1 (x)`.
pub fn summarize(d: &Diff) -> String {
    if d.is_empty() {
        return "No changes.".into();
    }
    CATEGORIES
        .iter()
        .filter_map(|cat| d.get(*cat).map(|ch| (cat, ch)))
        .map(|(cat, ch)| {
            let mut parts = Vec::new();
            if !ch.added.is_empty() {
                parts.push(format!("+{} ({})", ch.added.len(), preview(&ch.added)));
            }
            if !ch.removed.is_empty() {
                parts.push(format!("-{} ({})", ch.removed.len(), preview(&ch.removed)));
            }
            format!("{}: {}", category_label(cat), parts.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn counts_line(counts: &BTreeMap<String, usize>) -> String {
    CATEGORIES
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                counts.get(*c).copied().unwrap_or(0),
                category_label(c)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Snapshots ────────────────────────────────────────────────────────────────

/// Snapshots kept (two years of weekly runs).
const KEEP: i64 = 104;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub taken_at: String,
    /// `schedule` | `manual` | `first`.
    pub trigger: String,
    /// The commit the brain was running.
    pub version: Option<String>,
    pub counts: BTreeMap<String, usize>,
    /// Against `previous_id`; empty for the first snapshot.
    pub diff: Diff,
    pub summary: String,
    pub previous_id: Option<String>,
}

const SNAPSHOT_FIELDS: &str = "a.id AS id, toString(a.taken_at) AS taken_at, a.trigger AS trigger, \
     a.version AS version, a.counts AS counts, a.diff AS diff, a.summary AS summary, \
     a.previous_id AS previous_id";

fn json_field<T: for<'de> Deserialize<'de> + Default>(r: &neo4rs::Row, k: &str) -> T {
    r.get::<String>(k)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn snapshot_of(r: &neo4rs::Row) -> Snapshot {
    Snapshot {
        id: r.get("id").unwrap_or_default(),
        taken_at: r.get("taken_at").unwrap_or_default(),
        trigger: r.get("trigger").unwrap_or_default(),
        version: r.get::<String>("version").ok().filter(|s| !s.is_empty()),
        counts: json_field(r, "counts"),
        diff: json_field(r, "diff"),
        summary: r.get("summary").unwrap_or_default(),
        previous_id: r
            .get::<String>("previous_id")
            .ok()
            .filter(|s| !s.is_empty()),
    }
}

/// The newest snapshot and the inventory it recorded.
pub async fn latest(neo4j: &Neo4jClient) -> Result<Option<(Snapshot, Inventory)>> {
    let rows = neo4j
        .execute(neo4rs::query(&format!(
            "MATCH (a:ArchSnapshot) RETURN {SNAPSHOT_FIELDS}, a.inventory AS inventory \
             ORDER BY a.taken_at DESC LIMIT 1"
        )))
        .await?;
    Ok(rows
        .first()
        .map(|r| (snapshot_of(r), json_field(r, "inventory"))))
}

/// The newest `limit` snapshots, newest first (without inventories).
pub async fn history(neo4j: &Neo4jClient, limit: i64) -> Result<Vec<Snapshot>> {
    Ok(neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (a:ArchSnapshot) RETURN {SNAPSHOT_FIELDS} ORDER BY a.taken_at DESC LIMIT $limit"
            ))
            .param("limit", limit),
        )
        .await?
        .iter()
        .map(snapshot_of)
        .collect())
}

/// Record the live inventory and its diff against the previous snapshot.
pub async fn take(neo4j: &Neo4jClient, trigger: &str) -> Result<Snapshot> {
    let inv = collect(neo4j).await?;
    let prev = latest(neo4j).await?;
    let counts = inv.counts();
    let (d, summary) = match &prev {
        Some((_, old)) => {
            let d = diff(old, &inv);
            let s = summarize(&d);
            (d, s)
        }
        None => (
            Diff::new(),
            format!("First snapshot: {}.", counts_line(&counts)),
        ),
    };
    let version = neo4j
        .execute(neo4rs::query(
            "MATCH (v:BrainVersion {id: 'current'}) RETURN v.sha AS sha",
        ))
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get::<String>("sha").ok()));
    let id = uuid::Uuid::new_v4().to_string();
    let previous_id = prev.as_ref().map(|(s, _)| s.id.clone());
    let rows = neo4j
        .execute(
            neo4rs::query(
                "CREATE (a:ArchSnapshot {id: $id, taken_at: datetime(), trigger: $trigger, \
                 version: $version, inventory: $inventory, counts: $counts, diff: $diff, \
                 summary: $summary, previous_id: $previous_id}) \
                 RETURN toString(a.taken_at) AS taken_at",
            )
            .param("id", id.as_str())
            .param("trigger", trigger)
            .param("version", version.clone().unwrap_or_default())
            .param("inventory", serde_json::to_string(&inv)?)
            .param("counts", serde_json::to_string(&counts)?)
            .param("diff", serde_json::to_string(&d)?)
            .param("summary", summary.as_str())
            .param("previous_id", previous_id.clone().unwrap_or_default()),
        )
        .await?;
    let taken_at = rows
        .first()
        .and_then(|r| r.get::<String>("taken_at").ok())
        .unwrap_or_default();
    if let Err(e) = neo4j
        .run(
            neo4rs::query(
                "MATCH (a:ArchSnapshot) WITH a ORDER BY a.taken_at DESC SKIP $keep DETACH DELETE a",
            )
            .param("keep", KEEP),
        )
        .await
    {
        warn!(error = %e, "architecture: snapshot retention failed");
    }
    Ok(Snapshot {
        id,
        taken_at,
        trigger: trigger.into(),
        version,
        counts,
        diff: d,
        summary,
        previous_id,
    })
}

/// Take the first snapshot if there is none, so "what changed" has a baseline
/// from the first start instead of after the first weekly run.
pub async fn ensure_baseline(neo4j: &Neo4jClient) -> Result<Option<Snapshot>> {
    if latest(neo4j).await?.is_some() {
        return Ok(None);
    }
    take(neo4j, "first").await.map(Some)
}

// ── Layout ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub topology: Topology,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub column: u8,
    #[serde(default)]
    pub blurb: String,
    /// Selectors: `skill:Query`, `routes:api/*`, `component:neo4j`,
    /// `provider:*`, `mcp:*`, `collection:workflows`. A trailing `*` is a
    /// prefix match; an exact selector beats a wildcard.
    #[serde(default)]
    pub members: Vec<String>,
    /// Nodes that exist only in the layout (clients, say).
    #[serde(default)]
    pub nodes: Vec<StaticNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticNode {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Topology {
    #[serde(default)]
    pub networks: Vec<Network>,
    #[serde(default)]
    pub containers: Vec<Container>,
    #[serde(default)]
    pub host: Vec<HostProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub external: bool,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub name: String,
    #[serde(default)]
    pub networks: Vec<String>,
    /// Published `host:container` ports. None means unreachable from the host.
    #[serde(default)]
    pub ports: Vec<String>,
    /// The `components.yaml` entry it runs, if any.
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProcess {
    pub name: String,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub note: String,
}

/// `layout.yaml`, read once.
pub fn layout() -> &'static Layout {
    static L: OnceLock<Layout> = OnceLock::new();
    L.get_or_init(|| load_yaml::<Layout>("layout.yaml"))
}

/// One box in the diagram.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    /// `kind:name`, what a layout selector matches.
    pub id: String,
    pub kind: String,
    pub name: String,
    pub label: String,
    pub count: usize,
    /// The rows behind `count`: tools, routes, schedule names, …
    pub items: Vec<String>,
    pub detail: String,
    pub meta: Value,
}

fn node(
    kind: &str,
    name: &str,
    label: &str,
    items: Vec<String>,
    detail: String,
    meta: Value,
) -> Node {
    Node {
        id: format!("{kind}:{name}"),
        kind: kind.into(),
        name: name.into(),
        label: label.into(),
        count: items.len(),
        items,
        detail,
        meta,
    }
}

/// Every derived node: one per skill, route area, component, provider and MCP
/// server, plus one per collection (agents, workflows, dynamic tools).
pub fn build_nodes(inv: &Inventory, providers: &Value, comps: &[Component]) -> Vec<Node> {
    let mut out = Vec::new();
    for (skill, tools) in &inv.skills {
        out.push(node(
            "skill",
            skill,
            skill,
            tools.iter().cloned().collect(),
            String::new(),
            json!({}),
        ));
    }
    let mut areas: BTreeMap<String, (Vec<String>, BTreeSet<String>)> = BTreeMap::new();
    for r in routes() {
        let a = areas.entry(route_area(&r.path)).or_default();
        a.0.push(r.key());
        a.1.insert(r.module.clone());
    }
    for (area, (keys, modules)) in areas {
        let detail = modules.into_iter().collect::<Vec<_>>().join(", ");
        out.push(node(
            "routes",
            &area,
            &format!("/{area}"),
            keys,
            detail,
            json!({}),
        ));
    }
    for c in comps {
        out.push(node(
            "component",
            &c.name,
            &c.label,
            Vec::new(),
            c.purpose.clone(),
            json!({ "required": c.required, "kind": c.kind }),
        ));
    }
    for p in providers.as_array().into_iter().flatten() {
        let Some(name) = p["name"].as_str() else {
            continue;
        };
        let detail = [p["billing"].as_str(), p["data_policy"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        out.push(node(
            "provider",
            name,
            name,
            Vec::new(),
            detail,
            json!({ "kind": p["kind"], "enabled": p["enabled"], "key_configured": p["key_configured"] }),
        ));
    }
    for m in &inv.mcp_servers {
        out.push(node("mcp", m, m, Vec::new(), String::new(), json!({})));
    }
    for (name, label, set) in [
        ("agents", "Agents", &inv.agents),
        ("workflows", "Workflows", &inv.workflows),
        ("dynamic_tools", "Dynamic tools", &inv.dynamic_tools),
    ] {
        out.push(node(
            "collection",
            name,
            label,
            set.iter().cloned().collect(),
            String::new(),
            json!({}),
        ));
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacedGroup {
    pub id: String,
    pub label: String,
    pub column: u8,
    pub blurb: String,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Placement {
    pub groups: Vec<PlacedGroup>,
    /// Live nodes no group claims.
    pub unplaced: Vec<Node>,
    /// `group: selector` pairs that matched nothing.
    pub stale: Vec<String>,
}

fn selector_matches(sel: &str, id: &str) -> bool {
    match sel.strip_suffix('*') {
        Some(prefix) => id.starts_with(prefix),
        None => sel == id,
    }
}

/// Put each node in a group: an exact selector first, else the longest
/// matching wildcard. Within a group, nodes keep their selectors' order.
pub fn place(layout: &Layout, nodes: Vec<Node>) -> Placement {
    let mut groups: Vec<PlacedGroup> = layout
        .groups
        .iter()
        .map(|g| PlacedGroup {
            id: g.id.clone(),
            label: g.label.clone(),
            column: g.column,
            blurb: g.blurb.clone(),
            nodes: g
                .nodes
                .iter()
                .map(|s| {
                    node(
                        "static",
                        &s.name,
                        &s.label,
                        Vec::new(),
                        s.detail.clone(),
                        json!({}),
                    )
                })
                .collect(),
        })
        .collect();
    let mut used: HashSet<(usize, usize)> = HashSet::new();
    let mut slots: Vec<Vec<(usize, Node)>> = vec![Vec::new(); groups.len()];
    let mut unplaced = Vec::new();
    for n in nodes {
        let mut best: Option<(usize, usize, usize)> = None; // (rank, group, selector)
        for (gi, g) in layout.groups.iter().enumerate() {
            for (si, sel) in g.members.iter().enumerate() {
                if !selector_matches(sel, &n.id) {
                    continue;
                }
                // Exact beats any wildcard; a longer wildcard beats a shorter one.
                let rank = if sel.ends_with('*') {
                    usize::MAX - sel.len()
                } else {
                    0
                };
                if best.is_none_or(|(r, _, _)| rank < r) {
                    best = Some((rank, gi, si));
                }
            }
        }
        match best {
            Some((_, gi, si)) => {
                used.insert((gi, si));
                slots[gi].push((si, n));
            }
            None => unplaced.push(n),
        }
    }
    for (gi, mut s) in slots.into_iter().enumerate() {
        s.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(&b.1.label)));
        groups[gi].nodes.extend(s.into_iter().map(|(_, n)| n));
    }
    let stale = layout
        .groups
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| {
            let used = &used;
            g.members
                .iter()
                .enumerate()
                .filter(move |(si, _)| !used.contains(&(gi, *si)))
                .map(move |(_, sel)| format!("{}: {sel}", g.id))
        })
        .collect();
    Placement {
        groups,
        unplaced,
        stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
use crate::clients::rest as rest_handlers;
mod openai_compat;
fn router() {
    Router::new()
        .route("/mcp", post(handle_post_mcp))
        // .route("/commented", get(nope))
        .route(
            "/todos/{id}",
            get(rest_handlers::handle_get_todo)
                .put(rest_handlers::handle_update_todo)
                .delete(rest_handlers::handle_delete_todo),
        )
        .route("/v1/models", get(openai_compat::handle_list_models))
        .route("/api/search", get(crate::clients::search::handle_search))
}
#[cfg(test)]
mod tests { fn t() { Router::new().route("/test-only", get(h)); } }
"#;

    #[test]
    fn routes_are_read_from_the_router_with_their_modules() {
        let r = parse_routes(SRC);
        let keys: Vec<(String, String)> = r.iter().map(|r| (r.key(), r.module.clone())).collect();
        assert_eq!(
            keys,
            vec![
                ("GET /api/search".into(), "clients::search".into()),
                ("POST /mcp".into(), "mcp::http_transport".into()),
                ("DELETE /todos/{id}".into(), "clients::rest".into()),
                ("GET /todos/{id}".into(), "clients::rest".into()),
                ("PUT /todos/{id}".into(), "clients::rest".into()),
                ("GET /v1/models".into(), "mcp::openai_compat".into()),
            ]
        );
    }

    #[test]
    fn the_real_router_parses() {
        let r = routes();
        assert!(r.len() >= 60, "only {} routes parsed", r.len());
        let has = |k: &str| r.iter().any(|x| x.key() == k);
        assert!(
            has("POST /mcp") && has("GET /api/system/health") && has("POST /v1/chat/completions")
        );
        assert!(r.iter().all(|x| x.path.starts_with('/')));
    }

    #[test]
    fn areas_group_api_routes_by_their_first_segment() {
        assert_eq!(route_area("/api/topics/{name}"), "api/topics");
        assert_eq!(route_area("/mcp"), "mcp");
        assert_eq!(route_area("/v1/chat/completions"), "v1");
        assert_eq!(route_area("/todos/{id}"), "todos");
    }

    fn inv(tools: &[(&str, &str)], schedules: &[&str]) -> Inventory {
        let mut i = Inventory::default();
        for (s, t) in tools {
            i.skills
                .entry(s.to_string())
                .or_default()
                .insert(t.to_string());
        }
        i.workflows = schedules.iter().map(|s| s.to_string()).collect();
        i
    }

    #[test]
    fn a_diff_is_added_and_removed_per_category() {
        let old = inv(&[("Query", "a"), ("Query", "b")], &["daily"]);
        let new = inv(
            &[("Query", "a"), ("Query", "c"), ("git", "d")],
            &["daily", "weekly"],
        );
        let d = diff(&old, &new);
        assert_eq!(
            d["tools"],
            Change {
                added: vec!["c".into(), "d".into()],
                removed: vec!["b".into()]
            }
        );
        assert_eq!(d["skills"].added, vec!["git".to_string()]);
        assert_eq!(d["workflows"].added, vec!["weekly".to_string()]);
        assert!(!d.contains_key("routes"));
        assert_eq!(
            summarize(&d),
            "skills: +1 (git); tools: +2 (c, d), -1 (b); workflows: +1 (weekly)"
        );
        assert_eq!(summarize(&diff(&new, &new)), "No changes.");
    }

    #[test]
    fn an_inventory_round_trips_through_json() {
        let i = inv(&[("Query", "a")], &["daily"]);
        let back: Inventory = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(back, i);
        // A snapshot written before a category existed still reads.
        let old: Inventory = serde_json::from_str(r#"{"routes":["GET /x"]}"#).unwrap();
        assert_eq!(old.category("routes").len(), 1);
    }

    fn layout(yaml: &str) -> Layout {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn n(id: &str) -> Node {
        let (kind, name) = id.split_once(':').unwrap();
        node(kind, name, name, Vec::new(), String::new(), json!({}))
    }

    #[test]
    fn exact_selectors_beat_wildcards_and_leftovers_are_unplaced() {
        let l = layout(
            r#"
groups:
  - id: rest
    label: REST
    members: ["routes:api/*"]
  - id: system
    label: System
    members: ["routes:api/system", "skill:Gone"]
"#,
        );
        let p = place(
            &l,
            vec![
                n("routes:api/system"),
                n("routes:api/notes"),
                n("routes:mcp"),
            ],
        );
        let ids = |g: &PlacedGroup| g.nodes.iter().map(|x| x.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&p.groups[0]), vec!["routes:api/notes"]);
        assert_eq!(ids(&p.groups[1]), vec!["routes:api/system"]);
        assert_eq!(
            p.unplaced.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["routes:mcp"]
        );
        assert_eq!(p.stale, vec!["system: skill:Gone"]);
    }

    fn shipped_layout() -> Layout {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/core/architecture/layout.yaml"
        );
        serde_yaml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn the_shipped_layout_places_every_route_and_its_edges_name_groups() {
        let l = shipped_layout();
        let ids: HashSet<&str> = l.groups.iter().map(|g| g.id.as_str()).collect();
        for e in &l.edges {
            assert!(
                ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()),
                "edge {e:?}"
            );
        }
        let mut seen = HashSet::new();
        let nodes: Vec<Node> = routes()
            .iter()
            .map(|r| route_area(&r.path))
            .filter(|a| seen.insert(a.clone()))
            .map(|a| n(&format!("routes:{a}")))
            .collect();
        let p = place(&l, nodes);
        assert!(
            p.unplaced.is_empty(),
            "unplaced routes: {:?}",
            p.unplaced.iter().map(|x| &x.id).collect::<Vec<_>>()
        );
    }

    /// The topology in layout.yaml is hand-written, so it is checked against
    /// docker-compose.yml: same containers, networks and published ports.
    #[test]
    fn the_layout_topology_matches_docker_compose() {
        let l = shipped_layout();
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker-compose.yml");
        let compose: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let strs = |v: &serde_yaml::Value| -> Vec<String> {
            let mut out: Vec<String> = match v {
                serde_yaml::Value::Sequence(s) => s
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                serde_yaml::Value::Mapping(m) => m
                    .keys()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                _ => Vec::new(),
            };
            out.sort();
            out
        };
        let services = compose["services"].as_mapping().unwrap();
        let mut want: Vec<String> = services
            .keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect();
        want.sort();
        let mut have: Vec<String> = l
            .topology
            .containers
            .iter()
            .map(|c| c.name.clone())
            .collect();
        have.sort();
        assert_eq!(have, want, "layout.yaml containers vs compose services");
        for c in &l.topology.containers {
            let s = &compose["services"][c.name.as_str()];
            let mut nets = c.networks.clone();
            nets.sort();
            assert_eq!(nets, strs(&s["networks"]), "networks of {}", c.name);
            let mut ports = c.ports.clone();
            ports.sort();
            assert_eq!(ports, strs(&s["ports"]), "ports of {}", c.name);
        }
        let mut nets: Vec<(String, bool, bool)> = l
            .topology
            .networks
            .iter()
            .map(|n| (n.name.clone(), n.internal, n.external))
            .collect();
        nets.sort();
        let mut want_nets: Vec<(String, bool, bool)> = compose["networks"]
            .as_mapping()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().unwrap().to_string(),
                    v["internal"].as_bool().unwrap_or(false),
                    v["external"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        want_nets.sort();
        assert_eq!(nets, want_nets, "layout.yaml networks vs compose");
        let comps: HashSet<String> = load_components_for_test();
        for c in l
            .topology
            .containers
            .iter()
            .filter_map(|c| c.component.as_ref())
            .chain(l.topology.host.iter().filter_map(|h| h.component.as_ref()))
        {
            assert!(comps.contains(c), "topology names unknown component {c}");
        }
    }

    fn load_components_for_test() -> HashSet<String> {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/core/architecture/components.yaml"
        );
        let f: ComponentsFile = serde_yaml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        f.components.into_iter().map(|c| c.name).collect()
    }
}
