//! Plugin bundles (CORE_AND_PLUGINS_PLAN Phase 3a).
//!
//! A plugin is a folder under `PLUGINS_DIR` (default `./plugins`) with a
//! `plugin.yaml` and any of the seed dirs in [`MERGED_KINDS`]. The core ships
//! as the `core` bundle and is always enabled; others are named in
//! `PLUGINS_ENABLED` (comma-separated). This module finds the enabled bundles
//! and hands each kind's directories to its reader, and it seeds the three
//! ON-CREATE kinds itself (source lists, media sources, MCP servers).
//!
//! A bundle whose manifest is missing or wrong, or whose `requires` are not
//! met, is skipped with the reason; `GET /api/plugins` shows it.
//!
//! Finding and loading bundles is `agent_brain_graph::bundles`, re-exported
//! here; this module wires the compiled plugins and seeds what bundles ship.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use agent_brain_plugin_api::{Plugin, PluginContext};

use serde::Deserialize;
use tracing::{info, warn};

use agent_brain_repository::Neo4jClient;

pub use agent_brain_graph::bundles::*;

// ── In-process plugins (Phase 3c) ──────────────────────────────────────────

/// The plugin crates compiled into this binary. Each is instantiated only
/// when its bundle loaded.
pub fn compiled() -> Vec<Arc<dyn Plugin>> {
    vec![
        Arc::new(agent_brain_plugin_osint_data::OsintData),
        Arc::new(agent_brain_plugin_media::Media),
        Arc::new(agent_brain_plugin_alias_judge::AliasJudge),
        Arc::new(agent_brain_plugin_reference::Reference),
        Arc::new(agent_brain_plugin_developer::Developer),
        Arc::new(agent_brain_plugin_epistemics::Epistemics),
        Arc::new(agent_brain_plugin_sandbox::Sandbox),
    ]
}

/// A plugin whose tools were accepted, with its context, for `start`.
pub type Accepted = (Arc<dyn Plugin>, PluginContext);

fn router_cell() -> &'static RwLock<HashMap<String, agent_brain_plugin_api::axum::Router>> {
    static C: OnceLock<RwLock<HashMap<String, agent_brain_plugin_api::axum::Router>>> =
        OnceLock::new();
    C.get_or_init(|| RwLock::new(HashMap::new()))
}

