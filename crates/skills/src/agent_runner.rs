//! Agent Runner Skill — the `run_agent` background tool.
//!
//! Phase 1 of `project-docs/UNIFIED_AGENT_EXECUTION_PLAN.md`. Exposes the shared
//! [`AgentRunner`] (extracted from the chat sub-agent loop in Phase 0) as a real
//! MCP tool, so a **task chain or a scheduled task** can run a profile-driven,
//! bounded, tool-using agent for a step — the same loop chat's `delegate` uses,
//! reached from the background path instead of only from a live chat turn.
//!
//! Unlike chat's `delegate` (intercepted in the provider loop because its result
//! folds back into the ongoing turn), `run_agent` needs no interception: a
//! background chain step already flows through `ToolHandler`, and its result
//! becomes `{{_prev}}` for the next step. A step
//! `{tool_name: "run_agent", arguments: {agent, task}}` *is* "an agent for this
//! step"; a one-step chain of it *is* "one agent overseeing the task."
//!
//! Two ways this differs from a chat sub-agent (see `services/agent_runner.rs`):
//! writes are **allowed** (writing is the background job — `store_note`,
//! `write_workspace_file` — but the inner agent still runs the engine's
//! `TurnWriteGuard`), and there is **no event sink** (findings are collected and
//! returned as the tool result, not streamed to a user).
//!
//! Depth cap 1 is enforced inside `AgentRunner::run` (the inner toolset never
//! contains `delegate`/`run_agent`), so an agent cannot spawn another agent.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::{info, warn};

use agent_brain_chat::agent_runner::{
    AgentBudget, AgentRunner, Harness, RunOptions, VerifySpec, answer_crate, answer_rust_code,
    first_compile_error, label_proposed_placement, mark_unverified_answer, sandbox_dependencies,
    verification_failures,
};
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Tool-use rounds for a background agent. A notch above the chat sub-agent's 6:
/// a task-level agent has more to do than an in-chat lookup, but it stays bounded
/// so the worst-case model-call count is small (the step is one agent, depth 1).
const MAX_BACKGROUND_AGENT_ITERATIONS: usize = 8;

/// Ceiling on a step's `max_rounds`. An agent that writes code and compiles it
/// in the sandbox needs room to read the error and try again (search, read,
/// compile, fix, compile is five rounds before any answer); the cap keeps a
/// single step's worst case bounded.
const MAX_BACKGROUND_AGENT_ROUNDS_CAP: usize = 16;

/// Rounds for one run: the step's `max_rounds` clamped to
/// `1..=MAX_BACKGROUND_AGENT_ROUNDS_CAP`, or the default when absent.
fn effective_rounds(requested: Option<usize>) -> usize {
    requested.map_or(MAX_BACKGROUND_AGENT_ITERATIONS, |n| {
        n.clamp(1, MAX_BACKGROUND_AGENT_ROUNDS_CAP)
    })
}

/// Exposes `run_agent`. Holds a **lazy** handle to the shared [`AgentRunner`]:
/// the runner is created with the `ChatService` (the loop engine) after skills
/// are built, so the slot is empty at registration and populated once the chat
/// service comes up. On a deployment without a chat service (e.g. stdio), the
/// slot stays empty and `run_agent` reports itself unavailable rather than
/// failing obscurely.
pub struct AgentRunnerSkill {
    runner: Arc<RwLock<Option<Arc<AgentRunner>>>>,
    /// Root for `verify.cited_paths_exist`.
    codebase_dir: Option<std::path::PathBuf>,
}

impl AgentRunnerSkill {
    pub fn new(runner: Arc<RwLock<Option<Arc<AgentRunner>>>>) -> Self {
        Self {
            runner,
            codebase_dir: None,
        }
    }

