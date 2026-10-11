//! Context profile loader and bundle builder.
//!
//! Profiles live in the graph (`services/profiles.rs`), seeded from the
//! plugin bundles' `contexts/`; with no Neo4j they are read from the files.
//! This service caches them, auto-assigns profiles to goals via keyword
//! matching, and builds `ContextBundle`s: the rendered prompt (sections whose
//! tools are available, the generated tool catalogue) plus pre-loaded notes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::profiles::{BundleProfiles, Extensions, ProfileAudit, ProfileMeta};
use crate::tools::ToolHandler;
use agent_brain_llm::llm::{LlmClient, LlmConfig};
use agent_brain_repository::Neo4jClient;

// ============================================================================
// Types
// ============================================================================

/// Declarative "mini-agent contract" loaded from a YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextProfile {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub system_prompt: String,
    pub token_budget: Option<usize>,
    pub pre_load_query: Option<String>,
    pub model_preference: Option<String>,
    pub provider_hint: Option<String>,
    /// Whether this profile may be invoked as a sub-agent by an orchestrator via
    /// `delegate`. Default false — the human entry points (`general`,
    /// `orchestrator`) must never be spawnable, which would let an orchestrator
    /// delegate to itself. See project-docs/SUBAGENT_ORCHESTRATION_PLAN.md §3.1.
    #[serde(default)]
    pub spawnable: bool,
    /// One-line description of what this agent is *for*, injected into the
    /// orchestrator's `delegate` roster so it knows when to pick this worker.
    /// Falls back to `description` when absent.
    #[serde(default)]
    pub specialty: Option<String>,
    /// Who may see this profile's turns (`services/routing.rs`): `private`
    /// (the default when absent) keeps them on `no_log` providers; `public`
    /// also admits free tiers that may train on prompts. A sub-agent run uses
    /// the stricter of this and its caller's class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_class: Option<agent_brain_llm::routing::DataClass>,
    /// Models to step down to, in order, when `model_preference` fails or is
    /// refused — before the global `chat_fallback_ladder`. Catalog names, so a
    /// profile can say "another Gemini first, then Ollama Cloud". Every rung is
    /// still judged by the routing policy and the budget. `/chat` only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    /// Notes this profile's `search_notes` never returns, applied when it runs
    /// as an agent (`run_agent`/`delegate`). See `RetrievalExclude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_exclude: Option<agent_brain_knowledge::knowledge::RetrievalExclude>,
    /// Runnable only when named in a `run_agent` step, never offered to a
    /// model: left out of the `delegate` roster and `run_agent(agent:"auto")`,
    /// and refused by chat `delegate`. For a paid agent (`deep-diver-hard`), so
    /// spending stays a decision a human made by writing the task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub explicit_only: bool,
    /// Whether an item assigned to this agent may run on its own
    /// (`read_only`) or must wait for the user's approval (`writes`). Absent,
    /// it is derived from `tools` and fails safe (`items::side_effects`): an
    /// override is a deliberate, reviewable act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effects: Option<String>,
    /// Prompt parts after `system_prompt`, each kept only when the tools it
    /// `requires` are available (`services/profiles.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<crate::profiles::PromptSection>,
    /// Chat sessions whose id starts with one of these run this profile when
    /// the request names none: a workflow that opens a session for replies
    /// (`notify_user` with a session id) owns the replies, however terse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_prefixes: Vec<String>,
}

/// Runtime bundle produced by [`ContextBuilderService::build_bundle`].
#[derive(Debug, Clone)]
pub struct ContextBundle {
    pub profile: ContextProfile,
    /// Notes fetched via `pre_load_query` (may be empty).
    pub pre_loaded_notes: Vec<String>,
}

/// One step in a boot/init protocol YAML.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolStep {
    Log {
        message: String,
    },
    ToolCall {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    StoreNote {
        content: String,
        note_type: Option<String>,
    },
    Conditional {
        condition: String,
        #[serde(default)]
        then: Vec<ProtocolStep>,
    },
    RunProtocol {
        protocol: String,
    },
}

/// A boot/init protocol file.
#[derive(Debug, Clone, Deserialize)]
pub struct Protocol {
    pub name: String,
    #[serde(default)]
    pub steps: Vec<ProtocolStep>,
}

/// See [`ContextBuilderService::profile_for_session`].
fn session_owner<'a>(
    profiles: impl Iterator<Item = &'a ContextProfile>,
    session: &str,
) -> Option<String> {
    profiles
        .flat_map(|p| p.session_prefixes.iter().map(move |x| (x, &p.name)))
        .filter(|(x, _)| !x.is_empty() && session.starts_with(x.as_str()))
        .max_by(|a, b| a.0.len().cmp(&b.0.len()).then(b.1.cmp(a.1)))
        .map(|(_, n)| n.clone())
}