/// An accepted plugin's HTTP routes (`Plugin::routes`), served by the host
/// under `/api/plugins/<name>/`. `None` = not loaded, or no routes.
pub fn plugin_router(name: &str) -> Option<agent_brain_plugin_api::axum::Router> {
    router_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

static BACKGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Let plugins start background loops (`PluginContext::background_allowed`).
/// Only the deployed brain (the HTTP server) sets it, before the brain is
/// built: a stdio session alongside it must not run them too.
pub fn set_background_allowed(allowed: bool) {
    BACKGROUND.store(allowed, std::sync::atomic::Ordering::Relaxed);
}

pub fn background_allowed() -> bool {
    BACKGROUND.load(std::sync::atomic::Ordering::Relaxed)
}

/// Run every accepted plugin's `start` hook (schema, seeds). A failure is
/// logged and recorded on the plugin's code status; its tools stay.
pub async fn start_plugins(accepted: &[Accepted]) {
    for (plugin, ctx) in accepted {
        if let Err(e) = plugin.start(ctx).await {
            let error = format!("start failed: {e}");
            tracing::error!(plugin = %plugin.name(), "{error}");
            record_code_error(plugin.name(), error);
        }
    }
}

/// The skills of every compiled plugin whose bundle loaded. `taken` holds the
/// tool names already registered (the core's); a plugin that would claim one
/// of them, or one an earlier plugin claimed, is refused whole and the reason
/// recorded. A loaded bundle with a `crate/` this binary did not compile is
/// reported too. Records the result for [`code_status`].
pub fn plugin_skills(
    plugins: &Plugins,
    compiled: &[Arc<dyn Plugin>],
    taken: &[String],
    env: agent_brain_plugin_api::EnvFn,
    host: &crate::plugin_host::Host,
) -> (Vec<Arc<dyn agent_brain_plugin_api::Skill>>, Vec<Accepted>) {
    let mut accepted: Vec<Accepted> = Vec::new();
    let mut retrieval: Vec<&'static str> = Vec::new();
    let mut routers: HashMap<String, agent_brain_plugin_api::axum::Router> = HashMap::new();
    let mut claimed: Vec<String> = taken.to_vec();
    let mut status: HashMap<String, CodeStatus> = HashMap::new();
    let mut out: Vec<Arc<dyn agent_brain_plugin_api::Skill>> = Vec::new();
    for b in plugins.loaded() {
        let Some(plugin) = compiled.iter().find(|p| p.name() == b.name) else {
            if b.root.join("crate").is_dir() {
                let error =
                    "the bundle has a crate/ that this binary was not built with".to_string();
                warn!(plugin = %b.name, "{error}");
                status.insert(
                    b.name.clone(),
                    CodeStatus {
                        tools: vec![],
                        error: Some(error),
                    },
                );
            }
            continue;
        };
        let ctx = host.context(&b.name, &b.root, Arc::clone(&env));
        let skills = plugin.tools(&ctx);
        let names: Vec<String> = skills
            .iter()
            .flat_map(|s| s.tools().into_iter().map(|t| t.name))
            .collect();
        let clash: Vec<&String> = names
            .iter()
            .enumerate()
            .filter(|(i, n)| claimed.contains(n) || names[..*i].contains(n))
            .map(|(_, n)| n)
            .collect();
        if !clash.is_empty() {
            let error = format!(
                "tool name already taken: {}; the plugin's tools are left out",
                clash
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            tracing::error!(plugin = %b.name, "{error}");
            status.insert(
                b.name.clone(),
                CodeStatus {
                    tools: vec![],
                    error: Some(error),
                },
            );
            continue;
        }
        info!(plugin = %b.name, tools = %names.join(", "), "Plugin tools registered");
        claimed.extend(names.iter().cloned());
        status.insert(
            b.name.clone(),
            CodeStatus {
                tools: names,
                error: None,
            },
        );
        out.extend(skills);
        retrieval.extend(plugin.retrieval_tools());
        if let Some(r) = plugin.routes(&ctx) {
            routers.insert(b.name.clone(), r);
        }
        accepted.push((Arc::clone(plugin), ctx));
    }
    set_code_status(status);
    agent_brain_work::tools::set_plugin_retrieval_tools(retrieval);
    *router_cell().write().unwrap_or_else(|e| e.into_inner()) = routers;
    (out, accepted)
}

// ── ON-CREATE seeds: source lists, MCP servers, API contexts ───────────────
//
// The graph owns each of these after first creation, so runtime edits
// survive restarts. Each node records the plugin that seeded it.

#[derive(Debug, Deserialize)]
struct SourceListFile {
    name: String,
    description: String,
    domains: Vec<String>,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpServerFile {
    name: String,
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default = "none_scheme")]
    auth_scheme: String,
    #[serde(default)]
    auth_param: Option<String>,
    #[serde(default)]
    auth_env_var: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn none_scheme() -> String {
    "none".into()
}
fn yes() -> bool {
    true
}

/// An API the brain calls (`apis/*.yaml` → `(:ApiContext)`): where it is and
/// how it authenticates. The secret stays in env; only its variable's name
/// is stored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiContextFile {
    name: String,
    #[serde(default)]
    description: String,
    base_url: String,
    #[serde(default = "none_scheme")]
    auth_scheme: String,
    #[serde(default)]
    auth_param: Option<String>,
    #[serde(default)]
    auth_env_var: Option<String>,
    #[serde(default)]
    default_headers: std::collections::BTreeMap<String, String>,
    /// Node property → env var, set from the environment at every start.
    #[serde(default)]
    env_props: std::collections::BTreeMap<String, String>,
}

fn parse<T: for<'de> Deserialize<'de>>(path: &Path, what: &str) -> Option<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "{what}: cannot read file");
            return None;
        }
    };
    match serde_yaml::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "{what}: YAML parse error");
            None
        }
    }
}