    /// The codebase root `verify.cited_paths_exist` checks citations against.
    pub fn with_codebase_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.codebase_dir = dir;
        self
    }

    fn run_agent_def() -> ToolDefinition {
        ToolDefinition {
            name: "run_agent".to_string(),
            description:
                "Run a spawnable worker agent on a self-contained sub-goal and get its findings \
                 back as this step's result. The agent is a curated context profile (its own \
                 model, toolset, and system prompt) that decides its own steps, calls tools, and \
                 returns a cited answer — use it for the open-ended part of a task (research, \
                 build, review), not for a single deterministic tool call. Pass `agent: \"auto\"` \
                 to auto-pick the best-fitting agent, or a specific name. When none fits, say so \
                 plainly: a new agent is a new context profile, which a person adds. The agent does NOT see the rest of the chain \
                 or the conversation: put everything it needs into `task`. It may write (store \
                 notes, workspace files). A wrong or non-spawnable `agent` name returns the list \
                 of agents you can use."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Which spawnable agent to run (e.g. researcher, code-analyst, knowledge-worker), or \"auto\" to pick the best-fitting agent for the task."
                    },
                    "task": {
                        "type": "string",
                        "description": "A complete, self-contained instruction for the agent (it has no other context)."
                    },
                    "model": {
                        "type": "string",
                        "description": "Run the agent on this model (a catalog model id or name from list_models) instead of its profile's own. Judged by the routing policy; a model that is unknown or not allowed for this work fails the call with the reason, it never falls back."
                    },
                    "probe": {
                        "type": "boolean",
                        "description": "Run as a probe: every call with effects outside the run (writes, task and todo creation, non-GET requests) is answered with a stub saying what would have happened, and is not executed. The result's `trajectory` shows each call. For trying an agent under a candidate `harness`, or seeing what it would do."
                    },
                    "harness": {
                        "type": ["object", "string"],
                        "description": "Probe only: run the agent under a candidate harness instead of its profile's: `system_prompt` (the whole prompt) and/or `tools` (its toolset, registry tools only). Other keys are ignored; JSON text is accepted. The result's `harness` is what the run used.",
                        "properties": {
                            "system_prompt": { "type": "string" },
                            "tools": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "read_only": {
                        "type": "boolean",
                        "description": "Strip the agent's write tools for this run (store_note, write_workspace_file, …), as a chat sub-agent runs. For a draft or an answer that must not be saved anywhere."
                    },
                    "max_rounds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 16,
                        "description": "Tool-use rounds the agent may take (default 8, capped at 16). Raise it for work that must iterate on a result, such as compiling code and fixing the errors."
                    },
                    "verify": {
                        "type": "object",
                        "description": "Checked against the agent's real tool trace after it runs. If any requirement is unmet, the answer is prefixed with an `UNVERIFIED — …` line naming what was missing, and store_note then stores it as unsourced_synthesis, never semantic.",
                        "properties": {
                            "answer_requires": {
                                "type": "array",
                                "description": "Calls a claim in the answer stands for: when `when` (a regex; `^` is a line start) matches the answer, each of `tools` (`tool` or `tool:action`) must have run, e.g. [{\"when\": \"^Source: https?://\", \"tools\": [\"create_task\"]}].",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "when": { "type": "string" },
                                        "tools": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["when", "tools"]
                                }
                            },
                            "retry": {
                                "type": "boolean",
                                "description": "When a requirement is unmet, run the agent once more on the task, told what fell short and which calls already ran; the result is the second run's (`retried_after` lists the first run's failures). Not in a probe."
                            },
                            "require_tools": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Tools that must have run and succeeded at least once (e.g. [\"mcp_call\"] to require a page was read); `tool:action` requires that action (e.g. \"manage_plugin:write\"). A probe's stubbed call does not count."
                            },
                            "code_requires_tests": {
                                "type": "boolean",
                                "description": "If the answer contains a ```rust block, require an execute_code run whose tests all passed (at least one)."
                            },
                            "rerun_answer_code": {
                                "type": "boolean",
                                "description": "Compile and test the answer's own ```rust code in the sandbox after the run (and clippy it, with code_requires_clippy_clean), instead of trusting the agent's reported runs."
                            },
                            "code_requires_clippy_clean": {
                                "type": "boolean",
                                "description": "If the answer contains a ```rust block, the last execute_code clippy run must have reported no findings."
                            },
                            "cited_paths_exist": {
                                "type": "boolean",
                                "description": "Every `path:line` the answer cites must exist in the codebase with at least that many lines."
                            },
                            "question": {
                                "type": "string",
                                "description": "The question the agent was asked; read by require_code_if_question_mentions."
                            },
                            "require_code_if_question_mentions": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "If `question` names any of these words (whole word, any case), the answer must contain a ```rust block; with code_requires_tests it must also be backed by a passing test."
                            }
                        }
                    }
                },
                "required": ["agent", "task"]
            }),
        }
    }

    fn select_agent_def() -> ToolDefinition {
        ToolDefinition {
            name: "select_agent".to_string(),
            description:
                "Pick the best-fitting spawnable agent for a goal, without running it. Returns \
                 the chosen agent name (or none, when nothing fits) plus the full roster. Deterministic (specialty match, no model call), so \
                 a chain step can route to an agent cheaply; then call `run_agent` with the \
                 chosen name, or pass `run_agent` `agent: \"auto\"` to do both in one step."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The sub-goal to match an agent against."
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    async fn handle_select_agent(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SelectAgentInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };
        let goal = input.goal.trim();
        if goal.is_empty() {
            return ToolCallResult::error("`goal` must be a non-empty string.");
        }

        let Some(runner) = self.runner.read().await.clone() else {
            return ToolCallResult::error(
                "select_agent is unavailable: no agent loop engine is wired in this deployment. \
                 (It is enabled with SUBAGENT_ENABLED and requires the chat service.)",
            );
        };

        let selection = runner.select_agent(goal).await;
        let roster: Vec<Value> = selection
            .roster
            .iter()
            .map(|(name, specialty)| json!({ "agent": name, "specialty": specialty }))
            .collect();
        // `answer` carries the chosen name (or "none") so a downstream
        // `{{_prev}}` hands the routing decision straight into the next step.
        let answer = selection
            .agent
            .clone()
            .unwrap_or_else(|| "none".to_string());
        ToolCallResult::success_json(json!({
            "answer": answer,
            "agent": selection.agent,
            "score": selection.score,
            "roster": roster,
        }))
    }

    async fn handle_run_agent(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: RunAgentInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };
        let agent = input.agent.trim();
        let task = input.task.trim();
        if agent.is_empty() {
            return ToolCallResult::error("`agent` must be a non-empty spawnable agent name.");
        }
        if task.is_empty() {
            return ToolCallResult::error("`task` must be a non-empty instruction for the agent.");
        }

        let Some(runner) = self.runner.read().await.clone() else {
            return ToolCallResult::error(
                "run_agent is unavailable: no agent loop engine is wired in this deployment. \
                 (It is enabled with SUBAGENT_ENABLED and requires the chat service.)",
            );
        };

        info!(
            agent,
            task_len = task.len(),
            "run_agent dispatching background agent"
        );
        // The call's own `model`, else the chain step's pin (`__model`): a
        // step pin must reach the agent loop, which picks its own config.
        let model = input
            .model
            .as_deref()
            .or(input.step_model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        // Null or empty text is no harness: a workflow's optional input that
        // was not given renders as "".
        let harness = match input
            .harness
            .as_ref()
            .filter(|v| !v.is_null() && v.as_str().is_none_or(|t| !t.trim().is_empty()))
        {
            None => None,
            Some(v) => match parse_harness(v) {
                Ok(h) => {
                    if let Some(leak) = h
                        .system_prompt
                        .as_deref()
                        .and_then(|p| harness_leak(p, task))
                    {
                        return ToolCallResult::error(format!(
                            "candidate `harness` refused: {leak}. A candidate prompt is the agent's \
                             instructions only, never the task it is probed on or a tool list."
                        ));
                    }
                    Some(h)
                }
                Err(e) => return ToolCallResult::error(e),
            },
        };
        let budget = AgentBudget {
            max_iterations: effective_rounds(input.max_rounds),
            // Writing IS the background job; the inner agent keeps its
            // own TurnWriteGuard for grounding/duplicate suppression.
            // `read_only` (an HBI assist drafting into a form) strips them.
            allow_writes: !input.read_only,
            allow_explicit_only: true,
        };
        let opts = RunOptions {
            model: model.map(str::to_string),
            harness,
            probe: input.probe,
        };
        // No sink: collect the findings, do not stream to a user.
        let mut findings = runner
            .run_with(agent, task, budget, None, opts.clone())
            .await;

        // A pre-flight rejection (unknown/non-spawnable agent, empty task,
        // engine gone) is a bug to fix — surface it as a hard tool error. A
        // ran-but-empty result is NOT an error: reporting one would burn a retry
        // and can dead-letter the owning Task, whereas handing the (empty)
        // findings back lets the chain / evaluator react. Same rationale as
        // `execute_code` treating a failed run as a successful tool call.
        if findings.rejected {
            return ToolCallResult::error(findings.findings);
        }
        if !findings.ok {
            warn!(agent, "run_agent: agent ran but produced no answer");
        }
        // Surface the real tool-call trace so a programmatic consumer can check
        // the answer's claims against what actually ran (the answer text already
        // carries a human-readable receipt). See ToolInvocation for why.
        let tool_trace: Vec<Value> = findings
            .tool_trace
            .iter()
            .map(|inv| json!({ "tool": inv.tool, "success": inv.success }))
            .collect();
        // The caller's requirements, checked against what actually ran. A
        // failure does not fail the step (the answer may still be useful); it
        // marks the answer so it is never stored as knowledge.
        let mut failures = match input.verify.as_ref() {
            None => Vec::new(),
            Some(spec) => {
                self.verify(&runner, spec, &findings.findings, &findings.tool_trace)
                    .await
            }
        };
        // `verify.retry`: once more, told what fell short and what already ran
        // (so it does not repeat those calls). Not in a probe, where a stub
        // never meets a requirement.
        let mut first_failures = Vec::new();
        if let Some(spec) = input.verify.as_ref()
            && spec.retry
            && !input.probe
            && findings.ok
            && !failures.is_empty()
        {
            let again = retry_task(
                task,
                &failures,
                &findings.trajectory.calls_line(),
                &findings.findings,
            );
            warn!(agent, failures = ?failures, "run_agent: verification failed; retrying once");
            let second = runner.run_with(agent, &again, budget, None, opts).await;
            if !second.rejected {
                first_failures = std::mem::take(&mut failures);
                failures = self
                    .verify(&runner, spec, &second.findings, &second.tool_trace)
                    .await;
                findings = second;
            }
        }
        let answer = label_proposed_placement(&findings.findings);
        let answer = if failures.is_empty() {
            answer
        } else {
            warn!(agent, failures = ?failures, "run_agent: answer failed verification");
            mark_unverified_answer(&answer, &failures)
        };
        ToolCallResult::success_json(json!({
            "answer": answer,
            "ok": findings.ok,
            "retrieved": findings.retrieved,
            "agent": agent,
            // The model that answered (else the last one tried), every rung
            // the fallback ladder tried with its error, and the refused ones.
            "model": findings.model,
            "attempts": findings.attempts,
            "refused": findings.refused,
            "tool_trace": tool_trace,
            "verified": input.verify.is_some().then_some(failures.is_empty()),
            "verification_failures": failures,
            // Set when `verify.retry` ran the agent a second time: what the
            // first run fell short on. The rest of this result is the second run.
            "retried_after": (!first_failures.is_empty()).then_some(&first_failures),
            // The run step by step, as text a model reads (`render`), and the
            // harness it ran under: what a harness editor and composer need.
            "trajectory": findings.trajectory.render(),
            "harness": findings.harness,
            "probe": input.probe,
        }))
    }
}

