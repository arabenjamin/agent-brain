//! `manage_plugin`: the brain drafts plugin bundles (CORE_AND_PLUGINS_PLAN
//! Phase 3d).
//!
//! A bundle the brain writes goes to `PLUGINS_RUNTIME_DIR`, never to the
//! shipped `PLUGINS_DIR`. Every file is checked by the parser its loader
//! uses (workflow steps against the live tools and their schemas, declared
//! tools by their own checks, profiles, seeds) and a bundle with any bad file
//! is not written at all. A written bundle is inert: it loads on the next
//! start only when the owner adds its name to `PLUGINS_ENABLED`, which a Feed
//! item asks them to review. The brain can rewrite or delete only bundles it
//! wrote (`author: brain`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::bundle_tools::ToolFile;
use crate::plugins::{self, CORE, CORE_ONLY_KINDS, MERGED_KINDS, Manifest, Plugins, Requires};
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;

/// What `manage_plugin` stamps on the bundles it writes.
pub const AUTHOR: &str = "brain";

pub struct PluginManagerSkill {
    neo4j: Neo4jClient,
    live_tools: Arc<RwLock<Vec<String>>>,
    tool_schemas: Arc<RwLock<HashMap<String, Value>>>,
    /// The model catalog a step's `model` is checked against, read per call.
    telemetry: Option<agent_brain_repository::TelemetryClient>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    name: String,
    description: String,
    #[serde(default)]
    requires: Requires,
    /// `"<kind>/<file>.yaml"` → the file, as an object or as YAML text.
    files: BTreeMap<String, Value>,
}

impl PluginManagerSkill {
    pub fn new(
        neo4j: Neo4jClient,
        live_tools: Arc<RwLock<Vec<String>>>,
        tool_schemas: Arc<RwLock<HashMap<String, Value>>>,
        telemetry: Option<agent_brain_repository::TelemetryClient>,
    ) -> Self {
        Self {
            neo4j,
            live_tools,
            tool_schemas,
            telemetry,
        }
    }

    fn list(&self) -> ToolCallResult {
        let global = Plugins::global();
        let mut rows: Vec<Value> = global
            .bundles
            .iter()
            .map(|b| {
                json!({
                    "name": b.name,
                    "description": b.manifest.as_ref().map(|m| m.description.clone()),
                    "author": b.manifest.as_ref().and_then(|m| m.author.clone()),
                    "status": b.status,
                    "runtime": b.runtime,
                })
            })
            .collect();
        // Bundles written since this process started are not in the snapshot.
        for name in runtime_bundles() {
            if !global.bundles.iter().any(|b| b.name == name) {
                let m = plugins::read_manifest(&plugins::runtime_dir().join(&name), &name).ok();
                rows.push(json!({
                    "name": name,
                    "description": m.as_ref().map(|m| m.description.clone()),
                    "author": m.as_ref().and_then(|m| m.author.clone()),
                    "status": "written_since_start",
                    "runtime": true,
                }));
            }
        }
        ToolCallResult::success_json(json!({
            "count": rows.len(),
            "plugins": rows,
            "note": "A runtime bundle loads only when PLUGINS_ENABLED names it, on the next start.",
        }))
    }

    fn get(&self, name: &str) -> ToolCallResult {
        let Some(root) = bundle_root(name) else {
            return ToolCallResult::error(format!(
                "No plugin named '{name}'. `manage_plugin` action list names every one."
            ));
        };
        let manifest = match plugins::read_manifest(&root, name) {
            Ok(m) => m,
            Err(e) => return ToolCallResult::error(e),
        };
        let mut files = BTreeMap::new();
        for kind in MERGED_KINDS {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(root.join(kind))
                .map(|e| {
                    e.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .collect()
                })
                .unwrap_or_default();
            paths.sort();
            for p in paths {
                let file = p
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("")
                    .to_string();
                files.insert(
                    format!("{kind}/{file}"),
                    std::fs::read_to_string(&p).unwrap_or_else(|e| format!("<unreadable: {e}>")),
                );
            }
        }
        ToolCallResult::success_json(json!({
            "name": name,
            "manifest": manifest,
            "path": root.display().to_string(),
            "loaded": Plugins::global().is_loaded(name),
            "files": files,
        }))
    }

