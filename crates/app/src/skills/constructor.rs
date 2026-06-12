//! ConstructorSkill — Phase 2 of the Agent Constructor plan.
//!
//! One tool: `construct_agent`. Given a goal, it queries the self-model
//! meta-graph (`:ToolDef`, `:ContextProfile`, `:ModelDef` — Phase 0b) for
//! what actually exists, has an LLM plan an AgentSpec (a bespoke chain of
//! tool steps), validates the plan against the live tool inventory, persists
//! it as a `(:AgentSpec)` node, and optionally dispatches it as a Task +
//! job chain.
//!
//! Trust guardrails (settled design decisions):
//! - The planning LLM call is capability-routed (`["reasoning"]`) through the
//!   model router — the designated higher-order step from the cloud policy.
//! - Every dispatched constructed chain gets a **mandatory** adversarial
//!   pre-flight with elevated `min_robustness` (3.0 vs the template default
//!   2.5), and an evaluator step when `success_criteria` is provided.
//! - Self-modifying tools are denylisted from constructed plans.
//! - Plans are validated against the self-model: unknown tools are rejected,
//!   step counts are capped.
//!
//! Known Phase 2 limitation: if the adversarial review aborts a constructed
//! chain, the retry task re-enters the standard scheduler pipeline (keyword
//! chains), not the constructed spec. Reuse-before-construct is Phase 3.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::model_router;
use crate::services::queue::{ChainStep, QueueService, SELECTED_LLM};
use crate::services::traits::LlmProvider;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

/// Tools a constructed plan may never include. The constructor building
/// chains that rewrite schedules, chains, or tools is self-modification —
/// that autonomy has to be earned through Phase 3 performance history.
const DENYLISTED_TOOLS: &[&str] = &[
    "construct_agent",
    "manage_scheduled_task",
    "manage_chain",
    "manage_dynamic_tool",
    "scheduler_control",
    "use_model",
    "reload_models",
];

const DEFAULT_MAX_STEPS: usize = 6;
const HARD_MAX_STEPS: usize = 10;
/// Constructed chains face a stricter adversarial gate than templates (2.5).
const CONSTRUCTED_MIN_ROBUSTNESS: f32 = 3.0;

pub struct ConstructorSkill {
    neo4j: Neo4jClient,
    queue: Option<Arc<QueueService>>,
    llm: Arc<dyn LlmProvider>,
    telemetry: Option<TelemetryClient>,
}

impl ConstructorSkill {
    pub fn new(
        neo4j: Neo4jClient,
        queue: Option<Arc<QueueService>>,
        llm: Arc<dyn LlmProvider>,
        telemetry: Option<TelemetryClient>,
    ) -> Self {
        Self {
            neo4j,
            queue,
            llm,
            telemetry,
        }
    }

    fn construct_agent_def() -> ToolDefinition {
        ToolDefinition {
            name: "construct_agent".to_string(),
            description: "Construct a bespoke agent (a validated chain of tool steps) for a goal. \
                Queries the self-model for available tools/profiles/models, plans the steps with \
                an LLM, validates every step against the live tool inventory, and stores the \
                result as an AgentSpec node. With dispatch=true it also creates a Task and \
                enqueues the chain — every constructed chain gets a mandatory adversarial \
                pre-flight (min robustness 3.0) and, when success_criteria is set, an evaluator \
                step that grades the outcome. Use this for goals that don't fit an existing \
                chain template."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "What the constructed agent should accomplish."
                    },
                    "success_criteria": {
                        "type": "string",
                        "description": "Measurable definition of done. Strongly recommended — enables the evaluator loop."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background for the plan and the dispatched task."
                    },
                    "max_steps": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "description": "Step budget for the plan (default 6, hard cap 10)."
                    },
                    "dispatch": {
                        "type": "boolean",
                        "description": "true = create a Task and enqueue the chain now. false (default) = plan and store only."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional AgentSpec name (defaults to a slug of the goal)."
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    /// Fetch the self-model blocks the planner prompt is grounded in.
    async fn self_model_blocks(&self) -> Result<(String, String, Vec<String>), String> {
        // Tools: name + skill + description (truncated — descriptions are long).
        let rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (t:ToolDef) RETURN t.name AS name, t.skill AS skill, \
                 left(t.description, 160) AS description ORDER BY t.skill, t.name",
            ))
            .await
            .map_err(|e| format!("Self-model unavailable (ToolDef query failed): {e}"))?;