impl AgentRunnerSkill {
    /// Check a finished run against the caller's `verify` spec. With
    /// `rerun_answer_code`, the answer's own Rust code is compiled and tested
    /// here, so the trace's test and clippy results are not consulted: what is
    /// shown is what is checked.
    async fn verify(
        &self,
        runner: &AgentRunner,
        spec: &VerifySpec,
        answer: &str,
        trace: &[agent_brain_chat::agent_runner::ToolInvocation],
    ) -> Vec<String> {
        let code = answer_rust_code(answer).filter(|_| !answer.contains("NO FINDINGS"));
        if !spec.rerun_answer_code || code.is_none() {
            return verification_failures(spec, answer, trace, self.codebase_dir.as_deref());
        }
        let trace_spec = VerifySpec {
            code_requires_tests: false,
            code_requires_clippy_clean: false,
            ..spec.clone()
        };
        let mut failures =
            verification_failures(&trace_spec, answer, trace, self.codebase_dir.as_deref());
        failures.extend(self.rerun(runner, spec, &code.unwrap_or_default()).await);
        failures
    }

    /// Compile and test `code` as shown, then lint it, in the rust-sandbox
    /// (the `sandbox` plugin's `execute_code`).
    async fn rerun(&self, runner: &AgentRunner, spec: &VerifySpec, code: &str) -> Vec<String> {
        let manifest = self
            .codebase_dir
            .as_ref()
            .and_then(|d| {
                std::fs::read_to_string(d.join("plugins/sandbox/rust/crates/Cargo.toml")).ok()
            })
            .unwrap_or_default();
        let files = answer_crate(code, &sandbox_dependencies(&manifest));
        let run = |command: &'static str| {
            let files = files.clone();
            async move {
                let result = runner
                    .call_tool(
                        "execute_code",
                        json!({ "language": "rust", "command": command, "files": files }),
                    )
                    .await?;
                let text = result.content.iter().find_map(|c| match c {
                    agent_brain_protocol::Content::Text { text } => Some(text.clone()),
                    _ => None,
                })?;
                serde_json::from_str::<Value>(&text).ok()
            }
        };
        let mut failures = Vec::new();
        match run("test").await {
            None => failures
                .push("the verifier could not re-run the answer's code in the sandbox".to_string()),
            Some(r) => {
                let passed = r["tests_passed"].as_u64().unwrap_or(0);
                let failed = r["tests_failed"].as_u64().unwrap_or(0);
                if !(r["success"] == true && passed > 0 && failed == 0) {
                    let summary = r["summary"].as_str().unwrap_or("no summary");
                    let error = first_compile_error(r["stderr"].as_str().unwrap_or(""))
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default();
                    failures.push(format!(
                        "the answer's own Rust code, re-run by the verifier, did not pass: \
                         {}{error}",
                        summary.trim_end_matches('.')
                    ));
                    info!(stderr = %r["stderr"].as_str().unwrap_or("").chars().take(1500).collect::<String>(), "run_agent verifier: answer code failed");
                }
            }
        }
        if spec.code_requires_clippy_clean && failures.is_empty() {
            match run("clippy").await {
                Some(r) if r["success"] == true && r["lint_findings"].as_u64() == Some(0) => {}
                Some(r) => failures.push(format!(
                    "clippy on the answer's own code, re-run by the verifier: {}",
                    r["summary"].as_str().unwrap_or("not clean")
                )),
                None => failures
                    .push("the verifier could not run clippy on the answer's code".to_string()),
            }
        }
        failures
    }
}