    async fn write(&self, args: WriteArgs) -> ToolCallResult {
        let name = args.name.trim().to_string();
        if let Err(e) = writable(&name) {
            return ToolCallResult::error(e);
        }
        if args.description.trim().is_empty() {
            return ToolCallResult::error(
                "a plugin needs a description: the owner reviews it by that",
            );
        }
        if args.files.is_empty() {
            return ToolCallResult::error(
                "`files` is empty: give at least one \"<kind>/<file>.yaml\" (workflows, tools, contexts, sources, sources-media, mcp-servers, apis) or \"schemas/<file>.json\"",
            );
        }
        let runtime = plugins::runtime_dir();
        let target = runtime.join(&name);
        let version = plugins::read_manifest(&target, &name)
            .map(|m| m.version + 1)
            .unwrap_or(1);
        let manifest = Manifest {
            name: name.clone(),
            version,
            description: args.description.trim().to_string(),
            requires: args.requires,
            author: Some(AUTHOR.into()),
        };

        // Stage the whole bundle, check it as its loaders would, then swap it in.
        let staging = runtime.join(format!(".staging-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let mut errors = Vec::new();
        // Every problem in one answer: files with a good path are checked
        // even when another file's path was refused.
        stage(&staging, &manifest, &args.files, &mut errors);
        if staging.join("plugin.yaml").is_file() {
            self.check_bundle(&name, &staging, &args.files, &mut errors)
                .await;
        }
        if !errors.is_empty() {
            let _ = std::fs::remove_dir_all(&staging);
            return ToolCallResult::error(format!(
                "Plugin '{name}' NOT written ({} problem(s)); fix and write the whole bundle again:\n- {}",
                errors.len(),
                errors.join("\n- ")
            ));
        }
        if let Err(e) = swap_in(&staging, &target) {
            let _ = std::fs::remove_dir_all(&staging);
            return ToolCallResult::error(format!("Plugin '{name}' checked but not written: {e}"));
        }
        info!(plugin = %name, version, files = args.files.len(), "manage_plugin: bundle written");

        let enabled = enabled_names().contains(&name);
        let item = review_item(&self.neo4j, &manifest, &args.files, enabled).await;
        ToolCallResult::success_json(json!({
            "written": name,
            "version": version,
            "path": target.display().to_string(),
            "files": args.files.keys().collect::<Vec<_>>(),
            "enabled": enabled,
            "review_item": item.as_ref().ok(),
            "review_item_error": item.as_ref().err(),
            "next": if enabled {
                "PLUGINS_ENABLED already names this plugin: it loads, as written, on the next restart."
            } else {
                "Not loaded. The owner reviews the Feed item and enables it by adding the name to PLUGINS_ENABLED; it loads on the next restart. Do not tell the user it is running."
            },
        }))
    }

    fn delete(&self, name: &str) -> ToolCallResult {
        if let Err(e) = writable(name) {
            return ToolCallResult::error(e);
        }
        let target = plugins::runtime_dir().join(name);
        if !target.is_dir() {
            return ToolCallResult::error(format!("No brain-written plugin named '{name}'."));
        }
        if let Err(e) = std::fs::remove_dir_all(&target) {
            return ToolCallResult::error(format!("Could not delete '{name}': {e}"));
        }
        info!(plugin = %name, "manage_plugin: bundle deleted");
        let loaded = Plugins::global().is_loaded(name);
        ToolCallResult::success_json(json!({
            "deleted": name,
            "note": if loaded {
                "It stays loaded until the next restart; remove it from PLUGINS_ENABLED too."
            } else {
                "It was not loaded."
            },
        }))
    }

    /// Check every staged file with its loader's parser; push what is wrong.
    async fn check_bundle(
        &self,
        name: &str,
        root: &Path,
        files: &BTreeMap<String, Value>,
        errors: &mut Vec<String>,
    ) {
        // The bundle's own tools count as live for its workflows.
        let mut own_tools: Vec<ToolFile> = Vec::new();
        let mine = plugins::declared_status()
            .get(name)
            .map(|c| c.tools.clone())
            .unwrap_or_default();
        let live = self.live_tools.read().await.clone();
        let staged: Vec<&String> = files.keys().filter(|k| file_path_ok(k).is_ok()).collect();
        for key in staged.iter().filter(|k| k.starts_with("tools/")) {
            match ToolFile::load(&root.join(key)) {
                Ok(t) if live.contains(&t.name) && !mine.contains(&t.name) => {
                    errors.push(format!("{key}: tool name '{}' is already taken", t.name))
                }
                Ok(t) => own_tools.push(t),
                Err(e) => errors.push(format!("{key}: {e}")),
            }
        }
        let mut live_tools = live;
        let mut schemas = self.tool_schemas.read().await.clone();
        for t in &own_tools {
            live_tools.push(t.name.clone());
            schemas.insert(t.name.clone(), t.input_schema.clone());
        }

        let topics = agent_brain_work::workflow_seeder::declared_topics();
        let profiles = agent_brain_work::profiles::load(&self.neo4j)
            .await
            .unwrap_or_default();
        for key in staged {
            let (kind, _) = key.split_once('/').unwrap_or(("", ""));
            let path = root.join(key);
            let result: Result<(), String> = match kind {
                "tools" => continue,
                "workflows" => {
                    self.check_workflow(name, &path, &topics, &live_tools, &schemas)
                        .await
                }
                "contexts" => check_context(name, &path, &profiles),
                "views" => crate::views::ViewFile::load(&path).map(|_| ()),
                "programs" => agent_brain_work::programs::load_file(&path).map(|_| ()),
                "schemas" => check_schema(&path),
                _ => plugins::check_seed_file(kind, &path),
            };
            if let Err(e) = result {
                errors.push(format!("{key}: {e}"));
            }
        }
    }

    async fn check_workflow(
        &self,
        plugin: &str,
        path: &Path,
        topics: &[String],
        live_tools: &[String],
        schemas: &HashMap<String, Value>,
    ) -> Result<(), String> {
        use agent_brain_work::validation;
        let (def, _) = agent_brain_work::workflow_seeder::load_file(path, topics)?;
        if let Ok(Some(wf)) = self.neo4j.get_workflow_by_name(&def.name).await
            && wf.plugin.as_deref() != Some(plugin)
        {
            return Err(format!(
                "a workflow named '{}' already exists (plugin {}); pick another name",
                def.name,
                wf.plugin.as_deref().unwrap_or("none")
            ));
        }
        let steps: Vec<Value> = serde_json::from_str(&def.steps).map_err(|e| e.to_string())?;
        if steps.is_empty() {
            return Err("a workflow needs at least one step".into());
        }
        let declared: Vec<String> = def.inputs.iter().map(|i| i.name.clone()).collect();
        let mut v = validation::validate_step_array(&steps, live_tools, schemas);
        let catalog = self
            .telemetry
            .as_ref()
            .map(agent_brain_llm::model_router::catalog_models)
            .unwrap_or_default();
        v.extend(validation::validate_step_models(&steps, &catalog));
        v.extend(validation::validate_input_refs(&steps, &declared));
        v.extend(validation::validate_step_when(&steps));
        if validation::model_driven_call() {
            v.extend(validation::validate_raw_graph_writes(&steps, &[]));
        }
        if validation::has_hard(&v) {
            return Err(validation::render_hard(&v));
        }
        serde_json::from_value::<Vec<agent_brain_work::queue::ChainStep>>(Value::Array(steps))
            .map_err(|e| format!("steps: {e}"))?;
        Ok(())
    }
}

/// Refuse a name the brain may not write: not a slug, the core, a shipped
/// bundle, or a runtime bundle someone else wrote.
fn writable(name: &str) -> Result<(), String> {
    if !agent_brain_work::workflows::is_slug(name) {
        return Err(format!(
            "plugin name '{name}' must be a slug: lowercase letters, digits and dashes"
        ));
    }
    if name == CORE || Plugins::global().dir.join(name).exists() {
        return Err(format!(
            "'{name}' is a shipped bundle; manage_plugin writes only its own bundles. Pick another name."
        ));
    }
    if Plugins::global()
        .extra_dirs
        .iter()
        .any(|d| d.join(name).exists())
    {
        return Err(format!(
            "'{name}' is a bundle in PLUGINS_EXTRA_DIRS; manage_plugin writes only its own bundles. Pick another name."
        ));
    }
    let target = plugins::runtime_dir().join(name);
    if target.exists() {
        match plugins::read_manifest(&target, name) {
            Ok(m) if m.author.as_deref() == Some(AUTHOR) => {}
            _ => {
                return Err(format!(
                    "'{name}' in the runtime dir was not written by manage_plugin; it is left alone."
                ));
            }
        }
    }
    Ok(())
}

/// Write the manifest and every file under `root`. Bad paths are errors.
fn stage(
    root: &Path,
    manifest: &Manifest,
    files: &BTreeMap<String, Value>,
    errors: &mut Vec<String>,
) -> bool {
    let write = |path: &Path, text: &str| -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| e.to_string())
    };
    match serde_yaml::to_string(manifest) {
        Ok(text) => {
            if let Err(e) = write(&root.join("plugin.yaml"), &text) {
                errors.push(format!("plugin.yaml: {e}"));
                return false;
            }
        }
        Err(e) => {
            errors.push(format!("plugin.yaml: {e}"));
            return false;
        }
    }
    for (key, content) in files {
        let text = match file_path_ok(key).and_then(|json| render(content, json)) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{key}: {e}"));
                continue;
            }
        };
        if let Err(e) = write(&root.join(key), &text) {
            errors.push(format!("{key}: {e}"));
        }
    }
    errors.is_empty()
}