/// Check one seed file (`sources`, `mcp-servers`, `apis`, `search-engines`) as its
/// seeder would parse it; a kind a compiled plugin owns (`sources-media`)
/// goes to that plugin. `Err` says what is wrong.
pub fn check_seed_file(kind: &str, path: &Path) -> Result<(), String> {
    // A kind a compiled plugin owns (`Plugin::seed_kinds`) is checked by it.
    if let Some(p) = compiled()
        .into_iter()
        .find(|p| p.seed_kinds().contains(&kind))
    {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        return p.check_seed(kind, &text);
    }
    fn strict<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))
    }
    match kind {
        "sources" => {
            let l: SourceListFile = strict(path)?;
            if l.domains.is_empty() {
                return Err("a source list needs at least one domain".into());
            }
        }
        "mcp-servers" => {
            let s: McpServerFile = strict(path)?;
            if !s.url.starts_with("http://") && !s.url.starts_with("https://") {
                return Err(format!("url '{}' must be http(s)", s.url));
            }
        }
        "apis" => {
            let a: ApiContextFile = strict(path)?;
            if !a.base_url.starts_with("http://") && !a.base_url.starts_with("https://") {
                return Err(format!("base_url '{}' must be http(s)", a.base_url));
            }
            if let Some(k) = a
                .env_props
                .keys()
                .find(|k| !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            {
                return Err(format!("env_props key '{k}' must be an identifier"));
            }
        }
        "search-engines" => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
            agent_brain_retrieve::search_engines::parse(&text)?;
        }
        other => return Err(format!("'{other}' is not an ON-CREATE seed kind")),
    }
    Ok(())
}