#[async_trait]
impl Skill for AgentRunnerSkill {
    fn name(&self) -> &str {
        "Agent Runner"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::run_agent_def(), Self::select_agent_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "run_agent" => Some(self.handle_run_agent(arguments).await),
            "select_agent" => Some(self.handle_select_agent(arguments).await),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RunAgentInput {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    max_rounds: Option<usize>,
    #[serde(default)]
    verify: Option<VerifySpec>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    probe: bool,
    /// A candidate harness: an object, or that object as JSON text (what a
    /// workflow template renders), read by [`parse_harness`].
    #[serde(default)]
    harness: Option<Value>,
    /// A chain step's pinned model, stamped into the job args by the queue.
    #[serde(default, rename = "__model")]
    step_model: Option<String>,
}

/// A candidate harness from `run_agent`'s `harness`: its `system_prompt` and
/// `tools`, other keys ignored, so an editor's whole candidate (with its
/// rationale) can be handed over as it came. JSON text is parsed first.
fn parse_harness(v: &Value) -> Result<Harness, String> {
    let parsed;
    let v = match v {
        Value::String(text) => {
            parsed = agent_brain_llm::llm::first_json_object(text, &[])
                .ok_or("`harness` is text with no JSON object in it")?;
            &parsed
        }
        other => other,
    };
    let Some(obj) = v.as_object() else {
        return Err("`harness` must be an object with `system_prompt` and/or `tools`".into());
    };
    let system_prompt = match obj.get("system_prompt") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) if !p.trim().is_empty() => Some(p.clone()),
        Some(_) => return Err("`harness.system_prompt` must be non-empty text".into()),
    };
    let tools = match obj.get("tools") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|t| t.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or("`harness.tools` must be a list of tool names")?,
        ),
        Some(_) => return Err("`harness.tools` must be a list of tool names".into()),
    };
    if system_prompt.is_none() && tools.is_none() {
        return Err("`harness` names neither `system_prompt` nor `tools`".into());
    }
    Ok(Harness {
        system_prompt,
        tools,
    })
}