/// Whether a `pre_load_query` should run as Cypher rather than as a keyword.
///
/// True only for read-shaped queries: it must open with a read clause *and*
/// contain no write clause. Profiles are editable at runtime via the `context`
/// tool, so a pre-load must never be able to mutate the graph.
fn is_read_cypher(query: &str) -> bool {
    let upper = query.trim().to_uppercase();

    const READ_OPENERS: [&str; 6] = ["MATCH", "OPTIONAL", "WITH", "UNWIND", "CALL", "RETURN"];
    let opens_read = READ_OPENERS.iter().any(|kw| {
        upper.starts_with(kw) && upper[kw.len()..].starts_with(|c: char| !c.is_alphanumeric())
    });
    if !opens_read {
        return false;
    }

    const WRITE_CLAUSES: [&str; 8] = [
        "CREATE", "MERGE", "DELETE", "DETACH", "SET", "REMOVE", "DROP", "FOREACH",
    ];
    let has_write = upper
        .split(|c: char| !c.is_alphanumeric())
        .any(|tok| WRITE_CLAUSES.contains(&tok));

    !has_write
}

// ============================================================================
// Service
// ============================================================================

pub struct ContextBuilderService {
    neo4j: Option<Neo4jClient>,
    /// The core bundle's `contexts/`: profiles load from it first, UI saves
    /// write to it, and `boot.yaml` / `init.yaml` run from it.
    pub contexts_dir: PathBuf,
    /// Other loaded bundles' `contexts/`, read after the core's (`plugins.rs`).
    plugin_dirs: Vec<(String, PathBuf)>,
    profiles: Arc<RwLock<HashMap<String, ContextProfile>>>,
    /// Ownership and origin per profile (from the graph).
    meta: Arc<RwLock<HashMap<String, ProfileMeta>>>,
    /// Sections other bundles add to a profile (`extends:` files), by target.
    extensions: Arc<RwLock<Extensions>>,
    /// The registry's tools with their descriptions, once it is built.
    registry: Arc<RwLock<Vec<(String, String)>>>,
    /// What each profile names that does not exist (`audit_profiles`).
    audits: Arc<RwLock<HashMap<String, ProfileAudit>>>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
}