/// Seed source lists, MCP servers and API contexts from every loaded bundle
/// (`sources-media/` is the media plugin's: `Plugin::start`). Returns how many nodes were created.
pub async fn seed_on_create(neo4j: &Neo4jClient, plugins: &Plugins) -> usize {
    let mut created = 0usize;
    for (plugin, path) in plugins.files("sources", "yaml") {
        let Some(l) = parse::<SourceListFile>(&path, "seed sources") else {
            continue;
        };
        match neo4j
            .seed_source_list_if_absent(&l.name, &l.domains, &l.description, l.role.as_deref())
            .await
        {
            Ok(true) => {
                stamp(neo4j, "SourceList", &l.name, &plugin).await;
                info!(name = %l.name, %plugin, "Seeded SourceList (graph-owned from now on)");
                created += 1;
            }
            Ok(false) => {}
            Err(e) => warn!(name = %l.name, error = %e, "seed sources: failed"),
        }
    }
    for (plugin, path) in plugins.files("mcp-servers", "yaml") {
        let Some(s) = parse::<McpServerFile>(&path, "seed mcp-servers") else {
            continue;
        };
        let q = neo4rs::query(
            "MERGE (m:McpServer {name: $name}) \
             ON CREATE SET m.url = $url, m.description = $description, \
                           m.auth_scheme = $auth_scheme, m.auth_param = $auth_param, \
                           m.auth_env_var = $auth_env_var, m.enabled = $enabled, \
                           m.plugin = $plugin, m.created_at = datetime(), m.__new = true \
             WITH m, coalesce(m.__new, false) AS new REMOVE m.__new RETURN new",
        )
        .param("name", s.name.as_str())
        .param("url", s.url.as_str())
        .param("description", s.description.as_str())
        .param("auth_scheme", s.auth_scheme.as_str())
        .param("auth_param", s.auth_param.clone().unwrap_or_default())
        .param("auth_env_var", s.auth_env_var.clone().unwrap_or_default())
        .param("enabled", s.enabled)
        .param("plugin", plugin.as_str());
        match neo4j.execute(q).await {
            Ok(rows) => {
                if rows.first().and_then(|r| r.get::<bool>("new").ok()) == Some(true) {
                    info!(name = %s.name, %plugin, "Seeded McpServer (graph-owned from now on)");
                    created += 1;
                }
            }
            Err(e) => warn!(name = %s.name, error = %e, "seed mcp-servers: failed"),
        }
    }
    for (plugin, path) in plugins.files("apis", "yaml") {
        let Some(a) = parse::<ApiContextFile>(&path, "seed apis") else {
            continue;
        };
        let headers = if a.default_headers.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&a.default_headers).unwrap_or_default()
        };
        let q = neo4rs::query(
            "MERGE (c:ApiContext {name: $name}) \
             ON CREATE SET c.base_url = $base_url, c.auth_scheme = $auth_scheme, \
                           c.auth_param = $auth_param, c.auth_env_var = $auth_env_var, \
                           c.description = $description, c.default_headers = $headers, \
                           c.plugin = $plugin, c.__new = true \
             WITH c, coalesce(c.__new, false) AS new REMOVE c.__new RETURN new",
        )
        .param("name", a.name.as_str())
        .param("base_url", a.base_url.as_str())
        .param("auth_scheme", a.auth_scheme.as_str())
        .param("auth_param", a.auth_param.clone().unwrap_or_default())
        .param("auth_env_var", a.auth_env_var.clone().unwrap_or_default())
        .param("description", a.description.as_str())
        .param("headers", headers)
        .param("plugin", plugin.as_str());
        match neo4j.execute(q).await {
            Ok(rows) => {
                if rows.first().and_then(|r| r.get::<bool>("new").ok()) == Some(true) {
                    info!(name = %a.name, %plugin, "Seeded ApiContext (graph-owned from now on)");
                    created += 1;
                }
            }
            Err(e) => warn!(name = %a.name, error = %e, "seed apis: failed"),
        }
        for (prop, var) in &a.env_props {
            let Some(v) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) else {
                continue;
            };
            if !prop.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                warn!(name = %a.name, %prop, "seed apis: env_props key must be an identifier");
                continue;
            }
            let q = neo4rs::query(&format!(
                "MATCH (c:ApiContext {{name: $name}}) SET c.{prop} = $v"
            ))
            .param("name", a.name.as_str())
            .param("v", v);
            if let Err(e) = neo4j.run(q).await {
                warn!(name = %a.name, %prop, error = %e, "seed apis: env property not set");
            }
        }
    }
    created
}

/// Record which plugin seeded a node (by `name`).
async fn stamp(neo4j: &Neo4jClient, label: &str, name: &str, plugin: &str) {
    let q = neo4rs::query(&format!(
        "MATCH (n:{label} {{name: $name}}) SET n.plugin = $plugin"
    ))
    .param("name", name)
    .param("plugin", plugin);
    if let Err(e) = neo4j.run(q).await {
        warn!(%label, %name, error = %e, "seed: could not record the plugin");
    }
}

// ── Availability: what a workflow needs must exist ──────────────────────────