/// The task for a `verify.retry` run: the original, then what the first run
/// fell short on and what it already did.
fn retry_task(task: &str, failures: &[String], calls: &str, answer: &str) -> String {
    let head: String = answer.chars().take(1500).collect();
    format!(
        "{task}\n\n===== YOUR PREVIOUS RUN OF THIS TASK WAS CHECKED AND FELL SHORT\n{}\n\
         Its calls already ran and must not be repeated ({calls}).\n\
         Its answer was:\n{head}\n\
         Do only what is missing, then give the whole answer again in the same format.\n\
         ===== END",
        failures
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// What a candidate prompt carries that is not instructions: the probe's own
/// task pasted in, or a `TOOLS: [...]` line. On 2026-10-23 a composed intake
/// prompt ended with the editor's TOOLS list and the whole test task, and
/// the probes run on it answered with a stray TOOLS line.
fn harness_leak(prompt: &str, task: &str) -> Option<String> {
    let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let p = norm(prompt);
    let head: String = norm(task).chars().take(120).collect();
    if head.chars().count() >= 40 && p.contains(&head) {
        return Some("it contains the task it is being probed on".into());
    }
    if prompt
        .lines()
        .any(|l| l.trim_start().starts_with("TOOLS: [") || l.trim_start().starts_with("TOOLS: {"))
    {
        return Some("it contains a `TOOLS: [...]` line".into());
    }
    None
}

#[derive(Debug, Deserialize)]
struct SelectAgentInput {
    #[serde(default)]
    goal: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_says_what_fell_short_and_what_already_ran() {
        let t = retry_task(
            "Hand-off: x",
            &["the answer says `Source: https://a.b`, but no successful `create_task` call this run".into()],
            "CALLS: search_web → read_page → manage_todo",
            "Source: https://a.b",
        );
        assert!(t.starts_with("Hand-off: x\n\n===== YOUR PREVIOUS RUN"));
        assert!(
            t.contains("- the answer says")
                && t.contains("must not be repeated (CALLS: search_web")
        );
    }

    #[test]
    fn a_candidate_prompt_carrying_its_task_or_a_tool_list_is_caught() {
        let task = "A person handed the brain this in chat: https://youtu.be/x\nTheir note: worth a look for our own agents";
        let clean = "You decide what to do with something a person handed the brain.\nReply: What / Source.";
        assert_eq!(harness_leak(clean, task), None);
        let leaked = format!("{clean}\nTOOLS: [\"search_web\"]\nTASK: {task}");
        assert!(harness_leak(&leaked, task).unwrap().contains("task"));
        let tools_only = format!("{clean}\nTOOLS: [\"search_web\",\"read_page\"]");
        assert!(harness_leak(&tools_only, task).unwrap().contains("TOOLS"));
        // A short task cannot match by accident.
        assert_eq!(harness_leak("Do X, then report.", "Do X"), None);
    }

    #[test]
    fn a_harness_is_read_from_an_object_or_its_json_text() {
        let h = parse_harness(
            &json!({ "system_prompt": "Do X.", "tools": ["search_web"], "rationale": "why" }),
        )
        .unwrap();
        assert_eq!(h.system_prompt.as_deref(), Some("Do X."));
        assert_eq!(h.tools, Some(vec!["search_web".to_string()]));
        let h = parse_harness(&json!("Candidate:\n{\"system_prompt\": \"Do Y.\"}")).unwrap();
        assert_eq!(h.system_prompt.as_deref(), Some("Do Y."));
        assert!(h.tools.is_none());
        assert!(parse_harness(&json!({ "rationale": "only" })).is_err());
        assert!(parse_harness(&json!({ "system_prompt": "" })).is_err());
        assert!(parse_harness(&json!({ "tools": "search_web" })).is_err());
        assert!(parse_harness(&json!("no json here")).is_err());
    }

    fn skill_with_no_runner() -> AgentRunnerSkill {
        AgentRunnerSkill::new(Arc::new(RwLock::new(None)))
    }

    #[test]
    fn max_rounds_defaults_and_is_clamped() {
        assert_eq!(effective_rounds(None), MAX_BACKGROUND_AGENT_ITERATIONS);
        assert_eq!(effective_rounds(Some(12)), 12);
        assert_eq!(effective_rounds(Some(0)), 1);
        assert_eq!(effective_rounds(Some(500)), MAX_BACKGROUND_AGENT_ROUNDS_CAP);
    }

    #[test]
    fn exposes_run_agent_and_select_agent() {
        let skill = skill_with_no_runner();
        let tools = skill.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"run_agent"));
        assert!(names.contains(&"select_agent"));
    }

    #[tokio::test]
    async fn empty_agent_is_a_hard_error() {
        let skill = skill_with_no_runner();
        let res = skill
            .handle_run_agent(Some(json!({ "agent": "  ", "task": "do a thing" })))
            .await;
        assert_eq!(res.is_error, Some(true));
    }

    #[tokio::test]
    async fn empty_task_is_a_hard_error() {
        let skill = skill_with_no_runner();
        let res = skill
            .handle_run_agent(Some(json!({ "agent": "researcher", "task": "" })))
            .await;
        assert_eq!(res.is_error, Some(true));
    }

    #[tokio::test]
    async fn unavailable_runner_reports_cleanly() {
        // With no runner wired (stdio / feature off), a well-formed call must
        // report unavailability rather than panic or silently succeed.
        let skill = skill_with_no_runner();
        let res = skill
            .handle_run_agent(Some(
                json!({ "agent": "researcher", "task": "summarise X" }),
            ))
            .await;
        assert_eq!(res.is_error, Some(true));
    }
}
