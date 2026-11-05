//! `reason` and its neighbours: `synthesize_knowledge`, `adversarial_plan_review`.

use super::{KnowledgeSkill, default_consolidate_limit};
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

#[derive(Debug, Deserialize)]
struct AdversarialPlanReviewInput {
    goal: String,
    proposed_plan: String,
    #[serde(default = "default_n_hypotheses")]
    n_hypotheses: u8,
    #[serde(default)]
    task_id: Option<String>,
}

fn default_n_hypotheses() -> u8 {
    3
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
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        // A drafting agent invents modes ("summarize", "classify")
                        // when only the description names them; the enum lets
                        // the save check refuse one instead of it silently
                        // running as infer.
                        "enum": ["infer", "explain", "clarify", "audit", "structured"],
                        "description": "Mode: infer (default), explain, clarify, audit, or structured"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question/decision/request/action to reason about (required)"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional raw context text. When provided in infer mode, bypasses RAG and reasons directly over this text — use for live data like search results passed via {{_prev}}. Also used by clarify and audit."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max knowledge notes to retrieve (default: 8 for infer/explain)"
                    },
                    "store_inference": {
                        "type": "boolean",
                        "description": "Persist the result as a Note (default: true, infer mode only)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Include task-linked reflection notes (explain mode)"
                    },
                    "available_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tools available to the agent (clarify mode)"
                    },
                    "run_critic": {
                        "type": "boolean",
                        "description": "Run adversarial critic pass to stress-test the answer and adjust confidence (structured mode only, default: false)"
                    },
                    "create_gap_tasks": {
                        "type": "boolean",
                        "description": "Create Task nodes for each identified knowledge gap (structured mode only, requires store_inference=true, default: false)"
                    }
                },
                "required": ["question"]
            }),
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

    pub(super) fn adversarial_plan_review_def() -> ToolDefinition {
        ToolDefinition {
            name: "adversarial_plan_review".to_string(),
            description:
                "Stress-test a proposed plan before execution by generating N adversarial \
                         failure scenarios. For each hypothesis the tool scores how well the plan \
                         handles it (1–5) and suggests a mitigation. Returns an overall robustness \
                         score and adjusted plan notes. Results are stored as a semantic note so \
                         future reviews benefit from accumulated failure-pattern knowledge."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The goal or objective the plan is trying to achieve"
                    },
                    "proposed_plan": {
                        "type": "string",
                        "description": "Description of the proposed plan steps (free text or step list)"
                    },
                    "n_hypotheses": {
                        "type": "integer",
                        "description": "Number of failure hypotheses to generate (default: 3)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task ID to link the adversarial review note via REFLECTS_ON"
                    }
                },
                "required": ["goal", "proposed_plan"]
            }),
        }
    }

    pub(super) async fn handle_adversarial_plan_review(
        &self,
        arguments: Option<Value>,
    ) -> ToolCallResult {
        let input: AdversarialPlanReviewInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(
            goal = %input.goal,
            n = input.n_hypotheses,
            "Running adversarial plan review"
        );

        // Pull past adversarial analyses from the knowledge base so the LLM can
        // avoid repeating already-known failure patterns and build on prior learning.
        let past_patterns = self
            .svc
            .search_notes("adversarial review plan failure scenario", 3, 0, None, None)
            .await
            .unwrap_or_default();

        let past_context = if past_patterns.is_empty() {
            "None found.".to_string()
        } else {
            past_patterns
                .iter()
                .filter_map(|n| n["content"].as_str())
                .take(3)
                .map(|c| format!("- {}", c.chars().take(300).collect::<String>()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let prompt = format!(
            "You are an Adversarial Critic. Stress-test the following plan by generating \
             {n} distinct failure scenarios — plausible ways the plan could fail, be derailed, \
             or produce a wrong outcome.\n\n\
             GOAL: {goal}\n\
             PROPOSED PLAN: {plan}\n\n\
             PAST FAILURE PATTERNS (from knowledge base — do not repeat these):\n{past}\n\n\
             For each scenario score how well the current plan handles it \
             (1 = completely unprepared, 5 = fully mitigated).\n\
             Then give an overall_robustness score (1.0–5.0, one decimal) and \
             brief adjusted_plan_notes on how to strengthen the weakest points.\n\n\
             Respond ONLY with valid JSON — no markdown fences, no explanation outside the JSON:\n\
             {{\"hypotheses\":[{{\"scenario\":\"...\",\"robustness_score\":N,\"mitigation\":\"...\"}}],\
             \"overall_robustness\":N.N,\"adjusted_plan_notes\":\"...\"}}",
            n = input.n_hypotheses,
            goal = input.goal,
            plan = input.proposed_plan,
            past = past_context,
        );

        let raw = match self.llm.generate(&prompt, None).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("LLM call failed: {e}")),
        };

        let text = raw.trim();
        let parsed: Value = agent_brain_llm::llm::first_json_object(text, &["overall_robustness"])
            .unwrap_or_else(|| {
                json!({
                    "hypotheses": [],
                    "overall_robustness": 3.0,
                    "adjusted_plan_notes": text
                })
            });

        let overall_robustness = parsed["overall_robustness"].as_f64().unwrap_or(3.0);

        // Persist so future reviews can learn from this one.
        let note_content = format!(
            "Adversarial Plan Review\nGoal: {}\nPlan: {}\nOverall robustness: {:.1}/5\n\n{}",
            input.goal,
            input.proposed_plan.chars().take(200).collect::<String>(),
            overall_robustness,
            serde_json::to_string_pretty(&parsed).unwrap_or_default()
        );
        let stored = self
            .svc
            .store_note(
                &note_content,
                Some("semantic"),
                Some("adversarial_review"),
                None,
                None,
            )
            .await;
        // The schema promises the review is linked to its task; a review that
        // is never linked cannot be found from the task it judged.
        if let (Ok((note_id, _)), Some(task_id)) = (&stored, input.task_id.as_deref()) {
            match self.svc.link_note_to_task(note_id, task_id).await {
                Ok(true) => {}
                Ok(false) => warn!(
                    task_id,
                    "adversarial review: task not found, note left unlinked"
                ),
                Err(e) => {
                    warn!(task_id, error = %e, "adversarial review: linking note to task failed")
                }
            }
        }

        ToolCallResult::success_json(parsed)
    }

    pub(super) fn synthesize_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "synthesize_knowledge".to_string(),
            description: "Search for recent notes and inferences related to a topic, then use \
                         the LLM to distill durable facts, patterns, and insights into a new \
                         semantic knowledge note. Use this at the end of a reasoning or research \
                         chain to persist what was actually learned."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "The topic or goal to synthesize knowledge about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of source notes to draw from (default: 10)"
                    }
                },
                "required": ["topic"]
            }),
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
