//! One-off repair for notes that stored a query *envelope* instead of its content.
//!
//! Chains bank intermediate output in a `WorkingMemory` session and reassemble it
//! with `neo4j_query … RETURN w.content AS content ORDER BY w.turn_index`, because
//! `{{_prev}}` carries only the previous step's output. Until 2026-08-10,
//! `extract_result_text` passed the whole result envelope onward, so `store_note`
//! persisted notes shaped like:
//!
//! ```text
//! # Video learning: watch video: https://…
//!
//! {"count": 2, "rows": [{"content": "## VIDEO SUMMARY\n{…escaped…}"}, …]}
//! ```
//!
//! The real content was there, escaped inside JSON scaffolding — and embedded that
//! way, so retrieval matched against the scaffolding. Every `video_learning` note
//! written between 2026-08-04 and 2026-08-10 is affected (217 parents, 649 chunks).
//!
//! The fix in `extract_result_text` stops new pollution; this repairs the existing
//! notes **in place**. In-place matters: the parents carry `SUMMARIZED_BY` and
//! `DERIVED_FROM` edges from consolidations and inference notes, and delete-and-
//! recreate would orphan those and reset every `created_at`, which also drives
//! spaced repetition.
//!
//! Deliberately content-preserving: nothing is filtered or dropped on the basis of
//! what a note *says*. Contested or fringe material stays, so it can later be
//! fact-checked and labelled rather than silently excluded.
//!
//! Idempotent — selection is keyed on the envelope still being present, so an
//! interrupted run can simply be repeated.

use anyhow::Result;
use tracing::{info, warn};

use crate::repository::Neo4jClient;
use crate::services::knowledge::KnowledgeService;

/// What a repair run did (or, in dry-run mode, would do).
#[derive(Debug, Default)]
pub struct RepairStats {
    pub examined: usize,
    pub repaired: usize,
    pub skipped_unparseable: usize,
    pub chunks_removed: usize,
    pub chunks_created: usize,
}

/// Split a polluted note into its header and the JSON envelope that follows it.
///
/// The envelope is a *suffix*: `store_note` templates put it after a heading
/// (`# Video learning: {{goal}}\n\n{{_prev}}`), so a whole-document JSON parse —
/// which is what `extract_result_text` does — does not apply here.
///
/// Returns `(header, unwrapped_content)`, or `None` when the note does not have
/// the expected shape, so an unrecognised note is left untouched rather than
/// mangled.
pub fn repair_content(content: &str) -> Option<(String, String)> {
    // The envelope starts at the first `{` that begins a JSON object with a
    // "rows" key. Scanning for the first `{` alone would trip over prose braces.
    let start = content.find("{\n  \"count\"").or_else(|| {
        content
            .match_indices('{')
            .find(|(i, _)| content[*i..].starts_with("{\"count\""))
            .map(|(i, _)| i)
    })?;

    let (header, envelope) = content.split_at(start);
    let parsed: serde_json::Value = serde_json::from_str(envelope).ok()?;
    let unwrapped = crate::services::queue::unwrap_single_column_rows(&parsed)?;
    Some((header.to_string(), unwrapped))
}

/// Repair every note still carrying a reassembly envelope.
///
/// For each parent: rewrite content, recompute the embedding, drop the stale
/// `RELATES_TO` edges (they were computed from the polluted vector, so they encode
/// similarity between pieces of JSON scaffolding) and re-link, regenerate chunks,
/// and re-extract entities. `id`, `created_at`, `SUMMARIZED_BY` and `DERIVED_FROM`
/// are all preserved.
pub async fn repair_envelope_notes(
    neo4j: &Neo4jClient,
    knowledge: &KnowledgeService,
    dry_run: bool,
) -> Result<RepairStats> {
    let mut stats = RepairStats::default();

    // Parents only — chunks are regenerated from the repaired parent, so
    // repairing them independently would be wasted work.
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.content CONTAINS '\"rows\":' AND n.content CONTAINS '\"count\":' \
               AND NOT (n)-[:PART_OF]->() \
             RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                    n.source_context AS source_context \
             ORDER BY n.created_at",
        ))
        .await?;

    info!(candidates = rows.len(), dry_run, "Envelope repair starting");

    for row in &rows {
        stats.examined += 1;
        let Ok(id) = row.get::<String>("id") else {
            continue;
        };
        let Ok(content) = row.get::<String>("content") else {
            continue;
        };
        let note_type = row.get::<String>("note_type").ok();
        let source_context = row.get::<String>("source_context").ok();

        let Some((header, unwrapped)) = repair_content(&content) else {
            stats.skipped_unparseable += 1;
            warn!(note_id = %id, "Note does not match the envelope shape — left untouched");
            continue;
        };
        let repaired = format!("{header}{unwrapped}");

        if dry_run {
            info!(
                note_id = %id,
                before = content.chars().take(90).collect::<String>(),
                after = repaired.chars().take(90).collect::<String>(),
                "Would repair"
            );
            stats.repaired += 1;
            continue;
        }

        match knowledge
            .rewrite_note_in_place(
                &id,
                &repaired,
                note_type.as_deref(),
                source_context.as_deref(),
            )
            .await
        {
            Ok((removed, created)) => {
                stats.repaired += 1;
                stats.chunks_removed += removed;
                stats.chunks_created += created;
                info!(
                    note_id = %id,
                    chunks_removed = removed,
                    chunks_created = created,
                    progress = format!("{}/{}", stats.repaired, rows.len()),
                    "Repaired note"
                );
            }
            Err(e) => {
                warn!(note_id = %id, error = %e, "Repair failed — note left as-is");
            }
        }
    }

    info!(
        examined = stats.examined,
        repaired = stats.repaired,
        skipped = stats.skipped_unparseable,
        chunks_removed = stats.chunks_removed,
        chunks_created = stats.chunks_created,
        dry_run,
        "Envelope repair complete"
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_a_header_plus_envelope_note() {
        let content = "# Video learning: watch video: https://x\n\n{\n  \"count\": 2,\n  \"rows\": [\n    {\n      \"content\": \"## VIDEO SUMMARY\\nbody\"\n    },\n    {\n      \"content\": \"## ANALYSIS\\nmore\"\n    }\n  ]\n}";
        let (header, unwrapped) = repair_content(content).unwrap();
        assert_eq!(header, "# Video learning: watch video: https://x\n\n");
        assert_eq!(unwrapped, "## VIDEO SUMMARY\nbody\n\n## ANALYSIS\nmore");
    }

    #[test]
    fn leaves_a_clean_note_untouched() {
        // Post-fix notes have no envelope; the repair must be a no-op on them
        // so a re-run cannot corrupt already-good content.
        assert!(repair_content("# Video learning: x\n\n## VIDEO SUMMARY\nbody").is_none());
    }

    #[test]
    fn leaves_prose_containing_braces_untouched() {
        assert!(repair_content("Some prose with { a brace } and no envelope").is_none());
    }

    #[test]
    fn leaves_a_multi_column_envelope_untouched() {
        // Multi-column results are real tabular data — unwrapping would destroy
        // the association between columns, so the note is skipped.
        let content = "# Header\n\n{\n  \"count\": 1,\n  \"rows\": [\n    {\n      \"id\": \"a\",\n      \"goal\": \"b\"\n    }\n  ]\n}";
        assert!(repair_content(content).is_none());
    }
}
