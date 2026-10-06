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

use crate::services::agent_runner::{
    AgentBudget, AgentRunner, VerifySpec, answer_crate, answer_rust_code, sandbox_dependencies,
    verification_failures,
};
use crate::skills::Skill;
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
                 build, review), not for a single deterministic tool call. Prefer an existing \
                 agent over constructing a new one: pass `agent: \"auto\"` to auto-pick the \
                 best-fitting agent, or a specific name. Only construct a bespoke agent when \
                 `run_agent` reports that none fit. The agent does NOT see the rest of the chain \
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
                            "require_tools": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Tools that must have succeeded at least once (e.g. [\"mcp_call\"] to require a page was read)."
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
                 the chosen agent name (or none, when nothing fits — a signal to construct a new \
                 agent) plus the full roster. Deterministic (specialty match, no model call), so \
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
        let findings = runner
            .run(
                agent,
                task,
                AgentBudget {
                    max_iterations: effective_rounds(input.max_rounds),
                    // Writing IS the background job; the inner agent keeps its
                    // own TurnWriteGuard for grounding/duplicate suppression.
                    allow_writes: true,
                    allow_explicit_only: true,
                },
                // No sink: collect the findings, do not stream to a user.
                None,
            )
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
        let failures = match input.verify.as_ref() {
            None => Vec::new(),
            Some(spec) => {
                self.verify(&runner, spec, &findings.findings, &findings.tool_trace)
                    .await
            }
        };
        let answer = if failures.is_empty() {
            findings.findings
        } else {
            warn!(agent, failures = ?failures, "run_agent: answer failed verification");
            format!(
                "UNVERIFIED — {}. Treat everything below as unchecked.\n\n{}",
                failures.join("; "),
                findings.findings
            )
        };
        ToolCallResult::success_json(json!({
            "answer": answer,
            "ok": findings.ok,
            "retrieved": findings.retrieved,
            "agent": agent,
            "tool_trace": tool_trace,
            "verified": input.verify.is_some().then_some(failures.is_empty()),
            "verification_failures": failures,
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
        trace: &[crate::services::agent_runner::ToolInvocation],
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

    /// Compile and test `code` as shown, then lint it, in the rust-sandbox.
    async fn rerun(&self, runner: &AgentRunner, spec: &VerifySpec, code: &str) -> Vec<String> {
        let manifest = self
            .codebase_dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join("rust-sandbox/crates/Cargo.toml")).ok())
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
                    failures.push(format!(
                        "the answer's own Rust code, re-run by the verifier, did not pass: {summary}"
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
}

#[derive(Debug, Deserialize)]
struct SelectAgentInput {
    #[serde(default)]
    goal: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