        let mut tool_names: Vec<String> = Vec::new();
        let mut tools_block = String::new();
        for row in &rows {
            let name: String = row.get("name").unwrap_or_default();
            let skill: String = row.get("skill").unwrap_or_default();
            let desc: String = row.get("description").unwrap_or_default();
            if DENYLISTED_TOOLS.contains(&name.as_str()) {
                continue;
            }
            tools_block.push_str(&format!("- {name} [{skill}]: {desc}\n"));
            tool_names.push(name);
        }
        if tool_names.is_empty() {
            return Err("Self-model has no ToolDef nodes — run build_skills first".to_string());
        }

        let profile_rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (c:ContextProfile) RETURN c.name AS name, c.description AS description \
                 ORDER BY c.name",
            ))
            .await
            .map_err(|e| format!("ContextProfile query failed: {e}"))?;
        let mut profiles_block = String::new();
        for row in &profile_rows {
            let name: String = row.get("name").unwrap_or_default();
            let desc: String = row.get("description").unwrap_or_default();
            profiles_block.push_str(&format!("- {name}: {desc}\n"));
        }

        Ok((tools_block, profiles_block, tool_names))
    }

    fn planning_prompt(
        goal: &str,
        context: Option<&str>,
        success_criteria: Option<&str>,
        max_steps: usize,
        tools_block: &str,
        profiles_block: &str,
    ) -> String {
        format!(
            "You are the Agent Constructor for an autonomous brain. Design a chain of tool \
             steps (an agent) that accomplishes the GOAL.\n\n\
             GOAL: {goal}\n\
             {ctx}{crit}\n\
             AVAILABLE TOOLS (use ONLY these exact names):\n{tools_block}\n\
             AVAILABLE CONTEXT PROFILES (optional per-step persona):\n{profiles_block}\n\
             HARD RULES:\n\
             1. At most {max_steps} steps. Fewer is better.\n\
             2. Each step's output is passed to the next step ONLY via the literal string \
             {{{{_prev}}}} in its arguments. Earlier steps' outputs are NOT available — \
             sequence accordingly.\n\
             3. Any `reason` step that consumes a previous step's output MUST set \
             \"context\": \"{{{{_prev}}}}\" in its arguments and phrase the question around \
             the CONTEXT. Never let reason run ungrounded.\n\
             4. The final meaningful step must persist a durable artifact (e.g. store_note \
             with note_type semantic, or write_workspace_file).\n\
             5. A step that needs stronger reasoning may set \"required_capabilities\": \
             [\"reasoning\"] — the model router will pick the best allowed model.\n\
             6. Use template vars {{{{goal}}}}, {{{{task_id}}}}, {{{{date}}}} where useful.\n\n\
             Respond with ONLY valid JSON (no markdown fences):\n\
             {{\"name\": \"short-kebab-slug\", \"rationale\": \"one paragraph: why these steps\", \
             \"steps\": [{{\"tool_name\": \"...\", \"arguments\": {{...}}, \
             \"description\": \"what this step does\", \
             \"required_capabilities\": [\"reasoning\"] }}]}}\n\
             (required_capabilities is optional per step; omit it for simple steps.)",
            goal = goal,
            ctx = context
                .map(|c| format!("CONTEXT: {c}\n"))
                .unwrap_or_default(),
            crit = success_criteria
                .map(|c| format!("SUCCESS CRITERIA (the evaluator will grade against this): {c}\n"))
                .unwrap_or_default(),
            tools_block = tools_block,
            profiles_block = profiles_block,
            max_steps = max_steps,
        )
    }

    async fn handle_construct_agent(&self, args: Option<Value>) -> ToolCallResult {
        let args = args.unwrap_or_default();
        let Some(goal) = args["goal"].as_str().filter(|g| !g.trim().is_empty()) else {
            return ToolCallResult::error("`goal` is required".to_string());
        };
        let success_criteria = args["success_criteria"].as_str();
        let context = args["context"].as_str();
        let max_steps = (args["max_steps"]
            .as_u64()
            .unwrap_or(DEFAULT_MAX_STEPS as u64) as usize)
            .clamp(1, HARD_MAX_STEPS);
        let dispatch = args["dispatch"].as_bool().unwrap_or(false);

        // 1. Ground the planner in the self-model.
        let (tools_block, profiles_block, known_tools) = match self.self_model_blocks().await {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };

        // 2. Plan — capability-routed: constructing agents is the designated
        //    higher-order reasoning step from the cloud policy.
        let prompt = Self::planning_prompt(
            goal,
            context,
            success_criteria,
            max_steps,
            &tools_block,
            &profiles_block,
        );
        let selected = self.telemetry.as_ref().and_then(|tc| {
            model_router::resolve_model_config(
                tc,
                &["reasoning".to_string()],
                &crate::services::LlmConfig::default(),
            )
        });
        let planner_model = selected
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "active".to_string());
        let raw = match SELECTED_LLM
            .scope(selected, self.llm.generate(&prompt, None))
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Planning LLM call failed: {e}")),
        };

        // 3. Parse + validate against the self-model.
        let plan = match parse_and_validate_plan(&raw, &known_tools, max_steps) {
            Ok(p) => p,
            Err(e) => {
                return ToolCallResult::error(format!(
                    "Constructed plan rejected: {e}\n--- raw plan ---\n{}",
                    raw.chars().take(1500).collect::<String>()
                ));
            }
        };

        // 4. Persist the AgentSpec.
        let spec_id = uuid::Uuid::new_v4().to_string();
        let spec_name = args["name"]
            .as_str()
            .map(String::from)
            .or_else(|| plan.name.clone())
            .unwrap_or_else(|| slugify(goal));
        let steps_json = serde_json::to_string(&plan.raw_steps).unwrap_or_default();
        if let Err(e) = self
            .neo4j
            .run(
                neo4rs::query(
                    "CREATE (a:AgentSpec {id: $id, name: $name, goal: $goal, \
                     steps_json: $steps, rationale: $rationale, \
                     success_criteria: $criteria, created_by: 'construct_agent', \
                     planner_model: $planner_model, created_at: $now})",
                )
                .param("id", spec_id.as_str())
                .param("name", spec_name.as_str())
                .param("goal", goal)
                .param("steps", steps_json.as_str())
                .param("rationale", plan.rationale.as_deref().unwrap_or(""))
                .param("criteria", success_criteria.unwrap_or(""))
                .param("planner_model", planner_model.as_str())
                .param("now", chrono::Utc::now().to_rfc3339()),
            )
            .await
        {
            warn!(error = %e, "Failed to persist AgentSpec (continuing)");
        }

        let step_summary: Vec<Value> = plan
            .steps
            .iter()
            .map(|s| {
                json!({
                    "tool_name": s.tool_name,
                    "description": s.description,
                    "required_capabilities": s.required_capabilities,
                })
            })
            .collect();

        // 5. Plan-only mode stops here.
        if !dispatch {
            return ToolCallResult::success_json(json!({
                "agent_spec_id": spec_id,
                "name": spec_name,
                "planner_model": planner_model,
                "rationale": plan.rationale,
                "steps": step_summary,
                "dispatched": false,
                "hint": "Call again with dispatch=true to create a Task and run this chain."
            }));
        }

        // 6. Dispatch: Task + mandatory adversarial gate + evaluator + update_task.
        let Some(ref queue) = self.queue else {
            return ToolCallResult::error(
                "Queue service not available — cannot dispatch".to_string(),
            );
        };
        let task_id = match self
            .neo4j
            .create_task(
                goal,
                context.or(Some("Constructed by construct_agent")),
                success_criteria,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => return ToolCallResult::error(format!("Task creation failed: {e}")),
        };

        let mut steps = plan.steps;
        // Substitute {{task_id}} / {{goal}} / {{date}} like the scheduler does.
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        for s in &mut steps {
            if let Some(a) = &s.arguments {
                let substituted = a
                    .to_string()
                    .replace("{{task_id}}", &task_id)
                    .replace("{{goal}}", goal)
                    .replace("{{date}}", &date);
                s.arguments = serde_json::from_str(&substituted)
                    .ok()
                    .or(s.arguments.clone());
            }
        }

        // Evaluator before update_task when criteria exist.
        if let Some(c) = success_criteria {
            steps.push(ChainStep {
                tool_name: "reflect_on_work".to_string(),
                arguments: Some(json!({
                    "goal": c,
                    "current_state": "{{_prev}}",
                    "task_id": task_id
                })),
                priority: Some(1),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                description: Some("Evaluate constructed-chain output against criteria".into()),
                is_evaluator: true,
                min_score: Some(3.5),
                evaluator_task_id: Some(task_id.clone()),
                ..Default::default()
            });
        }
        steps.push(ChainStep {
            tool_name: "update_task".to_string(),
            arguments: Some(json!({ "task_id": task_id, "status": "completed" })),
            priority: Some(1),
            max_attempts: Some(3),
            provider_hint: Some("ollama".to_string()),
            ..Default::default()
        });

        // Mandatory adversarial pre-flight, elevated threshold.
        let plan_description = steps
            .iter()
            .filter_map(|s| s.description.as_deref().or(Some(s.tool_name.as_str())))
            .collect::<Vec<_>>()
            .join(" → ");
        steps.insert(
            0,
            ChainStep {
                tool_name: "adversarial_plan_review".to_string(),
                arguments: Some(json!({
                    "goal": goal,
                    "proposed_plan": plan_description,
                    "n_hypotheses": 3
                })),
                priority: Some(1),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                description: Some("Adversarial pre-flight (constructed chain)".into()),
                is_adversarial: true,
                min_robustness: Some(CONSTRUCTED_MIN_ROBUSTNESS),
                adversarial_task_id: Some(task_id.clone()),
                ..Default::default()
            },
        );

        let job_ids = match queue.enqueue_chain(&steps, None).await {
            Ok(ids) => ids,
            Err(e) => return ToolCallResult::error(format!("Enqueue failed: {e}")),
        };
        let _ = self
            .neo4j
            .update_task_status(&task_id, crate::models::TaskStatus::InProgress)
            .await;
        let _ = self
            .neo4j
            .run(
                neo4rs::query(
                    "MATCH (a:AgentSpec {id: $sid}), (t:Task {id: $tid}) \
                     MERGE (a)-[:CONSTRUCTED_FOR]->(t)",
                )
                .param("sid", spec_id.as_str())
                .param("tid", task_id.as_str()),
            )
            .await;

        info!(spec = %spec_name, task_id = %task_id, jobs = job_ids.len(), "Constructed agent dispatched");
        ToolCallResult::success_json(json!({
            "agent_spec_id": spec_id,
            "name": spec_name,
            "planner_model": planner_model,
            "rationale": plan.rationale,
            "steps": step_summary,
            "dispatched": true,
            "task_id": task_id,
            "job_ids": job_ids,
        }))
    }
}

