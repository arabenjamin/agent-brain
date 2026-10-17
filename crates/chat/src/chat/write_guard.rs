//! Per-turn write guard: grounding downgrade and duplicate-write suppression.

use super::*;

/// Tools that constitute *retrieval*: after one of these runs, the turn has
/// consulted something outside the model's own weights.
///
/// The list is deliberately narrow. `read_file`-style tools are retrieval in
/// the literal sense but say nothing about the *world*, and the failure this
/// guards against is a note about an external technology written from prior
/// alone. Graph reads count because "what do we already know" is a legitimate
/// grounding for a `semantic` note that consolidates stored knowledge. Plugins
/// add theirs through `Plugin::retrieval_tools` (media: `ingest_media`).
pub(super) const RETRIEVAL_TOOLS: &[&str] = &[
    "search_web",
    "read_page",
    "search_notes",
    "neo4j_query",
    "http_request",
    "get_search_usage",
    "execute_code",
];

/// Tools whose effects persist beyond the turn. An identical call to one of
/// these twice in a single turn is never intentional — see
/// [`TurnWriteGuard::screen`].
pub const WRITE_TOOLS: &[&str] = &[
    "store_note",
    "create_task",
    "write_workspace_file",
    "notify_user",
    "claim",
    "manage_workflow",
];

/// Appended to a `store_note` result when the guard downgraded its type, so the
/// model is told what was actually stored rather than silently getting its way.
pub(super) const DOWNGRADE_NOTICE: &str = "\n\n[GUARD — NOTE TYPE DOWNGRADED TO `unsourced_synthesis`: \
     this note was written as `semantic` (knowledge the brain established) but no retrieval \
     tool ran in this turn, so nothing outside the model's own prior backs it. It is stored \
     and retrievable, but labelled as unsourced on the way out. Say so in your answer. To \
     store it as `semantic`, search first and write it from what you found.]";

/// Returned in place of a re-executed duplicate write. See [`TurnWriteGuard::screen`].
pub(super) const DUPLICATE_NOTICE: &str = "[GUARD — DUPLICATE WRITE SUPPRESSED: this exact call was \
     already executed earlier in this turn and was not run again. The earlier call succeeded; \
     do not repeat it. Move on, or write the final answer.]";

/// Per-turn state for [`TurnWriteGuard::screen`].
///
/// Two failures observed on 2026-08-31, both from one chat turn, both silent:
///
/// 1. `gpt-oss:120b-cloud` was asked for technical deep-dives on three mesh
///    technologies. It ran **zero** searches (confirmed against the
///    `search_usage` ledger) and wrote all three from prior — including one for
///    "Metastatic", a typo of *Meshtastic* that had been sitting in a Todo since
///    08-11. It invented a Rust crate, an API, a handshake, and a citation, and
///    stored it as `note_type: semantic`, the one type `label_claims`
///    deliberately does *not* mark on retrieval. Fluent invention filed as
///    established knowledge is the worst cell in the table.
/// 2. The same turn wrote 15 notes for 3 topics — 7 near-identical `metastatic`
///    notes, 4 `reticulum`, 4 `tailscale` — as the tool loop re-issued
///    `store_note` on successive iterations.
///
/// Neither is a model-quality problem that a better prompt fixes: the first is a
/// missing precondition and the second is a missing idempotence check. The guard
/// is deliberately **non-blocking** — a downgraded note is still stored and a
/// duplicate still reports success — because a chat turn that refuses to write
/// is worse than one that writes with an honest label, and because this runs on
/// every turn where a false positive would otherwise cost the user real work.
#[derive(Debug, Default)]
pub(super) struct TurnWriteGuard {
    /// Set once any tool in [`RETRIEVAL_TOOLS`] has run this turn.
    pub(super) retrieval_ran: bool,
    /// Fingerprints of write calls already executed this turn.
    pub(super) seen_writes: std::collections::HashSet<u64>,
}