/// Why `wf` cannot run now, or `None`. A step's tool must be registered, an
/// `mcp_call` step's server must exist and be enabled, and a yaml-owned
/// workflow's plugin must be loaded. `live_tools` empty = not known, unchecked.
/// A step marked `optional` may name a tool that is not registered: the
/// queue skips it and the run goes on (a media workflow's `claim` step while
/// epistemics is off).
pub fn unavailable_reason(
    wf: &agent_brain_repository::Workflow,
    live_tools: &[String],
    mcp_servers: &std::collections::HashMap<String, bool>,
    plugin_loaded: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut reasons: Vec<String> = Vec::new();
    let mut add = |r: String| {
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    };
    if wf.managed_by.as_deref() == Some("yaml")
        && let Some(p) = wf.plugin.as_deref()
        && !plugin_loaded(p)
    {
        add(format!("plugin '{p}' is not enabled"));
    }
    let steps: Vec<serde_json::Value> = serde_json::from_str(&wf.steps).unwrap_or_default();
    for step in &steps {
        let Some(tool) = step
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
        else {
            add("a step names no tool (`tool_name`)".into());
            continue;
        };
        let optional = step
            .get("optional")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !optional && !live_tools.is_empty() && !live_tools.iter().any(|t| t == tool) {
            add(format!("tool '{tool}' is not registered"));
        }
        if tool == "mcp_call"
            && let Some(server) = step
                .pointer("/arguments/server")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.contains("{{"))
        {
            match mcp_servers.get(server) {
                None => add(format!("MCP server '{server}' is not registered")),
                Some(false) => add(format!("MCP server '{server}' is disabled")),
                Some(true) => {}
            }
        }
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// Mark every workflow with why it cannot run, or clear the mark. Run at
/// startup, once the tool registry is final. Returns how many are unavailable.
pub async fn mark_unavailable_workflows(
    neo4j: &Neo4jClient,
    live_tools: &[String],
    plugins: &Plugins,
) -> anyhow::Result<usize> {
    let mut mcp = std::collections::HashMap::new();
    for row in neo4j
        .execute(neo4rs::query(
            "MATCH (m:McpServer) RETURN m.name AS name, coalesce(m.enabled, true) AS enabled",
        ))
        .await?
    {
        if let Ok(name) = row.get::<String>("name") {
            mcp.insert(name, row.get::<bool>("enabled").unwrap_or(true));
        }
    }
    let mut unavailable = 0usize;
    for wf in neo4j.list_workflows(false).await? {
        let reason = unavailable_reason(&wf, live_tools, &mcp, &|p| plugins.is_loaded(p));
        if reason.is_some() {
            unavailable += 1;
        }
        if reason == wf.unavailable {
            continue;
        }
        neo4j
            .set_workflow_unavailable(&wf.id, reason.as_deref())
            .await?;
        match reason {
            Some(r) => {
                warn!(workflow = %wf.name, reason = %r, "Workflow cannot run — marked unavailable")
            }
            None => info!(workflow = %wf.name, "Workflow can run again — unavailable mark cleared"),
        }
    }
    Ok(unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(dir: &Path, name: &str, manifest: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("plugin.yaml"), manifest).unwrap();
    }

    fn status(p: &Plugins, name: &str) -> Status {
        p.bundles
            .iter()
            .find(|b| b.name == name)
            .unwrap()
            .status
            .clone()
    }

    #[test]
    fn the_shipped_core_bundle_loads_with_every_seed_kind() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let p = Plugins::load(&dir, &[], &|_| None);
        assert_eq!(p.core_loaded(), Ok(()));
        for k in MERGED_KINDS.iter().chain(CORE_ONLY_KINDS) {
            assert!(p.core_dir(k).is_dir(), "core bundle lacks {k}/");
        }
        assert!(p.files("workflows", "yaml").len() >= 15);
    }

    #[test]
    fn requirements_decide_what_loads_and_say_why() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        bundle(d, "core", "name: core\n");
        bundle(
            d,
            "needs-key",
            "name: needs-key\nrequires: { env: [SOME_KEY] }\n",
        );
        bundle(d, "late", "name: late\nrequires: { plugins: [base] }\n");
        bundle(d, "base", "name: base\n");
        bundle(
            d,
            "orphan",
            "name: orphan\nrequires: { plugins: [missing] }\n",
        );
        bundle(d, "misnamed", "name: other\n");
        bundle(d, "idle", "name: idle\n");
        let enabled: Vec<String> = ["needs-key", "late", "base", "orphan", "misnamed"]
            .map(String::from)
            .to_vec();
        let p = Plugins::load(d, &enabled, &|_| None);
        assert_eq!(status(&p, "core"), Status::Loaded);
        assert_eq!(
            status(&p, "late"),
            Status::Loaded,
            "a requirement listed later still loads first"
        );
        assert_eq!(status(&p, "base"), Status::Loaded);
        assert_eq!(
            status(&p, "needs-key"),
            Status::Skipped("needs env SOME_KEY".into())
        );
        assert!(matches!(status(&p, "orphan"), Status::Skipped(r) if r.contains("'missing'")));
        assert!(
            matches!(status(&p, "misnamed"), Status::Skipped(r) if r.contains("folder is 'misnamed'"))
        );
        assert_eq!(status(&p, "idle"), Status::NotEnabled);

        let with_key = Plugins::load(d, &enabled, &|k| (k == "SOME_KEY").then(|| "x".into()));
        assert_eq!(status(&with_key, "needs-key"), Status::Loaded);
    }

    #[test]
    fn files_come_core_first_and_core_only_kinds_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        bundle(d, "core", "name: core\n");
        bundle(d, "extra", "name: extra\n");
        for (b, f) in [("core", "b.yaml"), ("core", "a.yaml"), ("extra", "0.yaml")] {
            std::fs::create_dir_all(d.join(b).join("workflows")).unwrap();
            std::fs::write(d.join(b).join("workflows").join(f), "").unwrap();
        }
        // Topics merge (one per file); entities stay core-only.
        std::fs::create_dir_all(d.join("extra").join("topics")).unwrap();
        std::fs::create_dir_all(d.join("extra").join("entities")).unwrap();
        let p = Plugins::load(d, &["extra".into()], &|_| None);
        let names: Vec<String> = p
            .files("workflows", "yaml")
            .iter()
            .map(|(pl, f)| format!("{pl}/{}", f.file_name().unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(names, ["core/a.yaml", "core/b.yaml", "extra/0.yaml"]);
        let extra = p.bundles.iter().find(|b| b.name == "extra").unwrap();
        assert_eq!(extra.warnings.len(), 1);
        assert!(extra.warnings[0].starts_with("entities/"));
    }

    fn wf(
        steps: serde_json::Value,
        managed_by: &str,
        plugin: Option<&str>,
    ) -> agent_brain_repository::Workflow {
        serde_json::from_value(serde_json::json!({
            "id": "1", "name": "w", "description": null, "enabled": true,
            "interval_seconds": null, "steps": steps.to_string(), "last_run_at": null,
            "next_run_at": null, "created_at": "", "updated_at": "",
            "managed_by": managed_by, "plugin": plugin,
        }))
        .unwrap()
    }

    #[test]
    fn a_workflow_is_unavailable_for_every_missing_thing_it_names() {
        use serde_json::json;
        let live: Vec<String> = ["search_web", "mcp_call", "store_note"]
            .map(String::from)
            .to_vec();
        let mcp: std::collections::HashMap<String, bool> =
            [("exa".to_string(), true), ("off".to_string(), false)].into();
        let loaded = |p: &str| p == "core";
        let ok = wf(
            json!([{"tool_name": "search_web"}, {"tool_name": "mcp_call", "arguments": {"server": "exa"}}]),
            "yaml",
            Some("core"),
        );
        assert_eq!(unavailable_reason(&ok, &live, &mcp, &loaded), None);

        let bad = wf(
            json!([
                {"tool_name": "gone_tool"},
                {"tool_name": "gone_tool"},
                {"tool_name": "mcp_call", "arguments": {"server": "off"}},
                {"tool_name": "mcp_call", "arguments": {"server": "nobody"}},
                {"tool_name": "mcp_call", "arguments": {"server": "{{inputs.server}}"}},
                {"tool": "store_note"}
            ]),
            "yaml",
            Some("media"),
        );
        assert_eq!(
            unavailable_reason(&bad, &live, &mcp, &loaded).unwrap(),
            "plugin 'media' is not enabled; tool 'gone_tool' is not registered; \
             MCP server 'off' is disabled; MCP server 'nobody' is not registered; \
             a step names no tool (`tool_name`)"
        );
        // An optional step may name a missing tool: the queue skips it.
        let optional = wf(
            json!([{"tool_name": "store_note"}, {"tool_name": "claim", "optional": true}]),
            "yaml",
            Some("core"),
        );
        assert_eq!(unavailable_reason(&optional, &live, &mcp, &loaded), None);
        // A runtime-owned copy is judged by what it calls, not by its plugin.
        let taken = wf(
            json!([{"tool_name": "store_note"}]),
            "runtime",
            Some("media"),
        );
        assert_eq!(unavailable_reason(&taken, &live, &mcp, &loaded), None);
    }

    struct Fake(&'static str, Vec<&'static str>);
    struct FakeSkill(Vec<&'static str>);

    #[async_trait::async_trait]
    impl agent_brain_plugin_api::Skill for FakeSkill {
        fn name(&self) -> &str {
            "fake"
        }
        fn tools(&self) -> Vec<agent_brain_plugin_api::ToolDefinition> {
            self.0
                .iter()
                .map(|n| {
                    serde_json::from_value(
                        serde_json::json!({"name": n, "description": "", "inputSchema": {}}),
                    )
                    .unwrap()
                })
                .collect()
        }
        async fn execute(
            &self,
            _: &str,
            _: Option<serde_json::Value>,
        ) -> Option<agent_brain_plugin_api::ToolCallResult> {
            None
        }
    }

    impl Plugin for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn tools(&self, _: &PluginContext) -> Vec<Arc<dyn agent_brain_plugin_api::Skill>> {
            vec![Arc::new(FakeSkill(self.1.clone()))]
        }
    }

    #[test]
    fn a_plugin_that_would_shadow_a_tool_is_refused_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        bundle(d, "core", "name: core\n");
        for n in ["good", "shadow", "twice", "idle"] {
            bundle(d, n, &format!("name: {n}\n"));
        }
        bundle(d, "nocode", "name: nocode\n");
        std::fs::create_dir_all(d.join("nocode").join("crate")).unwrap();
        let enabled: Vec<String> = ["good", "shadow", "twice", "nocode"]
            .map(String::from)
            .to_vec();
        let p = Plugins::load(d, &enabled, &|_| None);
        let compiled: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(Fake("good", vec!["good_tool"])),
            Arc::new(Fake("shadow", vec!["search_web", "other"])),
            Arc::new(Fake("twice", vec!["good_tool"])),
            Arc::new(Fake("idle", vec!["idle_tool"])),
        ];
        let (skills, _) = plugin_skills(
            &p,
            &compiled,
            &["search_web".into()],
            Arc::new(|_: &str| None),
            &crate::plugin_host::Host::default(),
        );
        let names: Vec<String> = skills
            .iter()
            .flat_map(|s| s.tools())
            .map(|t| t.name)
            .collect();
        assert_eq!(
            names,
            ["good_tool"],
            "only the plugin with free names registers; an un-enabled one never runs"
        );
        let st = code_status();
        assert_eq!(st["good"].tools, ["good_tool"]);
        assert!(
            st["shadow"]
                .error
                .as_deref()
                .unwrap()
                .contains("search_web")
        );
        assert!(st["twice"].error.as_deref().unwrap().contains("good_tool"));
        assert!(
            st["nocode"]
                .error
                .as_deref()
                .unwrap()
                .contains("not built with")
        );
        assert!(!st.contains_key("idle"));
    }

    #[test]
    fn every_compiled_plugin_has_a_bundle() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        for plugin in compiled() {
            let m = read_manifest(&dir.join(plugin.name()), plugin.name());
            assert!(m.is_ok(), "{}: {:?}", plugin.name(), m.err());
        }
    }

    #[test]
    fn a_missing_core_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Plugins::load(tmp.path(), &[], &|_| None);
        assert!(p.core_loaded().is_err());
    }
}