/// `"<kind>/<file>.<ext>"` with a merged kind and the kind's extension.
/// Returns whether the file is JSON (`schemas/`).
fn file_path_ok(key: &str) -> Result<bool, String> {
    let Some((kind, file)) = key.split_once('/') else {
        return Err("must be \"<kind>/<file>.yaml\"".into());
    };
    if CORE_ONLY_KINDS.contains(&kind) {
        return Err(format!("{kind}/ is read from the core bundle only"));
    }
    if !MERGED_KINDS.contains(&kind) {
        return Err(format!(
            "'{kind}' is not a bundle kind ({})",
            MERGED_KINDS.join(", ")
        ));
    }
    let json = kind == "schemas";
    let ext = if json { ".json" } else { ".yaml" };
    let stem = file
        .strip_suffix(ext)
        .ok_or_else(|| format!("a {kind}/ file ends in {ext}"))?;
    if stem.is_empty()
        || !stem
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err("file names are lowercase letters, digits, '-' and '_'".into());
    }
    Ok(json)
}

/// A file's text: a string as given, an object serialized.
fn render(content: &Value, json: bool) -> Result<String, String> {
    match content {
        Value::String(s) => Ok(s.clone()),
        v if json => serde_json::to_string_pretty(v).map_err(|e| e.to_string()),
        v => serde_yaml::to_string(v).map_err(|e| e.to_string()),
    }
}