// ============================================================================
// Plan parsing + validation (pure — unit tested)
// ============================================================================

#[cfg_attr(test, derive(Debug))]
struct ValidatedPlan {
    name: Option<String>,
    rationale: Option<String>,
    steps: Vec<ChainStep>,
    /// The raw step objects as planned, persisted on the AgentSpec.
    raw_steps: Vec<Value>,
}

fn parse_and_validate_plan(
    raw: &str,
    known_tools: &[String],
    max_steps: usize,
) -> Result<ValidatedPlan, String> {
    let text = raw.trim();
    let start = text.find('{').ok_or("no JSON object in plan output")?;
    let end = text.rfind('}').map(|i| i + 1).ok_or("unterminated JSON")?;
    let parsed: Value =
        serde_json::from_str(&text[start..end]).map_err(|e| format!("plan JSON invalid: {e}"))?;

    let raw_steps = parsed["steps"]
        .as_array()
        .ok_or("plan has no `steps` array")?
        .clone();
    if raw_steps.is_empty() {
        return Err("plan has zero steps".to_string());
    }
    if raw_steps.len() > max_steps {
        return Err(format!(
            "plan has {} steps, budget is {max_steps}",
            raw_steps.len()
        ));
    }

    let mut steps: Vec<ChainStep> = Vec::with_capacity(raw_steps.len());
    for (i, s) in raw_steps.iter().enumerate() {
        let tool = s["tool_name"]
            .as_str()
            .ok_or_else(|| format!("step {i} missing tool_name"))?;
        if DENYLISTED_TOOLS.contains(&tool) {
            return Err(format!("step {i} uses denylisted tool `{tool}`"));
        }
        if !known_tools.iter().any(|k| k == tool) {
            return Err(format!("step {i} uses unknown tool `{tool}`"));
        }
        let arguments = match &s["arguments"] {
            Value::Null => None,
            v @ Value::Object(_) => Some(v.clone()),
            _ => return Err(format!("step {i} arguments must be a JSON object")),
        };
        let required_capabilities: Option<Vec<String>> = s["required_capabilities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        steps.push(ChainStep {
            tool_name: tool.to_string(),
            arguments,
            priority: Some(1),
            max_attempts: Some(3),
            provider_hint: Some("ollama".to_string()),
            description: s["description"].as_str().map(String::from),
            required_capabilities,
            ..Default::default()
        });
    }

    Ok(ValidatedPlan {
        name: parsed["name"].as_str().map(String::from),
        rationale: parsed["rationale"].as_str().map(String::from),
        steps,
        raw_steps,
    })
}

