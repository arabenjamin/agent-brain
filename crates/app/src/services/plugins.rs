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

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::repository::Neo4jClient;

/// The bundle every brain loads.
pub const CORE: &str = "core";

/// Seed dirs read from every enabled bundle.
pub const MERGED_KINDS: &[&str] = &[
    "workflows",
    "contexts",
    "sources",
    "sources-media",
    "mcp-servers",
    "schemas",
];

/// Seed dirs read from the core bundle only, for now: each builds one global
/// set (`_scope.yaml`, the entity rules) that has no merge rule yet. A plugin
/// shipping one is reported, never silently ignored.
pub const CORE_ONLY_KINDS: &[&str] = &["topics", "entities", "relations", "architecture"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires: Requires,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Env vars that must be set and non-empty.
    #[serde(default)]
    pub env: Vec<String>,
    /// Other plugins that must be loaded.
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum Status {
    Loaded,
    /// Present in `PLUGINS_DIR` but not named in `PLUGINS_ENABLED`.
    NotEnabled,
    /// Enabled but not loaded, with why.
    Skipped(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Bundle {
    pub name: String,
    pub root: PathBuf,
    pub manifest: Option<Manifest>,
    #[serde(flatten)]
    pub status: Status,
    /// Things worth knowing about a loaded bundle (a core-only kind it ships).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugins {
    pub dir: PathBuf,
    pub bundles: Vec<Bundle>,
}

impl Plugins {
    /// The bundles under `dir`, `core` first, then `enabled` in order, then
    /// any others present (as `NotEnabled`). `env` answers `requires.env`.
    pub fn load(dir: &Path, enabled: &[String], env: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut wanted: Vec<String> = vec![CORE.to_string()];
        for n in enabled {
            let n = n.trim();
            if !n.is_empty() && !wanted.iter().any(|w| w == n) {
                wanted.push(n.to_string());
            }
        }
        let mut bundles: Vec<Bundle> = Vec::new();
        // A plugin may require one listed after it, so resolve to a fixpoint.
        let mut pending: Vec<(String, Result<Manifest, String>)> = wanted
            .iter()
            .map(|n| (n.clone(), read_manifest(&dir.join(n), n)))
            .collect();
        loop {
            let before = pending.len();
            let mut still = Vec::new();
            for (name, m) in pending {
                match check(&name, &m, &bundles, env) {
                    Check::Ready => bundles.push(Bundle {
                        root: dir.join(&name),
                        warnings: core_only_warnings(&dir.join(&name), &name),
                        manifest: m.ok(),
                        name,
                        status: Status::Loaded,
                    }),
                    Check::Fail(reason) => bundles.push(Bundle {
                        root: dir.join(&name),
                        manifest: m.ok(),
                        name,
                        status: Status::Skipped(reason),
                        warnings: vec![],
                    }),
                    Check::Wait(_) => still.push((name, m)),
                }
            }
            if still.is_empty() {
                break;
            }
            if still.len() == before {
                for (name, m) in still {
                    let reason = match check(&name, &m, &bundles, env) {
                        Check::Wait(r) | Check::Fail(r) => r,
                        Check::Ready => unreachable!("a ready bundle is never left pending"),
                    };
                    bundles.push(Bundle {
                        root: dir.join(&name),
                        manifest: m.ok(),
                        name,
                        status: Status::Skipped(reason),
                        warnings: vec![],
                    });
                }
                break;
            }
            pending = still;
        }
        // Keep the requested order for everything that resolved.
        bundles.sort_by_key(|b| wanted.iter().position(|w| *w == b.name));
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut others: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().join("plugin.yaml").is_file())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| !wanted.contains(n))
                .collect();
            others.sort();
            for name in others {
                bundles.push(Bundle {
                    root: dir.join(&name),
                    manifest: read_manifest(&dir.join(&name), &name).ok(),
                    name,
                    status: Status::NotEnabled,
                    warnings: vec![],
                });
            }
        }
        Self {
            dir: dir.to_path_buf(),
            bundles,
        }
    }

    /// From `PLUGINS_DIR` and `PLUGINS_ENABLED`, once per process.
    pub fn global() -> Arc<Plugins> {
        static P: OnceLock<Arc<Plugins>> = OnceLock::new();
        P.get_or_init(|| {
            let dir =
                PathBuf::from(std::env::var("PLUGINS_DIR").unwrap_or_else(|_| "plugins".into()));
            let enabled: Vec<String> = std::env::var("PLUGINS_ENABLED")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let p = Plugins::load(&dir, &enabled, &|k| std::env::var(k).ok());
            for b in &p.bundles {
                match &b.status {
                    Status::Loaded => info!(plugin = %b.name, "Plugin loaded"),
                    Status::NotEnabled => debug!(plugin = %b.name, "Plugin present, not enabled"),
                    Status::Skipped(r) => warn!(plugin = %b.name, reason = %r, "Plugin skipped"),
                }
                for w in &b.warnings {
                    warn!(plugin = %b.name, "{w}");
                }
            }
            Arc::new(p)
        })
        .clone()
    }

    pub fn loaded(&self) -> impl Iterator<Item = &Bundle> {
        self.bundles.iter().filter(|b| b.status == Status::Loaded)
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded().any(|b| b.name == name)
    }

    /// Whether the core bundle loaded. Without it the brain must not start.
    pub fn core_loaded(&self) -> Result<(), String> {
        match self
            .bundles
            .iter()
            .find(|b| b.name == CORE)
            .map(|b| &b.status)
        {
            Some(Status::Loaded) => Ok(()),
            Some(Status::Skipped(r)) => Err(r.clone()),
            _ => Err(format!("no core bundle in {}", self.dir.display())),
        }
    }

    /// `(plugin, dir)` for each loaded bundle that has `kind/`, core first.
    pub fn dirs(&self, kind: &str) -> Vec<(String, PathBuf)> {
        self.loaded()
            .map(|b| (b.name.clone(), b.root.join(kind)))
            .filter(|(_, d)| d.is_dir())
            .collect()
    }

    /// `(plugin, file)` for every `*.<ext>` in `kind/` of each loaded bundle,
    /// core first, sorted by name within a bundle.
    pub fn files(&self, kind: &str, ext: &str) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for (plugin, dir) in self.dirs(kind) {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|e| e.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.retain(|p| p.extension().and_then(|e| e.to_str()) == Some(ext));
            files.sort();
            out.extend(files.into_iter().map(|f| (plugin.clone(), f)));
        }
        out
    }

    /// The core bundle's `kind/` dir (whether or not it exists).
    pub fn core_dir(&self, kind: &str) -> PathBuf {
        self.dir.join(CORE).join(kind)
    }
}