fn check_context(
    plugin: &str,
    path: &Path,
    profiles: &[(
        agent_brain_work::context_builder::ContextProfile,
        agent_brain_work::profiles::ProfileMeta,
    )],
) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let raw: Value = serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))?;
    if raw.get("extends").is_some() {
        let x: agent_brain_work::profiles::ProfileExtension =
            serde_json::from_value(raw).map_err(|e| format!("profile extension: {e}"))?;
        if !profiles.iter().any(|(p, _)| p.name == x.extends) {
            return Err(format!("extends '{}', which is not a profile", x.extends));
        }
        return Ok(());
    }
    let p: agent_brain_work::context_builder::ContextProfile =
        serde_json::from_value(raw).map_err(|e| format!("profile: {e}"))?;
    if let Some((_, meta)) = profiles.iter().find(|(q, _)| q.name == p.name)
        && meta.plugin.as_deref() != Some(plugin)
    {
        return Err(format!(
            "a profile named '{}' already exists (plugin {}); pick another name or extend it",
            p.name,
            meta.plugin.as_deref().unwrap_or("none")
        ));
    }
    Ok(())
}

fn check_schema(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("JSON: {e}"))?;
    jsonschema::validator_for(&v)
        .map(|_| ())
        .map_err(|e| format!("not a JSON Schema: {e}"))
}