fn slugify(goal: &str) -> String {
    goal.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
}

#[async_trait]
impl Skill for ConstructorSkill {
    fn name(&self) -> &str {
        "Agent Constructor"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::construct_agent_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "construct_agent" => Some(self.handle_construct_agent(args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<String> {
        ["search_web", "reason", "store_note", "search_notes"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn valid_plan_parses() {
        let raw = r#"{"name":"test","rationale":"because","steps":[
            {"tool_name":"search_web","arguments":{"query":"{{goal}}"},"description":"gather"},
            {"tool_name":"reason","arguments":{"question":"x","context":"{{_prev}}"},"required_capabilities":["reasoning"]},
            {"tool_name":"store_note","arguments":{"content":"{{_prev}}","note_type":"semantic"}}
        ]}"#;
        let plan = parse_and_validate_plan(raw, &tools(), 6).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.name.as_deref(), Some("test"));
        assert_eq!(
            plan.steps[1].required_capabilities,
            Some(vec!["reasoning".to_string()])
        );
    }

    #[test]
    fn unknown_tool_rejected() {
        let raw = r#"{"steps":[{"tool_name":"rm_rf_everything","arguments":{}}]}"#;
        let err = parse_and_validate_plan(raw, &tools(), 6).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn denylisted_tool_rejected() {
        let raw = r#"{"steps":[{"tool_name":"manage_scheduled_task","arguments":{}}]}"#;
        let err = parse_and_validate_plan(raw, &tools(), 6).unwrap_err();
        assert!(err.contains("denylisted"));
    }

    #[test]
    fn step_budget_enforced() {
        let step = r#"{"tool_name":"reason","arguments":{}}"#;
        let raw = format!(r#"{{"steps":[{0},{0},{0}]}}"#, step);
        let err = parse_and_validate_plan(&raw, &tools(), 2).unwrap_err();
        assert!(err.contains("budget"));
    }

    #[test]
    fn markdown_fenced_json_still_parses() {
        let raw = "```json\n{\"steps\":[{\"tool_name\":\"reason\",\"arguments\":{\"question\":\"q\"}}]}\n```";
        let plan = parse_and_validate_plan(raw, &tools(), 6).unwrap();
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn zero_steps_rejected() {
        let err = parse_and_validate_plan(r#"{"steps":[]}"#, &tools(), 6).unwrap_err();
        assert!(err.contains("zero steps"));
    }
}