enum Check {
    Ready,
    Wait(String),
    Fail(String),
}

fn check(
    name: &str,
    m: &Result<Manifest, String>,
    loaded: &[Bundle],
    env: &dyn Fn(&str) -> Option<String>,
) -> Check {
    let m = match m {
        Ok(m) => m,
        Err(e) => return Check::Fail(e.clone()),
    };
    let unset: Vec<&str> = m
        .requires
        .env
        .iter()
        .filter(|k| env(k).is_none_or(|v| v.trim().is_empty()))
        .map(String::as_str)
        .collect();
    if !unset.is_empty() {
        return Check::Fail(format!("needs env {}", unset.join(", ")));
    }
    for p in &m.requires.plugins {
        match loaded.iter().find(|b| b.name == *p).map(|b| &b.status) {
            Some(Status::Loaded) => {}
            Some(_) => return Check::Fail(format!("needs plugin '{p}', which did not load")),
            None if p == name => return Check::Fail("requires itself".into()),
            None => return Check::Wait(format!("needs plugin '{p}', which is not enabled")),
        }
    }
    Check::Ready
}

fn read_manifest(root: &Path, name: &str) -> Result<Manifest, String> {
    let path = root.join("plugin.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let m: Manifest = serde_yaml::from_str(&text).map_err(|e| format!("plugin.yaml: {e}"))?;
    if m.name != name {
        return Err(format!(
            "plugin.yaml names '{}' but the folder is '{name}'",
            m.name
        ));
    }
    Ok(m)
}

fn core_only_warnings(root: &Path, name: &str) -> Vec<String> {
    if name == CORE {
        return vec![];
    }
    CORE_ONLY_KINDS
        .iter()
        .filter(|k| root.join(k).is_dir())
        .map(|k| {
            format!("{k}/ is read from the core bundle only; this plugin's {k}/ is not loaded")
        })
        .collect()
}

// ── ON-CREATE seeds: source lists, media sources, MCP servers ──────────────
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

/// A group of watched sources (`sources-media/*.yaml`).
#[derive(Debug, Deserialize)]
struct MediaSourceGroupFile {
    name: String,
    #[serde(default)]
    description: String,
    sources: Vec<MediaSourceEntry>,
}

#[derive(Debug, Deserialize)]
struct MediaSourceEntry {
    #[serde(default)]
    name: Option<String>,
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "yes")]
    active: bool,
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

/// Seed source lists, media sources and MCP servers from every loaded
/// bundle. Returns how many nodes were created.
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
    for (plugin, path) in plugins.files("sources-media", "yaml") {
        let Some(g) = parse::<MediaSourceGroupFile>(&path, "seed sources-media") else {
            continue;
        };
        for s in &g.sources {
            let name = s
                .name
                .clone()
                .unwrap_or_else(|| format!("{}-{}", g.name, s.reference));
            let description = s
                .description
                .clone()
                .unwrap_or_else(|| g.description.clone());
            match neo4j
                .seed_media_source_if_absent(&name, &s.kind, &s.reference, &description, s.active)
                .await
            {
                Ok(true) => {
                    stamp(neo4j, "MediaSource", &name, &plugin).await;
                    info!(%name, kind = %s.kind, %plugin, "Seeded MediaSource (graph-owned from now on)");
                    created += 1;
                }
                Ok(false) => {}
                Err(e) => warn!(%name, error = %e, "seed sources-media: failed"),
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
pub fn unavailable_reason(
    wf: &crate::repository::Workflow,
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
        if !live_tools.is_empty() && !live_tools.iter().any(|t| t == tool) {
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
        assert!(p.files("workflows", "yaml").len() > 20);
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
        std::fs::create_dir_all(d.join("extra").join("topics")).unwrap();
        let p = Plugins::load(d, &["extra".into()], &|_| None);
        let names: Vec<String> = p
            .files("workflows", "yaml")
            .iter()
            .map(|(pl, f)| format!("{pl}/{}", f.file_name().unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(names, ["core/a.yaml", "core/b.yaml", "extra/0.yaml"]);
        let extra = p.bundles.iter().find(|b| b.name == "extra").unwrap();
        assert_eq!(extra.warnings.len(), 1);
        assert!(extra.warnings[0].starts_with("topics/"));
    }

    fn wf(
        steps: serde_json::Value,
        managed_by: &str,
        plugin: Option<&str>,
    ) -> crate::repository::Workflow {
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
        // A runtime-owned copy is judged by what it calls, not by its plugin.
        let taken = wf(
            json!([{"tool_name": "store_note"}]),
            "runtime",
            Some("media"),
        );
        assert_eq!(unavailable_reason(&taken, &live, &mcp, &loaded), None);
    }

    #[test]
    fn a_missing_core_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Plugins::load(tmp.path(), &[], &|_| None);
        assert!(p.core_loaded().is_err());
    }
}