/// Replace `target` with `staging` (a rename each way, so a reader never
/// sees half a bundle).
fn swap_in(staging: &Path, target: &Path) -> std::io::Result<()> {
    if target.exists() {
        let old = target.with_file_name(format!(
            ".old-{}",
            target
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("bundle")
        ));
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(target, &old)?;
        std::fs::rename(staging, target)?;
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    } else {
        std::fs::rename(staging, target)
    }
}

/// The shipped dir first, then the runtime dir.
fn bundle_root(name: &str) -> Option<PathBuf> {
    let shipped = Plugins::global().dir.join(name);
    let runtime = plugins::runtime_dir().join(name);
    [shipped, runtime]
        .into_iter()
        .find(|r| r.join("plugin.yaml").is_file())
}

/// Bundle folders in the runtime dir (staging and swap folders excluded).
fn runtime_bundles() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(plugins::runtime_dir())
        .map(|e| {
            e.flatten()
                .filter(|e| e.path().join("plugin.yaml").is_file())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn enabled_names() -> Vec<String> {
    std::env::var("PLUGINS_ENABLED")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A Feed item asking the owner to review the bundle. One per version.
async fn review_item(
    neo4j: &Neo4jClient,
    m: &Manifest,
    files: &BTreeMap<String, Value>,
    enabled: bool,
) -> Result<String, String> {
    let list: Vec<String> = files.keys().map(|k| format!("- `{k}`")).collect();
    let requires = if m.requires.env.is_empty() && m.requires.plugins.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nRequires env: {:?}; plugins: {:?}.",
            m.requires.env, m.requires.plugins
        )
    };
    let how = if enabled {
        "PLUGINS_ENABLED already names it: it loads as written on the next restart."
    } else {
        "To enable it, add its name to PLUGINS_ENABLED and restart. Until then nothing in it runs."
    };
    let body = format!(
        "The brain wrote plugin bundle **{}** (version {}) to `{}`.\n\n{}\n\nFiles:\n{}{}\n\n{how} \
         Read it with `manage_plugin` action get, or in the folder.",
        m.name,
        m.version,
        plugins::runtime_dir().join(&m.name).display(),
        m.description,
        list.join("\n"),
        requires,
    );
    let item = agent_brain_scheduler::items::NewItem {
        title: format!("Review plugin: {} v{}", m.name, m.version),
        description: Some(body),
        priority: Some(2),
        patch: agent_brain_repository::TodoItemPatch {
            assignee: Some(Some("user".into())),
            origin_kind: Some(Some("plugin".into())),
            origin_ref: Some(Some(format!("{}@{}", m.name, m.version))),
            ..Default::default()
        },
        ..Default::default()
    };
    agent_brain_scheduler::items::create_item(neo4j, item)
        .await
        .map(|(t, _)| t.id)
        .map_err(|e| {
            warn!(plugin = %m.name, error = %e, "manage_plugin: review item not created");
            e.to_string()
        })
}

#[async_trait]
impl Skill for PluginManagerSkill {
    fn name(&self) -> &str {
        "Plugins"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "manage_plugin".into(),
            description: "Draft plugin bundles: workflows, declared read-only Cypher tools, \
                profiles, sources, MCP servers and APIs, packaged under one name. \
                action list | get {name} | write {name, description, requires?, files} | delete {name}. \
                `files` maps \"<kind>/<file>.yaml\" (kinds: workflows, tools, contexts, sources, \
                sources-media, mcp-servers, apis; or \"schemas/<file>.json\") to the file as an object \
                or YAML text; a write replaces the whole bundle. Every file is checked as its loader \
                would (workflow steps against the live tools); one bad file and nothing is written. \
                A written bundle does NOT run: the owner reviews it in the Feed and enables it via \
                PLUGINS_ENABLED, effective on restart. Only bundles you wrote can be rewritten or deleted."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "write", "delete"]},
                    "name": {"type": "string", "description": "Bundle name, a slug (lowercase, digits, dashes)."},
                    "description": {"type": "string", "description": "write: what the bundle is for (the owner reviews it by this)."},
                    "requires": {
                        "type": "object",
                        "properties": {
                            "env": {"type": "array", "items": {"type": "string"}},
                            "plugins": {"type": "array", "items": {"type": "string"}}
                        },
                        "additionalProperties": false
                    },
                    "files": {
                        "type": "object",
                        "description": "write: \"<kind>/<file>.yaml\" → the file (object or YAML text). The file name of a workflow or tool equals its `name`.",
                        "additionalProperties": {"type": ["object", "string"]}
                    }
                },
                "required": ["action"]
            }),
        }]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "manage_plugin" {
            return None;
        }
        let args = arguments.unwrap_or(Value::Null);
        let name = args.get("name").and_then(Value::as_str).map(str::trim);
        let result = match (args.get("action").and_then(Value::as_str), name) {
            (Some("list"), _) => self.list(),
            (Some("get"), Some(n)) => self.get(n),
            (Some("delete"), Some(n)) => self.delete(n),
            (Some("write"), Some(_)) => {
                let mut body = args.clone();
                if let Some(o) = body.as_object_mut() {
                    o.remove("action");
                }
                match serde_json::from_value::<WriteArgs>(body) {
                    Ok(w) => self.write(w).await,
                    Err(e) => ToolCallResult::error(format!("write: {e}")),
                }
            }
            (Some("get" | "delete" | "write"), None) => {
                ToolCallResult::error("`name` is required for this action")
            }
            (other, _) => ToolCallResult::error(format!(
                "unknown action {other:?}: use list, get, write or delete"
            )),
        };
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_paths() {
        assert_eq!(file_path_ok("workflows/daily-x.yaml"), Ok(false));
        assert_eq!(file_path_ok("schemas/thing.json"), Ok(true));
        assert_eq!(file_path_ok("topics/x.yaml"), Ok(false));
        assert!(
            file_path_ok("entities/x.yaml")
                .unwrap_err()
                .contains("core bundle only")
        );
        assert!(file_path_ok("nope/x.yaml").is_err());
        assert!(file_path_ok("workflows/x.json").is_err());
        assert!(file_path_ok("workflows/../x.yaml").is_err());
        assert!(file_path_ok("workflows/a/b.yaml").is_err());
        assert!(file_path_ok("x.yaml").is_err());
    }

    #[test]
    fn writable_names() {
        assert!(writable("Bad Name").is_err());
        assert!(writable("core").is_err());
    }

    #[test]
    fn stage_writes_manifest_and_files() {
        let dir = std::env::temp_dir().join(format!("pm-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let m = Manifest {
            name: "x".into(),
            version: 1,
            description: "d".into(),
            requires: Requires::default(),
            author: Some(AUTHOR.into()),
        };
        let mut files = BTreeMap::new();
        files.insert(
            "sources/a.yaml".to_string(),
            json!({"name": "a", "description": "d", "domains": ["x.org"]}),
        );
        files.insert(
            "apis/b.yaml".to_string(),
            json!("name: b\nbase_url: https://b.example\n"),
        );
        let mut errors = vec![];
        assert!(stage(&dir, &m, &files, &mut errors), "{errors:?}");
        let back = plugins::read_manifest(&dir, "x").unwrap();
        assert_eq!(back.author.as_deref(), Some(AUTHOR));
        assert!(plugins::check_seed_file("sources", &dir.join("sources/a.yaml")).is_ok());
        assert!(plugins::check_seed_file("apis", &dir.join("apis/b.yaml")).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
