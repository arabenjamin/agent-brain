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

    /// Write a profile to `contexts_dir/{name}.yaml` and update the in-memory map.
    pub async fn save_profile(&self, profile: ContextProfile) -> anyhow::Result<()> {
        if profile.name == "boot" || profile.name == "init" {
            anyhow::bail!("Cannot overwrite reserved protocol files (boot/init)");
        }
        if profile.name.is_empty() {
            anyhow::bail!("Profile name must not be empty");
        }
        let path = self.contexts_dir.join(format!("{}.yaml", profile.name));
        let text = serde_yaml::to_string(&profile)
            .map_err(|e| anyhow::anyhow!("Failed to serialize profile: {}", e))?;
        std::fs::write(&path, text)?;
        let mut map = self.profiles.write().await;
        map.insert(profile.name.clone(), profile);
        Ok(())
    }

    /// Delete `contexts_dir/{name}.yaml` and remove from the in-memory map.
    /// Returns `false` if the profile was not found.
    pub async fn delete_profile(&self, name: &str) -> anyhow::Result<bool> {
        if name == "general" {
            anyhow::bail!("Cannot delete the default 'general' profile");
        }
        let path = self.contexts_dir.join(format!("{name}.yaml"));
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path)?;
        }
        let mut map = self.profiles.write().await;
        let was_in_memory = map.remove(name).is_some();
        Ok(existed || was_in_memory)
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
}
