//! `reason` and its neighbour `synthesize_knowledge`.

use super::{KnowledgeSkill, default_consolidate_limit};
use agent_brain_protocol::tool_schema;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

/// Check the links in a reason result's `answer` and `inferences` against
/// the prompt it was given, rewriting them in place. The finding, if any.
fn check_reason_links(parsed: &mut Value, prompt: &str) -> Option<Value> {
    use agent_brain_knowledge::knowledge::exact::{LinkCheck, check_links};
    // Every string under `answer` and `inferences`: a model asked for a
    // report sometimes returns `answer` as a list of lines.
    fn walk(v: &mut Value, prompt: &str, found: &mut LinkCheck) {
        match v {
            Value::String(t) => {
                let c = check_links(t, prompt);
                if c.changed() {
                    *t = c.text;
                    found.relinked.extend(c.relinked);
                    found.not_in_sources.extend(c.not_in_sources);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|i| walk(i, prompt, found)),
            Value::Object(m) => m.values_mut().for_each(|i| walk(i, prompt, found)),
            _ => {}
        }
    }
    let mut found = LinkCheck::default();
    walk(&mut parsed["answer"], prompt, &mut found);
    walk(&mut parsed["inferences"], prompt, &mut found);
    found.relinked.sort();
    found.relinked.dedup();
    found.not_in_sources.sort();
    found.not_in_sources.dedup();
    let report = found.report()?;
    warn!(relinked = found.relinked.len(), not_in_sources = ?found.not_in_sources,
          "reason: links checked against the prompt");
    Some(report)
}

/// A reason result's `answer` as text to store: a string as it is, a list as
/// its lines; with no `answer`, the raw reply, link-checked like the rest.
fn answer_text(parsed: &Value, raw: &str, prompt: &str) -> String {
    match &parsed["answer"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().map_or_else(|| i.to_string(), String::from))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => agent_brain_knowledge::knowledge::exact::check_links(raw, prompt).text,
        other => other.to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct ReasonInput {
    question: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default = "default_reason_limit")]
    limit: usize,
    #[serde(
        default = "default_store_inference",
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    store_inference: bool,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    available_tools: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    run_critic: bool,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    create_gap_tasks: bool,
}

#[derive(Debug, Deserialize)]
struct SynthesizeKnowledgeInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

fn default_reason_limit() -> usize {
    8
}

fn default_store_inference() -> bool {
    true
}

impl KnowledgeSkill {
    pub(super) fn reason_def() -> ToolDefinition {
        ToolDefinition {
            name: "reason".to_string(),
            description: "Multi-mode LLM reasoning tool. \
                         action=\"infer\" (default): derive new inferences from stored knowledge; \
                         action=\"explain\": narrate why a decision occurred, citing sources; \
                         action=\"clarify\": analyse a request for ambiguity and generate questions; \
                         action=\"audit\": check a proposed action against stored values and principles; \
                         action=\"structured\": full structured output with sources, caveats, follow-up questions, \
                         gaps, inferences, and optional adversarial critic pass."
                .to_string(),
            input_schema: tool_schema!(core "reason"),
        }
    }