/// The profile used when nothing else selects one.
///
/// Read from `CHAT_DEFAULT_PROFILE`, falling back to `"general"`. This exists so
/// the conversational front door can be swapped (and reverted) with an env var
/// rather than a rebuild — the front-door design is deliberately tool-poor, and
/// which profile plays that role is a deployment decision, not a code constant.
///
/// Also used to bound MCP `tools/list` when `MCP_TOOL_PROFILE` names it: a
/// delegating front door advertises a handful of tools instead of the full
/// registry, which keeps a connecting client's context small.
pub fn default_profile_name() -> String {
    std::env::var("CHAT_DEFAULT_PROFILE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "general".to_string())
}

impl ContextBuilderService {
    pub fn new(
        neo4j: Option<Neo4jClient>,
        contexts_dir: PathBuf,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
    ) -> Self {
        Self {
            neo4j,
            contexts_dir,
            plugin_dirs: Vec::new(),
            profiles: Arc::new(RwLock::new(HashMap::new())),
            meta: Arc::new(RwLock::new(HashMap::new())),
            extensions: Arc::new(RwLock::new(HashMap::new())),
            registry: Arc::new(RwLock::new(Vec::new())),
            audits: Arc::new(RwLock::new(HashMap::new())),
            llm_config,
        }
    }

    /// Also load the profiles in these bundles' `contexts/`, after the core's.
    pub fn with_plugin_dirs(mut self, dirs: Vec<(String, PathBuf)>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    /// Load the profiles: seed the graph from the bundles' `contexts/` and
    /// read it back (with no Neo4j, read the files). Returns how many loaded.
    pub async fn load_profiles(&self) -> anyhow::Result<usize> {
        let bundles = BundleProfiles::read(&self.contexts_dir, &self.plugin_dirs);
        *self.extensions.write().await = bundles.sections_by_target();
        let mut profiles = HashMap::new();
        let mut meta = HashMap::new();
        match &self.neo4j {
            Some(neo4j) => {
                let n = crate::profiles::seed(neo4j, &bundles).await?;
                debug!(written = n, "Seeded profiles from the plugin bundles");
                for (p, m) in crate::profiles::load(neo4j).await? {
                    meta.insert(p.name.clone(), m);
                    profiles.insert(p.name.clone(), p);
                }
            }
            None => {
                for (plugin, path, p) in bundles.profiles {
                    meta.insert(
                        p.name.clone(),
                        ProfileMeta {
                            managed_by: Some("yaml".into()),
                            plugin: Some(plugin),
                            source: Some(path.display().to_string()),
                        },
                    );
                    profiles.insert(p.name.clone(), p);
                }
            }
        }
        let count = profiles.len();
        *self.profiles.write().await = profiles;
        *self.meta.write().await = meta;
        Ok(count)
    }

    /// Give the service the finished registry (name, description), so
    /// prompts render against the tools that exist.
    pub async fn set_registry(&self, tools: Vec<(String, String)>) {
        *self.registry.write().await = tools;
    }

    /// Check every profile against the registry and the `:RemovedTool`
    /// tombstones; log what each names that does not exist. Returns how many
    /// profiles have findings.
    pub async fn audit_profiles(&self) -> usize {
        let registry: Vec<String> = self
            .registry
            .read()
            .await
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let mut removed: HashMap<String, Option<String>> = HashMap::new();
        if let Some(neo4j) = &self.neo4j
            && let Ok(rows) = neo4j
                .execute(neo4rs::query(
                    "MATCH (r:RemovedTool) RETURN r.name AS name, r.replaced_by AS by",
                ))
                .await
        {
            for r in rows {
                if let Ok(n) = r.get::<String>("name") {
                    removed.insert(n, r.get::<String>("by").ok().filter(|s| !s.is_empty()));
                }
            }
        }
        // Tools a plugin owns whose plugin is off (`(:PluginTool)`, kept by the
        // self-model sync): not missing, they come back with the plugin.
        let mut plugin_off: HashMap<String, String> = HashMap::new();
        if let Some(neo4j) = &self.neo4j
            && let Ok(rows) = neo4j
                .execute(neo4rs::query(
                    "MATCH (p:PluginTool) RETURN p.name AS name, p.plugin AS plugin",
                ))
                .await
        {
            let plugins = agent_brain_graph::bundles::Plugins::global();
            for r in rows {
                if let (Ok(n), Ok(p)) = (r.get::<String>("name"), r.get::<String>("plugin"))
                    && !plugins.is_loaded(&p)
                {
                    plugin_off.insert(n, p);
                }
            }
        }
        let ext = self.extensions.read().await.clone();
        let mut audits = HashMap::new();
        for p in self.list_profiles().await {
            let a = crate::profiles::audit(
                &p,
                &registry,
                &removed,
                ext.get(&p.name).map(Vec::as_slice).unwrap_or(&[]),
                &plugin_off,
            );
            if !a.missing_tools.is_empty() || !a.stale_mentions.is_empty() {
                warn!(
                    profile = %p.name,
                    missing_tools = %a.missing_tools.join(", "),
                    stale_mentions = %a.stale_mentions.join(", "),
                    "Profile names tools that do not exist"
                );
            }
            if a != ProfileAudit::default() {
                audits.insert(p.name.clone(), a);
            }
        }
        // A plugin being off is not a finding: counted only what is wrong.
        let n = audits
            .values()
            .filter(|a: &&ProfileAudit| !a.missing_tools.is_empty() || !a.stale_mentions.is_empty())
            .count();
        *self.audits.write().await = audits;
        n
    }

    /// The prompt `name` runs with now (sections filtered, catalogue
    /// generated), without the base prompt or pre-loaded notes.
    pub async fn rendered_prompt(&self, name: &str) -> Option<String> {
        let profile = self.get_profile(name).await?;
        let ext = self
            .extensions
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        Some(crate::profiles::render(
            &profile,
            &self.registry.read().await,
            &ext,
        ))
    }

    /// Ownership, origin, extensions and audit findings for one profile.
    pub async fn profile_info(&self, name: &str) -> serde_json::Value {
        let meta = self
            .meta
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        let audit = self
            .audits
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        let ext: Vec<serde_json::Value> = self
            .extensions
            .read()
            .await
            .get(name)
            .map(|v| {
                v.iter()
                    .map(|(plugin, s)| serde_json::json!({"plugin": plugin, "section": s.name, "requires": s.requires}))
                    .collect()
            })
            .unwrap_or_default();
        serde_json::json!({
            "managed_by": meta.managed_by,
            "plugin": meta.plugin,
            "source": meta.source,
            "extensions": ext,
            "missing_tools": audit.missing_tools,
            "stale_mentions": audit.stale_mentions,
            "plugin_off": audit.plugin_off,
        })
    }

    /// Return a cloned profile by name.
    pub async fn get_profile(&self, name: &str) -> Option<ContextProfile> {
        self.profiles.read().await.get(name).cloned()
    }

    /// The profile that claims `session` by one of its `session_prefixes`
    /// (the longest prefix wins, then the name, so the pick is stable).
    pub async fn profile_for_session(&self, session: &str) -> Option<String> {
        session_owner(self.profiles.read().await.values(), session)
    }

    /// Return all loaded profiles sorted by name.
    pub async fn list_profiles(&self) -> Vec<ContextProfile> {
        let map = self.profiles.read().await;
        let mut profiles: Vec<ContextProfile> = map.values().cloned().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        profiles
    }

    /// Profiles that may be spawned as sub-agents, sorted by name. Backs the
    /// `delegate` roster and the spawnability check, replacing a hardcoded list —
    /// a new worker becomes delegable by adding `spawnable: true` to its YAML.
    pub async fn spawnable_profiles(&self) -> Vec<ContextProfile> {
        self.list_profiles()
            .await
            .into_iter()
            .filter(|p| p.spawnable && !p.explicit_only)
            .collect()
    }

    /// Save a profile from a form or API body. The body is laid over the
    /// stored profile, so a field the form does not send keeps its value
    /// (`explicit_only`, `sections`, `data_class`, …). In the graph the
    /// profile becomes runtime-owned: the seeder leaves it alone until
    /// [`Self::reset_profile`] hands it back to its file. With no Neo4j it is
    /// written to the core bundle's `contexts/`.
    pub async fn save_profile(&self, body: &serde_json::Value) -> anyhow::Result<ContextProfile> {
        let name = body
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            anyhow::bail!("Profile name must not be empty");
        }
        if name == "boot" || name == "init" {
            anyhow::bail!("Cannot overwrite reserved protocol files (boot/init)");
        }
        let stored = self.get_profile(&name).await;
        let profile = crate::profiles::overlay(stored.as_ref(), body)?;
        match &self.neo4j {
            Some(neo4j) => {
                let meta = self
                    .meta
                    .read()
                    .await
                    .get(&name)
                    .cloned()
                    .unwrap_or_default();
                crate::profiles::write(
                    neo4j,
                    &profile,
                    "runtime",
                    meta.plugin.as_deref(),
                    meta.source.as_deref(),
                )
                .await?;
                self.meta.write().await.insert(
                    name.clone(),
                    ProfileMeta {
                        managed_by: Some("runtime".into()),
                        ..meta
                    },
                );
            }
            None => {
                let path = self.contexts_dir.join(format!("{name}.yaml"));
                std::fs::write(&path, serde_yaml::to_string(&profile)?)?;
            }
        }
        self.profiles.write().await.insert(name, profile.clone());
        Ok(profile)
    }

    /// Hand a runtime-owned profile back to its file: rewrite it from the
    /// bundle and mark it yaml-owned again.
    pub async fn reset_profile(&self, name: &str) -> anyhow::Result<ContextProfile> {
        let Some(neo4j) = &self.neo4j else {
            anyhow::bail!("profiles are files when there is no graph; nothing to reset");
        };
        let bundles = BundleProfiles::read(&self.contexts_dir, &self.plugin_dirs);
        let Some((plugin, path, profile)) = bundles
            .profiles
            .into_iter()
            .find(|(_, _, p)| p.name == name)
        else {
            anyhow::bail!("no loaded bundle declares a profile named '{name}'");
        };
        let src = crate::profiles::source_of(&path);
        crate::profiles::write(neo4j, &profile, "yaml", Some(&plugin), Some(&src)).await?;
        self.load_profiles().await?;
        Ok(profile)
    }

    /// Delete a profile. A yaml-owned one is refused: the next start would
    /// seed it again from its file, which is what to edit or remove.
    /// Returns `false` if there was no such profile.
    pub async fn delete_profile(&self, name: &str) -> anyhow::Result<bool> {
        if name == "general" || name == default_profile_name().as_str() {
            anyhow::bail!("Cannot delete the default '{name}' profile");
        }
        if self.get_profile(name).await.is_none() {
            return Ok(false);
        }
        let meta = self
            .meta
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        if meta.managed_by.as_deref() == Some("yaml") {
            anyhow::bail!(
                "'{name}' is defined in {}; deleting it here would come back on restart. Remove the file (or its plugin) instead.",
                meta.source.as_deref().unwrap_or("a plugin bundle")
            );
        }
        match &self.neo4j {
            Some(neo4j) => crate::profiles::delete(neo4j, name).await?,
            None => {
                let path = self.contexts_dir.join(format!("{name}.yaml"));
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        self.profiles.write().await.remove(name);
        self.meta.write().await.remove(name);
        Ok(true)
    }

    /// Build a [`ContextBundle`] for the named profile, optionally fetching pre-load notes.
    pub async fn build_bundle(&self, profile_name: &str) -> anyhow::Result<ContextBundle> {
        let profile = self
            .get_profile(profile_name)
            .await
            .ok_or_else(|| anyhow::anyhow!("Context profile '{}' not found", profile_name))?;

        let mut pre_loaded_notes = Vec::new();

        if let (Some(neo4j), Some(query)) = (&self.neo4j, &profile.pre_load_query) {
            // A profile may pre-load context two ways:
            //   1. Cypher — the query runs verbatim and every `content` column
            //      it returns is injected. Lets a profile inject live self-state
            //      (model catalog, chain inventory) rather than only notes.
            //   2. Plain text — treated as a keyword and matched against note
            //      content, the original behaviour.
            let q = if is_read_cypher(query) {
                neo4rs::query(query.as_str())
            } else {
                neo4rs::query(
                    "MATCH (n:Note) \
                     WHERE toLower(n.content) CONTAINS toLower($q) \
                     RETURN n.content AS content ORDER BY n.updated_at DESC LIMIT 10",
                )
                .param("q", query.as_str())
            };
            match neo4j.execute(q).await {
                Ok(rows) => {
                    for row in rows {
                        if let Ok(content) = row.get::<String>("content") {
                            pre_loaded_notes.push(content);
                        }
                    }
                }
                // A bad pre-load must never break the profile — warn and carry on.
                Err(e) => warn!(
                    profile = %profile_name,
                    error = %e,
                    "pre_load_query failed; continuing without pre-loaded context"
                ),
            }
        }

        // The prompt a turn runs with: system_prompt plus the sections (its
        // own and other bundles') whose tools this deployment has.
        let mut profile = profile;
        let ext = self
            .extensions
            .read()
            .await
            .get(&profile.name)
            .cloned()
            .unwrap_or_default();
        profile.system_prompt =
            crate::profiles::render(&profile, &self.registry.read().await, &ext);

        Ok(ContextBundle {
            profile,
            pre_loaded_notes,
        })
    }

    /// Assign a context profile to a goal using description-based text overlap
    /// (fast path) with an LLM classifier fallback for ambiguous goals.
    /// Returns the best profile name, or [`default_profile_name`] as the default.
    pub async fn auto_assign(&self, goal: &str) -> String {
        let profiles = self.profiles.read().await.clone();
        if profiles.is_empty() {
            return default_profile_name();
        }

        let goal_tokens = Self::tokenize(goal);
        // The default profile is the fallback, never a candidate: matching it
        // explicitly would shadow a real specialist. Read once, not per-profile.
        let default_name = default_profile_name();

        // Score each non-default profile by token overlap with name + description.
        let mut scores: Vec<(usize, String)> = profiles
            .iter()
            .filter(|(n, _)| n.as_str() != default_name && n.as_str() != "general")
            .map(|(name, profile)| {
                let profile_tokens = Self::tokenize(&format!(
                    "{} {}",
                    name.replace('-', " "),
                    profile.description
                ));
                let score = goal_tokens
                    .iter()
                    .filter(|tok| tok.len() > 2 && profile_tokens.contains(*tok))
                    .count();
                (score, name.clone())
            })
            .collect();

        scores.sort_by_key(|b| std::cmp::Reverse(b.0));

        // Clear winner: top score > 0 and strictly better than second-best.
        if let (Some((best_score, best_name)), second_score) = (
            scores.first().cloned(),
            scores.get(1).map(|(s, _)| *s).unwrap_or(0),
        ) && best_score > 0
            && best_score > second_score
        {
            debug!(
                profile = %best_name,
                score = best_score,
                "auto_assign: text-overlap match"
            );
            return best_name;
        }

        // LLM fallback: classify ambiguous or novel goals against profile descriptions.
        if let Some(llm) = self.make_llm().await {
            let profile_list: String = profiles
                .iter()
                .filter(|(n, _)| n.as_str() != default_name && n.as_str() != "general")
                .map(|(name, p)| format!("- {}: {}", name, p.description))
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = format!(
                "You are a goal router. Respond in English only.\n\
                 Given a goal, pick the single most relevant context profile.\n\
                 Profiles:\n{}\n\n\
                 Goal: {}\n\n\
                 Reply with ONLY the profile name exactly as shown (e.g. \"task-manager\"). \
                 If none fit well, reply with \"general\".",
                profile_list, goal
            );

            if let Ok(response) = llm.generate(&prompt).await {
                let chosen = response.text.trim().to_lowercase();
                let chosen = chosen.trim_matches('"').trim_matches('\'').trim();
                if profiles.contains_key(chosen) {
                    debug!(profile = %chosen, "auto_assign: LLM match");
                    return chosen.to_string();
                }
            }
        }

        debug!(goal = %goal, "auto_assign: fallback to default profile");
        default_profile_name()
    }

    /// Tokenize text into a set of lowercase alphanumeric tokens.
    fn tokenize(text: &str) -> HashSet<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// Build an LLM client from the live config, returning None if unavailable.
    async fn make_llm(&self) -> Option<LlmClient> {
        let config = self.llm_config.read().await.clone();
        config.and_then(|c| LlmClient::with_config(c).ok())
    }

    /// Execute a named protocol (boot.yaml / init.yaml) file.
    ///
    /// Protocol errors are logged as warnings and do not abort execution.
    pub async fn run_protocol(
        &self,
        name: &str,
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        neo4j: Option<&Neo4jClient>,
    ) -> anyhow::Result<()> {
        let path = self.contexts_dir.join(format!("{}.yaml", name));
        if !path.exists() {
            debug!(protocol = name, "Protocol file not found — skipping");
            return Ok(());
        }

        let text = std::fs::read_to_string(&path)?;
        let protocol: Protocol = serde_yaml::from_str(&text)?;
        info!(protocol = %protocol.name, steps = protocol.steps.len(), "Running protocol");

        for step in &protocol.steps {
            self.exec_step(step, &tool_handler, neo4j).await;
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Protocol step executor (non-recursive — conditional sub-steps are flat)
    // -------------------------------------------------------------------------

    async fn exec_step(
        &self,
        step: &ProtocolStep,
        tool_handler: &Arc<RwLock<Option<ToolHandler>>>,
        neo4j: Option<&Neo4jClient>,
    ) {
        match step {
            ProtocolStep::Log { message } => {
                info!(protocol_log = %message, "Protocol step: log");
            }
            ProtocolStep::ToolCall { tool, args } => {
                let handler_opt = tool_handler.read().await.clone();
                if let Some(handler) = handler_opt {
                    let result = handler.execute(tool, Some(args.clone())).await;
                    // A failed protocol step must be loud. `boot.yaml` called
                    // `scheduler_control{action:"status"}` — an action that does
                    // not exist — on every startup for months, and the only trace
                    // was this line at debug level while the surrounding `log`
                    // steps cheerfully printed "Scheduler status obtained".
                    if result.is_error == Some(true) {
                        warn!(
                            tool = %tool,
                            error = %protocol_result_text(&result),
                            "Protocol step: tool_call FAILED"
                        );
                    } else {
                        debug!(tool = %tool, "Protocol step: tool_call");
                    }
                } else {
                    warn!(tool = %tool, "Protocol step: tool_call — handler not ready");
                }
            }
            ProtocolStep::StoreNote { content, note_type } => {
                let note_type_val = note_type.as_deref().unwrap_or("episodic");
                let handler_opt = tool_handler.read().await.clone();
                if let Some(handler) = handler_opt {
                    let args = serde_json::json!({
                        "content": content,
                        "note_type": note_type_val
                    });
                    let _ = handler.execute("store_note", Some(args)).await;
                    debug!("Protocol step: store_note");
                }
            }
            ProtocolStep::Conditional { condition, then } => {
                let satisfied = self.eval_condition(condition, neo4j).await;
                if satisfied {
                    // Sub-steps are leaf-only (no nested conditionals) to avoid async recursion.
                    for sub_step in then {
                        self.exec_leaf_step(sub_step, tool_handler, neo4j).await;
                    }
                }
            }
            ProtocolStep::RunProtocol { protocol: sub_name } => {
                // Load and execute the sub-protocol inline (no recursive call to run_protocol).
                let sub_path = self.contexts_dir.join(format!("{sub_name}.yaml"));
                if sub_path.exists() {
                    match std::fs::read_to_string(&sub_path)
                        .map_err(|e| e.to_string())
                        .and_then(|t| {
                            serde_yaml::from_str::<Protocol>(&t).map_err(|e| e.to_string())
                        }) {
                        Ok(sub_proto) => {
                            info!(protocol = %sub_name, steps = sub_proto.steps.len(), "Running sub-protocol");
                            for sub_step in &sub_proto.steps {
                                self.exec_leaf_step(sub_step, tool_handler, neo4j).await;
                            }
                        }
                        Err(e) => {
                            warn!(protocol = %sub_name, error = %e, "Failed to load sub-protocol")
                        }
                    }
                } else {
                    debug!(protocol = %sub_name, "Sub-protocol file not found — skipping");
                }
            }
        }
    }

    /// Execute a leaf protocol step (no recursion into Conditional/RunProtocol).
    async fn exec_leaf_step(
        &self,
        step: &ProtocolStep,
        tool_handler: &Arc<RwLock<Option<ToolHandler>>>,
        _neo4j: Option<&Neo4jClient>,
    ) {
        match step {
            ProtocolStep::Log { message } => {
                info!(protocol_log = %message, "Protocol sub-step: log");
            }
            ProtocolStep::ToolCall { tool, args } => {
                let handler_opt = tool_handler.read().await.clone();
                if let Some(handler) = handler_opt {
                    let result = handler.execute(tool, Some(args.clone())).await;
                    if result.is_error == Some(true) {
                        warn!(
                            tool = %tool,
                            error = %protocol_result_text(&result),
                            "Protocol sub-step: tool_call FAILED"
                        );
                    }
                    debug!(tool = %tool, "Protocol sub-step: tool_call");
                }
            }
            ProtocolStep::StoreNote { content, note_type } => {
                let note_type_val = note_type.as_deref().unwrap_or("episodic");
                let handler_opt = tool_handler.read().await.clone();
                if let Some(handler) = handler_opt {
                    let args = serde_json::json!({
                        "content": content,
                        "note_type": note_type_val
                    });
                    let _ = handler.execute("store_note", Some(args)).await;
                }
            }
            // Nested conditionals and run_protocol inside conditionals not supported.
            ProtocolStep::Conditional { .. } | ProtocolStep::RunProtocol { .. } => {
                warn!("Nested conditionals/run_protocol inside a conditional are not supported");
            }
        }
    }

    async fn eval_condition(&self, condition: &str, neo4j: Option<&Neo4jClient>) -> bool {
        match condition {
            "graph_empty" => {
                if let Some(db) = neo4j {
                    let q = neo4rs::query("MATCH (n:Note) RETURN count(n) AS cnt");
                    let count: i64 = db
                        .execute(q)
                        .await
                        .ok()
                        .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                        .unwrap_or(1); // assume non-empty on error
                    count == 0
                } else {
                    false
                }
            }
            _ => {
                warn!(condition = %condition, "Unknown protocol condition");
                false
            }
        }
    }
}

/// Extract readable error text from a failed protocol tool call, for logging.
fn protocol_result_text(result: &agent_brain_protocol::ToolCallResult) -> String {
    result
        .content
        .first()
        .and_then(|c| {
            if let agent_brain_protocol::Content::Text { text } = c {
                Some(text.chars().take(300).collect::<String>())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "<no error text>".to_string())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_belongs_to_the_profile_with_the_longest_prefix() {
        let p = |name: &str, prefixes: &[&str]| -> ContextProfile {
            serde_json::from_value(serde_json::json!({
                "name": name, "description": "d", "session_prefixes": prefixes,
            }))
            .unwrap()
        };
        let all = [p("a", &["job"]), p("b", &["jobsearch-"]), p("c", &[""])];
        assert_eq!(
            session_owner(all.iter(), "jobsearch-2026-10-11").as_deref(),
            Some("b")
        );
        assert_eq!(session_owner(all.iter(), "jobs").as_deref(), Some("a"));
        // An empty prefix claims nothing.
        assert_eq!(session_owner(all.iter(), "chat-1"), None);
    }

    /// Every shipped context profile (except the boot/init protocol files, which
    /// the loader skips) must deserialize as a `ContextProfile`. The loader only
    /// WARNs on a parse failure and drops the profile, so a malformed one is
    /// otherwise invisible until a persona silently goes missing.
    #[test]
    fn all_shipped_context_profiles_parse() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/contexts");
        let mut count = 0;
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == "boot" || stem == "init" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: Result<ContextProfile, _> = serde_yaml::from_str(&text);
            assert!(
                parsed.is_ok(),
                "context profile {} failed to parse: {:?}",
                path.display(),
                parsed.err()
            );
            count += 1;
        }
        assert!(count > 0, "no context profiles found at {}", dir.display());
    }

    /// The paid deep diver is reachable only by a step that names it: if it
    /// lost `explicit_only`, the orchestrator could delegate to Claude Sonnet
    /// on its own and spend without anyone asking.
    #[test]
    fn the_paid_deep_diver_is_explicit_only() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/contexts/deep-diver-hard.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let profile: ContextProfile = serde_yaml::from_str(&text).unwrap();
        assert!(profile.spawnable && profile.explicit_only);
        assert_eq!(profile.model_preference.as_deref(), Some("claude-sonnet-5"));
    }

    /// The schedule-author drafting specialist must be spawnable — the
    /// orchestrator delegates schedule DRAFTING to it (Phase 3).
    #[test]
    fn schedule_author_is_spawnable() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/contexts/schedule-author.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let profile: ContextProfile = serde_yaml::from_str(&text).unwrap();
        assert!(profile.spawnable, "schedule-author must be spawnable");
        assert!(
            profile.specialty.is_some(),
            "schedule-author needs a specialty for the roster"
        );
    }

    #[test]
    fn read_cypher_is_detected() {
        assert!(is_read_cypher("MATCH (n:Note) RETURN n.content AS content"));
        assert!(is_read_cypher(
            "  match (m:ModelDef) return m.name as content  "
        ));
        assert!(is_read_cypher(
            "OPTIONAL MATCH (t:Todo) RETURN t.title AS content"
        ));
        assert!(is_read_cypher(
            "CALL db.labels() YIELD label RETURN label AS content"
        ));
        assert!(is_read_cypher(
            "UNWIND [1,2] AS x RETURN toString(x) AS content"
        ));
    }

    #[test]
    fn plain_keywords_are_not_cypher() {
        // Free-text pre-loads keep the legacy note-keyword behaviour.
        assert!(!is_read_cypher("supply chain intelligence"));
        assert!(!is_read_cypher(""));
        // Substring-only matches must not trip the opener check.
        assert!(!is_read_cypher("matching notes about withdrawals"));
        assert!(!is_read_cypher("callbacks"));
    }

    #[test]
    fn write_clauses_are_rejected() {
        // A read opener is not enough — any write clause disqualifies the query
        // so a runtime-edited profile can never mutate the graph on pre-load.
        assert!(!is_read_cypher(
            "MATCH (n:Note) SET n.x = 1 RETURN n AS content"
        ));
        assert!(!is_read_cypher("MATCH (n:Note) DETACH DELETE n"));
        assert!(!is_read_cypher(
            "MATCH (n:Note) MERGE (m:Note {id:'x'}) RETURN m AS content"
        ));
        assert!(!is_read_cypher(
            "WITH 1 AS x CREATE (n:Note) RETURN n AS content"
        ));
        assert!(!is_read_cypher(
            "MATCH (n:Note) REMOVE n.x RETURN n AS content"
        ));
    }

    #[test]
    fn write_words_inside_strings_are_conservatively_rejected() {
        // Keyword scanning is deliberately blunt: a false negative degrades to
        // "no pre-load", which is safe. A false positive would allow a write.
        assert!(!is_read_cypher(
            "MATCH (n:Note) WHERE n.c = 'create' RETURN n AS content"
        ));
    }

    /// `CHAT_DEFAULT_PROFILE` selects the fallback profile; absent or blank it
    /// stays `general`. This is the whole mechanism behind swapping the chat
    /// front door without a rebuild, so a regression here silently reverts the
    /// deployment to the old tool-heavy default.
    ///
    /// Serialised with its own env var and restored afterwards — these run in
    /// one process, and a leaked var would change what every later test sees.
    #[test]
    fn default_profile_name_follows_the_env_var() {
        let saved = std::env::var("CHAT_DEFAULT_PROFILE").ok();

        unsafe { std::env::remove_var("CHAT_DEFAULT_PROFILE") };
        assert_eq!(default_profile_name(), "general");

        unsafe { std::env::set_var("CHAT_DEFAULT_PROFILE", "GeneralDefault-Orchrestator") };
        assert_eq!(default_profile_name(), "GeneralDefault-Orchrestator");

        // Blank / whitespace-only must not produce an unresolvable profile name.
        unsafe { std::env::set_var("CHAT_DEFAULT_PROFILE", "   ") };
        assert_eq!(default_profile_name(), "general");

        // Surrounding whitespace is trimmed, not preserved into a lookup key.
        unsafe { std::env::set_var("CHAT_DEFAULT_PROFILE", "  orchestrator  ") };
        assert_eq!(default_profile_name(), "orchestrator");

        match saved {
            Some(v) => unsafe { std::env::set_var("CHAT_DEFAULT_PROFILE", v) },
            None => unsafe { std::env::remove_var("CHAT_DEFAULT_PROFILE") },
        }
    }

    /// The classes decided for the checked-in profiles. The front door went
    /// public on 2026-09-15 to keep a gemini-2.5-flash pin, and back to private
    /// on 2026-09-26 with its move to gemma4:31b-cloud (no_log). Losing or
    /// flipping the field (a hand edit, a UI save) would silently change which
    /// providers may see what is typed into chat.
    #[test]
    fn the_checked_in_profiles_keep_their_decided_data_class() {
        use agent_brain_llm::routing::DataClass;
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/contexts");
        for (file, want) in [
            ("GeneralDefault-Orchrestator.yaml", DataClass::Private),
            ("researcher.yaml", DataClass::Private),
        ] {
            let text = std::fs::read_to_string(dir.join(file)).expect("profile readable");
            let p: ContextProfile = serde_yaml::from_str(&text).expect("profile parses");
            assert_eq!(p.data_class, Some(want), "{file}");
        }
    }

    /// A profile's `fallback_models` must name catalog models: an unknown name
    /// is skipped at runtime with a WARN, so a typo would silently shorten the
    /// ladder exactly when it is needed.
    #[test]
    fn every_profile_fallback_model_is_in_the_catalog() {
        use agent_brain_llm::model_config::ModelCatalog;
        let c = ModelCatalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models.yaml"),
        )
        .expect("models.yaml must parse");
        let known = |name: &str| c.models.iter().any(|(k, m)| k == name || m.model == name);
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/contexts");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("contexts dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let Ok(p) = serde_yaml::from_str::<ContextProfile>(&text) else {
                continue; // boot/init protocols are not profiles
            };
            for m in &p.fallback_models {
                if !known(m) {
                    missing.push(format!("{}: {m}", p.name));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "fallback_models not in models.yaml: {missing:?}"
        );
    }
}