/// Note types exempt from the grounding downgrade.
///
/// Only `semantic` claims to be *knowledge the brain established*; every other
/// type either already carries its provenance (`claim`, `source_record`,
/// `inference`, `unsourced_synthesis`) or is a record of the turn itself
/// (`episodic`, `reflection`, `outcome`), which is legitimately unsourced.
/// `None` is guarded because `store_note` defaults it to `semantic`.
pub(super) fn note_type_needs_grounding(note_type: Option<&str>) -> bool {
    matches!(note_type.map(str::trim), None | Some("") | Some("semantic"))
}

/// Stable fingerprint of a write call, used for duplicate suppression.
pub(super) fn write_fingerprint(tool: &str, args: &Value) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut hasher);
    // Serialising a `Value` is order-stable: serde_json preserves object key
    // order, and both calls come from the same model emitting the same JSON.
    serde_json::to_string(args)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// What the guard decided about one tool call.
#[derive(Debug, PartialEq)]
pub(super) enum Screened {
    /// Execute as normal.
    Pass,
    /// Execute, but with these args, and append this notice to the result.
    Rewrite { args: Value, notice: &'static str },
    /// Do not execute; return this text as the result instead.
    Suppress { notice: &'static str },
}

impl TurnWriteGuard {
    /// Record that `tool` ran, then decide what to do with the *next* call.
    ///
    /// Called once per tool call, before dispatch. Ordering inside is
    /// load-bearing: the duplicate check runs before the downgrade so a repeated
    /// call is suppressed on its original fingerprint rather than on the
    /// rewritten one, which would let the same note through twice — once as
    /// `semantic` and once as `unsourced_synthesis`.
    pub(super) fn screen(&mut self, tool: &str, args: &Value) -> Screened {
        if WRITE_TOOLS.contains(&tool) {
            let fp = write_fingerprint(tool, args);
            if !self.seen_writes.insert(fp) {
                return Screened::Suppress {
                    notice: DUPLICATE_NOTICE,
                };
            }
        }

        if tool == "store_note" && !self.retrieval_ran {
            let note_type = args.get("note_type").and_then(Value::as_str);
            if note_type_needs_grounding(note_type) {
                let mut rewritten = args.clone();
                if let Some(obj) = rewritten.as_object_mut() {
                    obj.insert("note_type".into(), json!("unsourced_synthesis"));
                    return Screened::Rewrite {
                        args: rewritten,
                        notice: DOWNGRADE_NOTICE,
                    };
                }
            }
        }

        Screened::Pass
    }

    /// Mark retrieval as having happened. Called *after* dispatch so a tool
    /// cannot ground the very call that invoked it.
    pub(super) fn observe(&mut self, tool: &str, success: bool) {
        if success
            && (RETRIEVAL_TOOLS.contains(&tool)
                || agent_brain_work::tools::plugin_retrieval_tools().contains(&tool))
        {
            self.retrieval_ran = true;
        }
    }

    /// Record that retrieval happened through a channel `observe` can't see.
    /// A `delegate` call is intercepted in the loop and never reaches
    /// `execute_guarded`, so a worker that searched on the orchestrator's behalf
    /// would otherwise leave `retrieval_ran` false — and a `semantic` note the
    /// orchestrator writes afterward would be wrongly downgraded. The loop calls
    /// this when a delegated sub-agent actually ran a retrieval tool.
    pub(super) fn mark_retrieval(&mut self) {
        self.retrieval_ran = true;
    }
}

/// Execute one tool call through the turn guard, returning `(success, text)`.
///
/// **All four provider loops reach this through [`ChatService::dispatch_tool_call`]**,
/// which is the single place that also intercepts `delegate`. Routing every loop
/// through one dispatcher is deliberate: wiring a guard (or the delegate
/// interception) into one loop and leaving the other three is the "labelled in
/// one path, not the other" failure this codebase has re-learned repeatedly —
/// most recently when claim labelling covered one retrieval path and the
/// unlabelled copy of the same assertion reached the context window anyway.
///
/// A suppressed duplicate reports `success = true`: the identical earlier call
/// did succeed, and reporting failure would invite the model to retry the very
/// write being suppressed.
/// Run a tool and return `(success, model_text, bulk)`.
///
/// `bulk` is output destined for the PERSON and never for the model — the whole
/// rows behind a result the user asked to see. Keeping it out of `model_text`
/// is the point: it makes a tool's context cost independent of how much data it
/// returns. See [`ToolCallResult::bulk`].
pub(super) async fn execute_guarded(
    handler: &Option<ToolHandler>,
    guard: &mut TurnWriteGuard,
    tool_name: &str,
    tool_args: &Value,
) -> (bool, String, Option<String>) {
    let (args_to_send, notice) = match guard.screen(tool_name, tool_args) {
        Screened::Suppress { notice } => {
            warn!(tool = %tool_name, "Duplicate write suppressed within chat turn");
            return (true, notice.to_string(), None);
        }
        Screened::Rewrite { args, notice } => {
            warn!(
                tool = %tool_name,
                "Ungrounded semantic note downgraded to unsourced_synthesis — no retrieval ran this turn"
            );
            (args, Some(notice))
        }
        Screened::Pass => (tool_args.clone(), None),
    };

    let Some(h) = handler else {
        return (false, "No tool handler available".to_string(), None);
    };

    let args = if args_to_send.is_object() {
        Some(args_to_send)
    } else {
        None
    };
    let result = h.execute(tool_name, args).await;
    let is_err = result.is_error.unwrap_or(false);
    let mut text = result
        .content
        .iter()
        .filter_map(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    guard.observe(tool_name, !is_err);

    // Only annotate a write that actually happened — appending the downgrade
    // notice to an error would describe a note that was never stored.
    if let Some(n) = notice
        && !is_err
    {
        text.push_str(n);
    }

    (!is_err, text, result.bulk)
}

#[cfg(test)]
mod write_guard_tests {
    use super::*;

    fn note(note_type: Option<&str>, content: &str) -> Value {
        match note_type {
            Some(t) => json!({ "content": content, "note_type": t }),
            None => json!({ "content": content }),
        }
    }

    // --- grounding ---------------------------------------------------------

    #[test]
    fn ungrounded_semantic_note_is_downgraded() {
        let mut g = TurnWriteGuard::default();
        let args = note(Some("semantic"), "Metastatic is a Rust mesh library.");
        match g.screen("store_note", &args) {
            Screened::Rewrite { args, notice } => {
                assert_eq!(args["note_type"], "unsourced_synthesis");
                // Content must survive untouched — the type is the only repair.
                assert_eq!(args["content"], "Metastatic is a Rust mesh library.");
                assert!(notice.contains("unsourced_synthesis"));
            }
            other => panic!("expected downgrade, got {other:?}"),
        }
    }

    #[test]
    fn missing_note_type_is_downgraded_because_it_defaults_to_semantic() {
        let mut g = TurnWriteGuard::default();
        assert!(matches!(
            g.screen("store_note", &note(None, "x")),
            Screened::Rewrite { .. }
        ));
    }

    #[test]
    fn retrieval_earlier_in_the_turn_grounds_a_semantic_note() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", true);
        assert_eq!(
            g.screen("store_note", &note(Some("semantic"), "x")),
            Screened::Pass
        );
    }

    /// A successful `delegate` grounds the orchestrator the same way a direct
    /// retrieval tool does — the worker searched on its behalf. Without this, a
    /// semantic note written after delegating would be downgraded even though
    /// the turn genuinely consulted the world.
    #[test]
    fn a_delegate_retrieval_grounds_a_semantic_note() {
        let mut g = TurnWriteGuard::default();
        // `delegate` is intercepted in the loop, so the guard is told directly
        // rather than through `observe`.
        g.mark_retrieval();
        assert_eq!(
            g.screen("store_note", &note(Some("semantic"), "x")),
            Screened::Pass
        );
    }

    #[test]
    fn a_failed_retrieval_does_not_ground_anything() {
        // The 2026-08-18 SearXNG outage (since removed) returned well-formed empty results for
        // three days. A retrieval that did not retrieve must not license a
        // note claiming to be established knowledge.
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", false);
        assert!(matches!(
            g.screen("store_note", &note(Some("semantic"), "x")),
            Screened::Rewrite { .. }
        ));
    }

    #[test]
    fn non_semantic_types_are_left_alone() {
        // Every other type either carries its own provenance or is a record of
        // the turn itself. Notably `episodic`: run() writes one per chat turn.
        for t in [
            "episodic",
            "reflection",
            "source_record",
            "claim",
            "inference",
            "outcome",
            "unsourced_synthesis",
        ] {
            let mut g = TurnWriteGuard::default();
            assert_eq!(
                g.screen("store_note", &note(Some(t), "x")),
                Screened::Pass,
                "type {t} should not be downgraded"
            );
        }
    }

    #[test]
    fn only_store_note_is_grounded() {
        let mut g = TurnWriteGuard::default();
        assert_eq!(
            g.screen("create_task", &json!({ "goal": "do a thing" })),
            Screened::Pass
        );
    }

    // --- duplicate suppression --------------------------------------------

    #[test]
    fn an_identical_write_runs_once() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", true);
        let args = note(Some("semantic"), "same body");
        assert_eq!(g.screen("store_note", &args), Screened::Pass);
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Suppress { .. }
        ));
    }

    #[test]
    fn differing_content_is_not_a_duplicate() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_notes", true);
        assert_eq!(g.screen("store_note", &note(None, "a")), Screened::Pass);
        assert_eq!(g.screen("store_note", &note(None, "b")), Screened::Pass);
    }

    #[test]
    fn reads_are_never_deduped() {
        // Searching the same phrase twice is wasteful, not incorrect, and
        // suppressing it would hand the model a fabricated tool result.
        let mut g = TurnWriteGuard::default();
        let args = json!({ "query": "reticulum" });
        assert_eq!(g.screen("search_web", &args), Screened::Pass);
        assert_eq!(g.screen("search_web", &args), Screened::Pass);
    }

    #[test]
    fn dedup_runs_before_downgrade_so_one_note_cannot_land_under_two_types() {
        // Ordering guard: if the downgrade ran first, the second call would
        // fingerprint the *rewritten* args, miss the original, and store the
        // same content twice — once semantic, once unsourced_synthesis.
        let mut g = TurnWriteGuard::default();
        let args = note(Some("semantic"), "body");
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Rewrite { .. }
        ));
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Suppress { .. }
        ));
    }

    #[test]
    fn the_regression_case_writes_three_notes_not_fifteen() {
        // 2026-08-31: one turn wrote 7 metastatic + 4 reticulum + 4 tailscale
        // notes, all as `semantic`, with zero searches in the whole turn.
        let mut g = TurnWriteGuard::default();
        let topics = [("metastatic", 7), ("reticulum", 4), ("tailscale", 4)];
        let mut executed = 0;
        let mut downgraded = 0;
        for (topic, repeats) in topics {
            for _ in 0..repeats {
                match g.screen("store_note", &note(Some("semantic"), topic)) {
                    Screened::Rewrite { .. } => {
                        executed += 1;
                        downgraded += 1;
                    }
                    Screened::Pass => executed += 1,
                    Screened::Suppress { .. } => {}
                }
            }
        }
        assert_eq!(executed, 3, "one write per distinct note");
        assert_eq!(downgraded, 3, "all three ungrounded");
    }
}
