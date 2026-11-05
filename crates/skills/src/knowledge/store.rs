//! `store_note`: what a note must be before it is written, and which ones are flagged.

use super::KnowledgeSkill;
use agent_brain_chat::agent_runner::UNVERIFIED_MARKER;
use agent_brain_models::ProvenanceFlag;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

/// Marker a research step writes as the first line of its answer when the
/// evidence did not answer the question. Chain prompts ask for it by name.
pub const NO_FINDINGS_MARKER: &str = "NO FINDINGS";

/// The opening of a body paragraph that says the evidence did not cover the
/// question: "The provided context does not contain …". Only short notes are
/// checked, so an answer that lists what is missing at the end is not caught.
static NULL_RESULT_OPENER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)^(\*\*)?(the provided|based on the provided|regarding|the web search|the search)[^\n]{0,240}\b(do|does) not (contain|provide|offer|include|mention|address)\b",
    )
    .expect("null-result opener regex")
});

/// Longest note the opener fallback applies to; matches
/// `scripts/retype_null_result_notes.cypher`.
const NULL_RESULT_MAX_CHARS: usize = 900;

/// Whether a note that would be stored as knowledge (`semantic`, or no type)
/// only reports that a search came up empty.
///
/// Research and gap chains store their `reason` output as `semantic`, and
/// consolidation merges semantic notes. A note saying "the sources do not
/// contain anything about X" was therefore merged as a finding: 4 of the 8
/// sources of the "what is a knowledge gap" consolidation (2026-10-03) were
/// such notes, and 104 more were waiting. Stored as `episodic`, the note still
/// records that the search ran and leaves consolidation alone.
///
/// Two signals: the `NO FINDINGS` marker on any line (what the prompts ask
/// for), or, for a short note, a first body paragraph that opens by saying the
/// evidence lacks the answer (what local models write when they skip the marker).
fn is_null_result(content: &str, note_type: Option<&str>) -> bool {
    if !matches!(note_type.map(str::trim), None | Some("") | Some("semantic")) {
        return false;
    }
    let marked = content.lines().any(|line| {
        line.trim_start_matches(|c: char| c == '#' || c == '*' || c == '-' || c.is_whitespace())
            .starts_with(NO_FINDINGS_MARKER)
    });
    if marked {
        return true;
    }
    if content.chars().count() >= NULL_RESULT_MAX_CHARS {
        return false;
    }
    // First paragraph after an optional markdown heading line.
    let body = match content.trim_start().strip_prefix('#') {
        Some(_) => content
            .trim_start()
            .split_once('\n')
            .map_or("", |(_, rest)| rest),
        None => content,
    };
    NULL_RESULT_OPENER.is_match(body.trim_start())
}

/// Whether a note headed for `semantic` (or untyped) carries the UNVERIFIED
/// marker on a line of its own. Such a note is stored as `unsourced_synthesis`.
fn is_unverified(content: &str, note_type: Option<&str>) -> bool {
    matches!(note_type.map(str::trim), None | Some("") | Some("semantic"))
        && content.lines().any(|line| {
            line.trim_start_matches(|c: char| c == '#' || c == '*' || c.is_whitespace())
                .starts_with(UNVERIFIED_MARKER)
        })
}

/// The verifier's reason from an UNVERIFIED marker line: the text after the
/// marker, up to the sentence `run_agent` appends. `None` without a marker.
fn unverified_reason(content: &str) -> Option<String> {
    let line = content.lines().find_map(|line| {
        line.trim_start_matches(|c: char| c == '#' || c == '*' || c.is_whitespace())
            .strip_prefix(UNVERIFIED_MARKER)
    })?;
    let reason = line
        .split(". Treat everything below as unchecked")
        .next()
        .unwrap_or(line)
        .trim()
        .trim_end_matches('.');
    Some(if reason.is_empty() {
        "unspecified".to_string()
    } else {
        reason.to_string()
    })
}

#[derive(Debug, Deserialize)]
struct StoreNoteInput {
    content: String,
    #[serde(default)]
    note_type: Option<String>,
    #[serde(default)]
    source_context: Option<String>,
    #[serde(default)]
    event_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
}

