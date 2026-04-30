//! Context profile loader and bundle builder.
//!
//! Loads YAML context profiles from a directory, auto-assigns profiles to goals
//! via keyword matching, and builds `ContextBundle` objects that include a
//! filtered tool list plus any pre-loaded notes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::mcp::tools::ToolHandler;
use crate::repository::Neo4jClient;
use crate::services::llm::{LlmClient, LlmConfig};

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

// ============================================================================
// Service
// ============================================================================

pub struct ContextBuilderService {
    neo4j: Option<Neo4jClient>,
    pub contexts_dir: PathBuf,
    profiles: Arc<RwLock<HashMap<String, ContextProfile>>>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
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
            profiles: Arc::new(RwLock::new(HashMap::new())),
            llm_config,
        }
    }

    /// Read all `*.yaml` files from `contexts_dir` (excluding boot.yaml and init.yaml).
    /// Returns the number of profiles loaded.
    pub async fn load_profiles(&self) -> anyhow::Result<usize> {
        let dir = &self.contexts_dir;
        if !dir.exists() {
            warn!(path = %dir.display(), "contexts_dir does not exist — skipping profile load");
            return Ok(0);
        }

        let mut map = self.profiles.write().await;
        map.clear();

        let rd = std::fs::read_dir(dir)?;
        let mut count = 0usize;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            // boot.yaml and init.yaml are protocol files, not profiles.
            if stem == "boot" || stem == "init" {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_yaml::from_str::<ContextProfile>(&text) {
                    Ok(profile) => {
                        debug!(name = %profile.name, "Loaded context profile");
                        map.insert(profile.name.clone(), profile);
                        count += 1;
                    }
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to parse context profile")
                    }
                },
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to read context profile")
                }
            }
        }
        Ok(count)
    }

    /// Return a cloned profile by name.
    pub async fn get_profile(&self, name: &str) -> Option<ContextProfile> {
        self.profiles.read().await.get(name).cloned()
    }

    /// Return all loaded profiles sorted by name.
    pub async fn list_profiles(&self) -> Vec<ContextProfile> {
        let map = self.profiles.read().await;
        let mut profiles: Vec<ContextProfile> = map.values().cloned().collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        profiles
    }

    /// Build a [`ContextBundle`] for the named profile, optionally fetching pre-load notes.
    pub async fn build_bundle(&self, profile_name: &str) -> anyhow::Result<ContextBundle> {
        let profile = self
            .get_profile(profile_name)
            .await
            .ok_or_else(|| anyhow::anyhow!("Context profile '{}' not found", profile_name))?;

        let mut pre_loaded_notes = Vec::new();

        if let (Some(neo4j), Some(query)) = (&self.neo4j, &profile.pre_load_query) {
            // Simple keyword search via Neo4j full-text.
            let q = neo4rs::query(
                "MATCH (n:Note) \
                 WHERE toLower(n.content) CONTAINS toLower($q) \
                 RETURN n.content AS content ORDER BY n.updated_at DESC LIMIT 10",
            )
            .param("q", query.as_str());
            if let Ok(rows) = neo4j.execute(q).await {
                for row in rows {
                    if let Ok(content) = row.get::<String>("content") {
                        pre_loaded_notes.push(content);
                    }
                }
            }
        }

        Ok(ContextBundle {
            profile,
            pre_loaded_notes,
        })
    }

    /// Assign a context profile to a goal using description-based text overlap
    /// (fast path) with an LLM classifier fallback for ambiguous goals.
    /// Returns the best profile name, or `"general"` as the default.
    pub async fn auto_assign(&self, goal: &str) -> String {
        let profiles = self.profiles.read().await.clone();
        if profiles.is_empty() {
            return "general".to_string();
        }

        let goal_tokens = Self::tokenize(goal);

        // Score each non-general profile by token overlap with name + description.
        let mut scores: Vec<(usize, String)> = profiles
            .iter()
            .filter(|(n, _)| *n != "general")
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
                .filter(|(n, _)| *n != "general")
                .map(|(name, p)| format!("- {}: {}", name, p.description))
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = format!(
                "You are a goal router. Given a goal, pick the single most relevant context profile.\n\
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

        debug!(goal = %goal, "auto_assign: fallback to general");
        "general".to_string()
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
                    debug!(tool = %tool, is_error = ?result.is_error, "Protocol step: tool_call");
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
                    let _ = handler.execute(tool, Some(args.clone())).await;
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
