//! Knowledge Skill - Provides tools for managing notes and memories.
//!
//! This file holds the tool dispatch, `prune_old_notes` and `consolidate_memories`.
//! `knowledge/` has a file per concern: `store`, `recall`, `reason`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;

use agent_brain_knowledge::KnowledgeStore;
use agent_brain_llm::LlmProvider;
use agent_brain_protocol::Skill;
use agent_brain_protocol::tool_schema;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Knowledge Skill implementation.
pub struct KnowledgeSkill {
    svc: Arc<dyn KnowledgeStore>,
    llm: Arc<dyn LlmProvider>,
}

impl KnowledgeSkill {
    /// Create a new knowledge skill.
    pub fn new(svc: Arc<dyn KnowledgeStore>, llm: Arc<dyn LlmProvider>) -> Self {
        Self { svc, llm }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    // export_graph_visualization is served by GET /api/graph (REST API)

    fn prune_old_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "prune_old_notes".to_string(),
            description:
                "Delete stale notes. Use score_threshold/lambda for adaptive decay scoring \
                         (recommended), or days_stale/min_accesses for simple time-based pruning. \
                         Protected types (consolidated, reflection) are never deleted. \
                         min_retain (default 50) and max_pct (default 0.30) prevent over-pruning."
                    .to_string(),
            input_schema: tool_schema!(core "prune_old_notes"),
        }
    }

    fn consolidate_memories_def() -> ToolDefinition {
        ToolDefinition {
            name: "consolidate_memories".to_string(),
            description: "Use the LLM to synthesize multiple notes on a topic into a single \
                         consolidated summary note. Creates SUMMARIZED_BY edges from source \
                         notes to the consolidated note. Requires LLM."
                .to_string(),
            input_schema: tool_schema!(core "consolidate_memories"),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_prune_old_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: PruneOldNotesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(
            days_stale = input.days_stale,
            min_accesses = input.min_accesses,
            dry_run = input.dry_run,
            "Pruning stale notes"
        );

        match self
            .svc
            .prune_old_notes(
                input.days_stale,
                input.min_accesses,
                input.score_threshold,
                input.lambda,
                input.dry_run,
                input.min_retain,
                input.max_pct,
            )
            .await
        {
            Ok(count) => {
                let response = if input.dry_run {
                    json!({
                        "count": count,
                        "dry_run": true,
                        "message": format!("Would delete {} stale note(s) (dry run)", count)
                    })
                } else {
                    json!({
                        "count": count,
                        "message": format!("Deleted {} stale note(s)", count)
                    })
                };
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to prune notes: {e}")),
        }
    }

    async fn handle_consolidate_memories(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ConsolidateMemoriesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Consolidating memories");

        match self
            .svc
            .consolidate_memories(&input.topic, input.limit)
            .await
        {
            Ok((id, source_count, preview)) => {
                let response = json!({
                    "id": id,
                    "source_count": source_count,
                    "preview": preview
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Consolidation failed: {e}")),
        }
    }
}

#[async_trait]
impl Skill for KnowledgeSkill {
    fn name(&self) -> &str {
        "Knowledge Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::store_note_def(),
            Self::search_notes_def(),
            Self::recall_recent_def(),
            Self::prune_old_notes_def(),
            Self::consolidate_memories_def(),
            Self::synthesize_knowledge_def(),
            Self::reason_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "store_note" => Some(self.handle_store_note(arguments).await),
            "search_notes" => Some(self.handle_search_notes(arguments).await),
            "recall_recent" => Some(self.handle_recall_recent(arguments).await),
            "prune_old_notes" => Some(self.handle_prune_old_notes(arguments).await),
            "consolidate_memories" => Some(self.handle_consolidate_memories(arguments).await),
            "synthesize_knowledge" => Some(self.handle_synthesize_knowledge(arguments).await),
            "reason" => Some(self.handle_reason(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

/// Marker `run_agent` puts on the first line of an answer that failed its
/// caller's `verify` requirements.
pub use agent_brain_chat::agent_runner::UNVERIFIED_MARKER;

mod reason;
mod recall;
mod store;
pub use store::*;

#[derive(Debug, Deserialize)]
struct PruneOldNotesInput {
    #[serde(default = "default_days_stale")]
    days_stale: i64,
    #[serde(default = "default_min_accesses")]
    min_accesses: i64,
    #[serde(default)]
    score_threshold: Option<f64>,
    #[serde(default)]
    lambda: Option<f64>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default = "default_min_retain")]
    min_retain: i64,
    #[serde(default = "default_max_prune_pct")]
    max_pct: f64,
}

#[derive(Debug, Deserialize)]
struct ConsolidateMemoriesInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

fn default_days_stale() -> i64 {
    30
}
fn default_min_accesses() -> i64 {
    2
}
fn default_min_retain() -> i64 {
    50
}
fn default_max_prune_pct() -> f64 {
    0.30
}
fn default_consolidate_limit() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use std::sync::Arc;

    fn skill_ok() -> KnowledgeSkill {
        KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("{}")),
        )
    }

    // -- null-result notes ----------------------------------------------------

    #[tokio::test]
    async fn store_note_reports_the_episodic_type_for_a_null_result() {
        let skill = skill_ok();
        let args =
            json!({"content": "## Research: x\n\nNO FINDINGS — nothing.", "note_type": "semantic"});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["note_type"], "episodic");
    }

    #[tokio::test]
    async fn a_half_location_stores_nothing_and_a_store_without_locations_says_so() {
        let skill = skill_ok();
        let r = skill
            .execute(
                "store_note",
                Some(json!({"content": "ship sighted", "lat": 26.5})),
            )
            .await
            .unwrap();
        assert!(r.is_error.unwrap_or(false), "{r:?}");
        assert!(format!("{r:?}").contains("Not stored") && format!("{r:?}").contains("both"));
        // The mock keeps no locations: the note is stored and the response
        // says its point was not, rather than dropping it quietly.
        let args = json!({"content": "ship sighted", "lat": 26.5, "lon": 56.2});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["success"], true);
        assert!(
            r["location"]["error"]
                .as_str()
                .unwrap()
                .contains("keeps no locations"),
            "{r}"
        );
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill_ok().tools().len(), 7);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let skill = skill_ok();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(skill.execute("nonexistent_tool", None));
        assert!(result.is_none());
    }

    // -- recall_recent --------------------------------------------------------

    #[tokio::test]
    async fn recall_recent_defaults_to_today_and_echoes_window() {
        let skill = skill_ok();
        // No args → window defaults to "today"; the mock echoes since/until.
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(r["window"], "today");
        assert!(r["since"].as_str().is_some(), "since bound present");
        assert!(r["until"].as_str().is_some(), "until bound present");
        // "since" (local midnight, UTC) must not be after "until" (now, UTC).
        assert!(r["since"].as_str().unwrap() <= r["until"].as_str().unwrap());
    }

    #[tokio::test]
    async fn recall_recent_accepts_n_day_window() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({"window": "7d"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["window"], "7d");
        assert_eq!(r["total_notes"], 0); // mock store is empty
    }

    // The drill-down half: a bucket reported in `by_source_context` has to be
    // expandable by name. Without this, a caller handed "video_learning: 43" can
    // only reach for search_notes, which ranks by topic and reports absence for a
    // label — the 2026-09-11 "no videos today" false negative on a day with 11.
    #[tokio::test]
    async fn recall_recent_forwards_drill_down_filters() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({
                        "source_context": "video_learning",
                        "note_type": "source_record"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["filter"]["source_context"], "video_learning");
        assert_eq!(r["filter"]["note_type"], "source_record");
    }

    // An omitted filter passed as "" must be absent, not a constraint for "" —
    // otherwise the drill-down silently matches nothing and looks like absence
    // again, one layer down.
    #[tokio::test]
    async fn recall_recent_treats_empty_filter_as_absent() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({"source_context": "", "note_type": "  "})),
                )
                .await
                .unwrap(),
        );
        assert!(r["filter"]["source_context"].is_null());
        assert!(r["filter"]["note_type"].is_null());
    }

    // -- search_notes enumeration ---------------------------------------------

    /// A query-less enumeration that matches nothing must name the labels that
    /// DO exist, exactly as `recall_recent` does. Without it the caller sees a
    /// bare `count: 0` and cannot tell a wrong label from an empty graph — on
    /// 2026-09-11 a delegated worker probed note_type "agent", source_context
    /// "agent", then note_type "news", learned nothing from any miss, burned
    /// all 8 of its rounds and reported it could not determine the answer.
    #[tokio::test]
    async fn empty_enumeration_names_the_labels_that_exist() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("search_notes", Some(json!({"note_type": "agent"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["count"], 0);
        // The escape hatch: totals plus both label inventories.
        assert_eq!(r["graph_total_notes"], 250);
        assert!(r["available_note_types"].is_array());
        assert!(r["available_source_contexts"].is_array());
        let note = r["note"].as_str().expect("advisory note present");
        assert!(
            note.contains("NOT evidence"),
            "must reject the absence reading"
        );
        assert!(note.contains("250"), "must quote the real total");
    }

    /// The hint is for a zero — a successful enumeration must not be polluted
    /// with it, or every drill-down carries a "you may be wrong" banner.
    #[tokio::test]
    async fn non_empty_enumeration_carries_no_absence_hint() {
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore {
                search_result: Ok(vec![json!({"id": "n1", "content": "a"})]),
                ..Default::default()
            }),
            Arc::new(MockLlm::ok("{}")),
        );
        let r = result_json(
            skill
                .execute("search_notes", Some(json!({"note_type": "source_record"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["count"], 1);
        assert!(r["note"].is_null());
        assert!(r["available_note_types"].is_null());
    }

    // -- recall_recent paging -------------------------------------------------

    /// `offset` must reach the service, or "show me the rest" silently re-reads
    /// page one forever.
    #[tokio::test]
    async fn recall_recent_forwards_offset() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({"source_context": "video_learning", "offset": 50})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["offset"], 50);
    }

    /// Absent offset is page one, not an error.
    #[tokio::test]
    async fn recall_recent_defaults_offset_to_zero() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(r["offset"], 0);
    }

    // -- full delivery (the bulk channel) -------------------------------------

    /// `full: true` must reach the service, or "show me everything" silently
    /// returns a model-sized page instead of a delivery.
    #[tokio::test]
    async fn recall_recent_forwards_full() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({"full": true})))
                .await
                .unwrap(),
        );
        assert_eq!(r["full"], true);
    }

    // -- store_note -----------------------------------------------------------

    #[tokio::test]
    async fn store_note_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"content": "hello world"});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["id"], "note-id-1");
        assert_eq!(r["links_created"], 2);
    }

    #[tokio::test]
    async fn store_note_missing_content_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("store_note", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn store_note_propagates_store_error() {
        let store = MockKnowledgeStore {
            store_result: Err("db down".into()),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"content": "test"});
        let msg = result_error(skill.execute("store_note", Some(args)).await.unwrap());
        assert!(msg.contains("db down"));
    }

    // -- search_notes ---------------------------------------------------------

    #[tokio::test]
    async fn search_notes_success() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n1", "content": "c1"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "something"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_with_entity_expansion() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n1"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "rust", "entity_expansion": true});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_empty_query_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({"query": ""})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn search_notes_no_query_or_entity_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    // -- search_notes entity_name path ----------------------------------------

    #[tokio::test]
    async fn search_notes_by_entity_name_success() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n2"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"entity_name": "Rust"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
        assert_eq!(r["entity_name"], "Rust");
    }

    /// An empty/whitespace `entity_name` must be treated as absent, not searched
    /// — otherwise a model looping `search_notes` with `entity_name: ""` gets
    /// noise back and keeps looping (measured 2026-09-07 burning a sub-agent's
    /// budget). With nothing else supplied it must error, pointing the model at
    /// a real query rather than returning a successful empty search.
    #[tokio::test]
    async fn search_notes_empty_entity_name_is_not_searched() {
        let skill = skill_ok();
        for v in ["", "   "] {
            let r = skill
                .execute("search_notes", Some(serde_json::json!({"entity_name": v})))
                .await
                .unwrap();
            assert_eq!(
                r.is_error,
                Some(true),
                "entity_name {v:?} should error, not search"
            );
        }
    }

    /// But an empty `entity_name` alongside a real `query` falls through to the
    /// query search rather than erroring — the empty entity is simply ignored.
    #[tokio::test]
    async fn empty_entity_name_with_a_real_query_falls_through_to_query() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n3"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"entity_name": "", "query": "reticulum"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    /// An empty/whitespace `note_type` must be treated as absent, not a filter
    /// for `note_type = ""`. Regression: a delegated worker passed note_type:""
    /// alongside a query, the real store filtered for type "" and returned
    /// nothing, and the worker reported "no information" on data that existed
    /// (2026-09-07). With no query either, an empty note_type leaves nothing to
    /// search, so it must error rather than list notes of type "".
    #[tokio::test]
    async fn empty_note_type_is_not_treated_as_a_filter() {
        let skill = skill_ok();
        for v in ["", "   "] {
            let r = skill
                .execute("search_notes", Some(serde_json::json!({"note_type": v})))
                .await
                .unwrap();
            assert_eq!(
                r.is_error,
                Some(true),
                "note_type {v:?} must be ignored, not listed as a type"
            );
        }
    }

    // -- prune_old_notes ------------------------------------------------------

    #[tokio::test]
    async fn prune_old_notes_dry_run() {
        let skill = skill_ok();
        let args = serde_json::json!({"dry_run": true});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["dry_run"], true);
        assert_eq!(r["count"], 3);
    }

    #[tokio::test]
    async fn prune_old_notes_actual() {
        let skill = skill_ok();
        let args = serde_json::json!({"days_stale": 14});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 3);
        assert!(r.get("dry_run").is_none());
    }

    // -- consolidate_memories -------------------------------------------------

    #[tokio::test]
    async fn consolidate_memories_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "rust async"});
        let r = result_json(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["id"], "consolidated-id");
        assert_eq!(r["source_count"], 5);
    }

    #[tokio::test]
    async fn consolidate_memories_propagates_error() {
        let store = MockKnowledgeStore {
            consolidate_result: Err("consolidation failed".into()),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"topic": "test"});
        let msg = result_error(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert!(msg.contains("consolidation failed"));
    }

    // -- synthesize_knowledge -------------------------------------------------

    #[tokio::test]
    async fn synthesize_knowledge_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "Rust async"});
        let r = result_json(
            skill
                .execute("synthesize_knowledge", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["note_id"], "synth-id");
        assert_eq!(r["success"], true);
    }

    // -- reason ---------------------------------------------------------------

    #[tokio::test]
    async fn reason_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"question": "What is async?"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["answer"], "answer text");
        assert!((r["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-9);
    }

    // -- reason action=audit --------------------------------------------------

    #[tokio::test]
    async fn reason_audit_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "audit", "question": "deploy to prod"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["aligned"], true);
    }

    // -- reason action=explain ------------------------------------------------

    #[tokio::test]
    async fn reason_explain_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "explain", "question": "chose Rust"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["explanation"], "explanation text");
    }

    // -- reason action=clarify ------------------------------------------------

    #[tokio::test]
    async fn reason_clarify_parses_llm_json() {
        let llm_resp = r#"{"needs_clarification": true, "ambiguities": ["scope"], "clarifying_questions": ["Which env?"], "assumptions": [], "recommended_approach": "ask first"}"#;
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok(llm_resp)),
        );
        let args = serde_json::json!({"action": "clarify", "question": "deploy the thing"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["needs_clarification"], true);
    }

    #[tokio::test]
    async fn reason_clarify_llm_error_returns_error() {
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::err("llm unavailable")),
        );
        let args = serde_json::json!({"action": "clarify", "question": "do something"});
        let msg = result_error(skill.execute("reason", Some(args)).await.unwrap());
        assert!(msg.contains("llm unavailable"));
    }
}