    pub(super) async fn handle_reason(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReasonInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match input.action.as_deref().unwrap_or("infer") {
            "explain" => {
                info!(decision = %input.question, "Explaining reasoning");
                match self
                    .svc
                    .explain_reasoning(&input.question, input.task_id.as_deref(), input.limit)
                    .await
                {
                    Ok((explanation, sources)) => ToolCallResult::success_json(json!({
                        "explanation": explanation,
                        "knowledge_sources": sources,
                    })),
                    Err(e) => ToolCallResult::error(format!("Explanation failed: {e}")),
                }
            }
            "clarify" => {
                info!(request = %input.question, "Analyzing request for clarification");
                let mut prompt = format!(
                    "Analyze the following request for ambiguity. Determine if clarification is needed before acting.\n\nREQUEST: {}\n",
                    input.question
                );
                if let Some(ctx) = &input.context {
                    prompt.push_str(&format!("\nCONTEXT: {ctx}\n"));
                }
                if let Some(tools) = &input.available_tools
                    && !tools.is_empty()
                {
                    prompt.push_str(&format!("\nAVAILABLE TOOLS: {}\n", tools.join(", ")));
                }
                prompt.push_str(
                    r#"
Respond with a JSON object only (no markdown, no explanation):
{
  "needs_clarification": true,
  "ambiguities": ["specific ambiguous aspect 1", "..."],
  "clarifying_questions": ["question to ask 1", "..."],
  "assumptions": ["assumption that would be made if proceeding 1", "..."],
  "recommended_approach": "brief description of how to proceed"
}"#,
                );
                match self
                    .llm
                    .generate_json(&prompt, None, &["needs_clarification"], 2)
                    .await
                {
                    Ok(parsed) => ToolCallResult::success_json(parsed),
                    // The LLM was unavailable, or still couldn't produce valid
                    // JSON after self-correcting retries — surface the error
                    // rather than fabricate a clarification result.
                    Err(e) => ToolCallResult::error(format!("Clarification analysis failed: {e}")),
                }
            }
            "audit" => {
                info!(action = %input.question, "Auditing action");
                match self
                    .svc
                    .audit_action(&input.question, input.context.as_deref())
                    .await
                {
                    Ok((aligned, confidence, concerns, suggestions, reasoning)) => {
                        ToolCallResult::success_json(json!({
                            "aligned": aligned,
                            "confidence": confidence,
                            "concerns": concerns,
                            "suggestions": suggestions,
                            "reasoning": reasoning,
                        }))
                    }
                    Err(e) => ToolCallResult::error(format!("Audit failed: {e}")),
                }
            }
            "structured" => {
                info!(question = %input.question, run_critic = input.run_critic, "Structured reasoning");
                match self
                    .svc
                    .reason_structured(
                        &input.question,
                        input.limit,
                        input.store_inference,
                        input.run_critic,
                    )
                    .await
                {
                    Ok(output) => {
                        let sources_json: Vec<Value> = output
                            .sources
                            .iter()
                            .map(|s| json!({ "note_id": s.note_id, "preview": s.preview }))
                            .collect();

                        let mut response = json!({
                            "answer": output.answer,
                            "sources": sources_json,
                            "confidence": output.confidence,
                            "caveats": output.caveats,
                            "follow_up_questions": output.follow_up_questions,
                            "inferences": output.inferences,
                            "gaps": output.gaps,
                            "critic_counter_arguments": output.critic_counter_arguments,
                        });

                        // Emitted only when true, so a normal result is unchanged.
                        // A consumer reading `confidence`/`gaps`/`caveats` has no
                        // other way to tell a graded answer from raw prose wearing
                        // the same field names.
                        if output.structured_output_failed {
                            response["structured_output_failed"] = json!(true);
                            response["caveats"] = json!([
                                "The model did not return parseable structured output; this \
                                 answer is raw prose. `confidence`, `gaps` and `caveats` were \
                                 NOT reported by the model and must not be read as its own \
                                 assessment."
                            ]);
                        }

                        if let Some(nid) = &output.inference_note_id {
                            response["inference_note_id"] = json!(nid);
                        }

                        if input.create_gap_tasks
                            && let Some(ref note_id) = output.inference_note_id
                            && !output.gaps.is_empty()
                        {
                            match self.svc.create_gap_tasks(&output.gaps, note_id).await {
                                Ok(ids) => {
                                    response["gap_task_ids"] = json!(ids);
                                }
                                Err(e) => {
                                    response["gap_task_error"] = json!(e.to_string());
                                }
                            }
                        }

                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Structured reasoning failed: {e}")),
                }
            }
            _ => {
                // "infer" (default)
                info!(question = %input.question, limit = input.limit, "Reasoning");

                // When external context is provided, bypass RAG and reason directly over it.
                // This prevents contamination from stale notes when the caller has live data
                // (e.g. freshly-fetched search results in a job chain).
                if let Some(ctx) = &input.context {
                    let prompt = format!(
                        "You are a reasoning engine. Respond in English only. All JSON field values must be in English.\n\
                         Using the provided context, answer the question \
                         clearly. Distinguish what is directly stated vs inferred.\n\
                         Output ONLY valid JSON. The \"answer\" field may contain markdown formatting \
                         (headers, bullets, bold) when the question requests a structured report:\n\
                         {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\"gaps\":[\"...\"]}}\n\n\
                         QUESTION: {}\n\
                         CONTEXT:\n{}",
                        input.question, ctx
                    );
                    return match self.llm.generate(&prompt, None).await {
                        Ok(text_resp) => {
                            let text = text_resp.trim();
                            let mut parsed: Value =
                                agent_brain_llm::llm::first_json_object(text, &["answer"])
                                    .unwrap_or_else(|| {
                                        json!({
                                            "answer": text,
                                            "inferences": [],
                                            "confidence": 0.5,
                                            "gaps": []
                                        })
                                    });
                            // A link the model cites must be in what it was given:
                            // a note id written as a URL is relinked, any other
                            // link outside the prompt is marked (Phase R step 3).
                            let report = check_reason_links(&mut parsed, &prompt);
                            // The context path must honor store_inference like the
                            // RAG path does — scheduler chains rely on the inference
                            // note as their durable output artifact.
                            if input.store_inference {
                                let answer = answer_text(&parsed, text, &prompt);
                                let inferences: Vec<String> = parsed["inferences"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if let Some(id) = self
                                    .svc
                                    .store_context_inference(&input.question, &answer, &inferences)
                                    .await
                                {
                                    parsed["id"] = json!(id);
                                }
                            }
                            if let Some(r) = report {
                                parsed["link_check"] = r;
                            }
                            ToolCallResult::success_json(parsed)
                        }
                        Err(e) => ToolCallResult::error(format!("Reasoning failed: {e}")),
                    };
                }

                match self
                    .svc
                    .reason(&input.question, input.limit, input.store_inference)
                    .await
                {
                    Ok((answer, inferences, confidence, gaps, note_id)) => {
                        let mut response = json!({
                            "answer": answer,
                            "inferences": inferences,
                            "confidence": confidence,
                            "gaps": gaps,
                        });
                        if let Some(nid) = note_id {
                            response["id"] = json!(nid);
                        }
                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Reasoning failed: {e}")),
                }
            }
        }
    }

    pub(super) fn synthesize_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "synthesize_knowledge".to_string(),
            description: "Search for recent notes and inferences related to a topic, then use \
                         the LLM to distill durable facts, patterns, and insights into a new \
                         semantic knowledge note. Use this at the end of a reasoning or research \
                         chain to persist what was actually learned."
                .to_string(),
            input_schema: tool_schema!(core "synthesize_knowledge"),
        }
    }

    pub(super) async fn handle_synthesize_knowledge(
        &self,
        arguments: Option<Value>,
    ) -> ToolCallResult {
        let input: SynthesizeKnowledgeInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Synthesizing semantic knowledge");

        match self
            .svc
            .synthesize_knowledge(&input.topic, input.limit)
            .await
        {
            Ok((note_id, preview)) => ToolCallResult::success_json(json!({
                "success": true,
                "note_id": note_id,
                "note_type": "semantic",
                "preview": preview
            })),
            Err(e) => ToolCallResult::error(format!("Knowledge synthesis failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a context-path inference stores: a list answer as its lines, a
    /// missing one as the raw reply.
    #[test]
    fn the_stored_answer_is_text_whatever_shape_the_model_returned() {
        assert_eq!(answer_text(&json!({"answer": "one"}), "raw", ""), "one");
        assert_eq!(
            answer_text(&json!({"answer": ["a (x)", "b"]}), "raw", ""),
            "a (x)\nb"
        );
        assert_eq!(
            answer_text(&json!({"other": 1}), "raw reply", ""),
            "raw reply"
        );
    }
}