/// A note's point: both coordinates or neither, each in range.
fn note_point(lat: Option<f64>, lon: Option<f64>) -> Result<Option<(f64, f64)>, String> {
    match (lat, lon) {
        (None, None) => Ok(None),
        (Some(la), Some(lo)) if (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) => {
            Ok(Some((la, lo)))
        }
        (Some(_), Some(_)) => Err("lat must be within -90..90 and lon within -180..180".into()),
        _ => Err("a location needs both lat and lon".into()),
    }
}

impl KnowledgeSkill {
    pub(super) fn store_note_def() -> ToolDefinition {
        ToolDefinition {
            name: "store_note".to_string(),
            description: "Store a text note or memory in the knowledge graph. \
                         The note will be embedded for semantic search, automatically \
                         linked to similar notes, and entities extracted when LLM is available. \
                         Long notes (>1500 chars) are semantically chunked into sub-notes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Content of the note"
                    },
                    "note_type": {
                        "type": "string",
                        "enum": ["semantic", "episodic", "reflection", "consolidated", "outcome", "inference", "claim", "source_record", "news", "unsourced_synthesis"],
                        "description": "Type of note (default: semantic). Use `source_record` for what an external source asserted (web/video), `claim` for a single checkable proposition, `unsourced_synthesis` for your own uncited reasoning. Do not invent a type — an unknown value is stored but never labelled or recalled correctly."
                    },
                    "source_context": {
                        "type": "string",
                        "description": "Optional source context (e.g. session ID, document URL)"
                    },
                    "event_at": {
                        "type": "string",
                        "description": "Optional ISO-8601 timestamp of the event this note describes"
                    },
                    "lat": {
                        "type": "number",
                        "minimum": -90,
                        "maximum": 90,
                        "description": "Optional: where what the note describes happened (latitude, WGS 84), with `lon`. Only for a note about an event or observation at a place (a reported incident, a ship's position); a note that merely mentions a place is found near it already."
                    },
                    "lon": {
                        "type": "number",
                        "minimum": -180,
                        "maximum": 180,
                        "description": "Optional: longitude, with `lat`."
                    },
                    "provenance": {
                        "type": "string",
                        "enum": ["user_input", "synthesis_inference", "core_training"],
                        "description": "Source authority flag: who/what produced this note. Default: user_input for an outside MCP client, synthesis_inference for the brain's own jobs, chats and agents (which cannot claim user_input)."
                    }
                },
                "required": ["content"]
            }),
        }
    }

    pub(super) async fn handle_store_note(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: StoreNoteInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        // Checked before anything is stored: a half-given point is refused,
        // never stored without it.
        let point = match note_point(input.lat, input.lon) {
            Ok(p) => p,
            Err(e) => return ToolCallResult::error(format!("Not stored: {e}.")),
        };

        info!(content_len = input.content.len(), "Storing note");

        // Who is writing decides the default; see `effective_provenance`.
        let provenance = input
            .provenance
            .as_deref()
            .and_then(|s| s.parse::<ProvenanceFlag>().ok());

        let null_result = is_null_result(&input.content, input.note_type.as_deref());
        let unverified = !null_result && is_unverified(&input.content, input.note_type.as_deref());
        // Recorded whatever type the caller asked for: a note that failed
        // verification is labelled so on retrieval, not just retyped.
        let failed_reason = (!null_result)
            .then(|| unverified_reason(&input.content))
            .flatten();
        let note_type = if null_result {
            info!(
                source_context = ?input.source_context,
                "store_note: research found nothing — storing as episodic, not semantic"
            );
            Some("episodic")
        } else if unverified {
            info!(
                source_context = ?input.source_context,
                "store_note: agent answer failed verification — storing as unsourced_synthesis"
            );
            Some("unsourced_synthesis")
        } else {
            input.note_type.as_deref()
        };

        match self
            .svc
            .store_note(
                &input.content,
                note_type,
                input.source_context.as_deref(),
                input.event_at.as_deref(),
                provenance,
            )
            .await
        {
            Ok((id, links_created)) => {
                if let Some(reason) = &failed_reason
                    && let Err(e) = self.svc.mark_verification_failed(&id, reason).await
                {
                    warn!(error = %e, note_id = %id, "store_note: could not flag failed verification");
                }
                let location_error = match point {
                    Some((lat, lon)) => self.svc.set_note_location(&id, lat, lon).await.err(),
                    None => None,
                };
                // "answer" echoes the stored content so downstream {{_prev}} steps
                // (e.g. notify_user after store_note) receive the original text, not metadata JSON.
                let mut response = json!({
                    "success": true,
                    "id": id,
                    "links_created": links_created,
                    "message": "Note stored successfully",
                    "answer": input.content
                });
                if let Some((lat, lon)) = point {
                    response["location"] = match &location_error {
                        None => json!({ "lat": lat, "lon": lon }),
                        Some(e) => json!({ "error": format!("stored without its location: {e}") }),
                    };
                }
                if unverified {
                    response["note_type"] = json!("unsourced_synthesis");
                    response["message"] = json!(
                        "Note stored as `unsourced_synthesis`: it carries an UNVERIFIED marker \
                         (the agent did not do what its caller required), so it is not knowledge."
                    );
                }
                if null_result {
                    response["note_type"] = json!("episodic");
                    response["message"] = json!(
                        "Note stored as `episodic`: it reports that the evidence did not answer \
                         the question, which is a record of a search, not knowledge."
                    );
                }
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to store note: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use agent_brain_protocol::Skill;
    use std::sync::Arc;

    #[test]
    fn a_note_point_needs_both_coordinates_in_range() {
        assert_eq!(note_point(None, None), Ok(None));
        assert_eq!(
            note_point(Some(26.56), Some(56.25)),
            Ok(Some((26.56, 56.25)))
        );
        assert!(note_point(Some(26.5), None).unwrap_err().contains("both"));
        assert!(note_point(None, Some(56.0)).unwrap_err().contains("both"));
        assert!(
            note_point(Some(91.0), Some(0.0))
                .unwrap_err()
                .contains("within")
        );
        assert!(
            note_point(Some(0.0), Some(-181.0))
                .unwrap_err()
                .contains("within")
        );
    }

    fn skill_ok() -> KnowledgeSkill {
        KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("{}")),
        )
    }

    #[test]
    fn a_no_findings_marker_makes_a_research_note_episodic() {
        let note =
            "## Research: sodium battery cost\n\nNO FINDINGS — no result gave a $/kWh figure.";
        assert!(is_null_result(note, Some("semantic")));
        assert!(is_null_result(note, None));
        assert!(is_null_result(
            "## Deep dive: x\n\n**NO FINDINGS** nothing",
            None
        ));
    }

    #[test]
    fn a_short_note_opening_with_absence_is_a_null_result() {
        let note = "## Gap synthesis: IRA captivity context\n\nThe provided context does not \
                    contain any specific information detailing the nature of the context.";
        assert!(is_null_result(note, Some("semantic")));
    }

    #[test]
    fn an_answer_that_lists_what_is_missing_afterwards_stays_semantic() {
        let note = "## Gap synthesis: sodium batteries\n\n## ANSWER\n- CATL ships Naxtra cells \
                    (https://example.com).\n\n## STILL UNKNOWN\nThe provided sources do not \
                    contain a price per kWh.";
        assert!(!is_null_result(note, Some("semantic")));
    }

    #[test]
    fn a_long_note_is_not_judged_by_its_opener() {
        let note = format!(
            "## Research: x\n\nThe provided context does not contain a definition, but: {}",
            "fact. ".repeat(200)
        );
        assert!(!is_null_result(&note, Some("semantic")));
    }

    #[test]
    fn other_note_types_are_never_retyped() {
        let note = "NO FINDINGS — nothing turned up.";
        assert!(!is_null_result(note, Some("reflection")));
        assert!(!is_null_result(note, Some("source_record")));
    }

    #[tokio::test]
    async fn an_unverified_agent_answer_is_stored_as_unsourced_synthesis() {
        let skill = skill_ok();
        let args = json!({
            "content": "## Deep dive: x\n\nUNVERIFIED — no successful `mcp_call` call this run.\n\n## ANSWER\n- y",
            "note_type": "semantic"
        });
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["note_type"], "unsourced_synthesis");
        assert!(!is_unverified("UNVERIFIED — x", Some("reflection")));
    }

    #[test]
    fn unverified_reason_is_the_verifiers_words() {
        let content = "## Deep dive: q\n\nUNVERIFIED — the code did not compile. Treat everything below as unchecked.\n\n## ANSWER";
        assert_eq!(
            unverified_reason(content).as_deref(),
            Some("the code did not compile")
        );
        assert_eq!(unverified_reason("## ANSWER\n- fine"), None);
        assert_eq!(
            unverified_reason("UNVERIFIED —").as_deref(),
            Some("unspecified")
        );
    }
}
