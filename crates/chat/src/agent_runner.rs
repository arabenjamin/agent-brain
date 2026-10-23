//! Shared agent-execution primitive.
//!
//! `AgentRunner::run` is the one place a **profile-driven, bounded, tool-using
//! agent loop** lives. It was lifted verbatim out of
//! `ChatService::run_subagent` (Phase 0 of
//! `project-docs/UNIFIED_AGENT_EXECUTION_PLAN.md`) so that all three goal
//! entrypoints — chat, tasks, schedules — can reach the *same* loop instead of
//! only chat having one. Chat's `run_subagent` is now a thin streaming wrapper
//! over this; the background `run_agent` tool (Phase 1) will call it with no
//! sink.
//!
//! ## Why the loop machinery stays in `ChatService`
//!
//! The chat round loop with its provider adapters (`run_anthropic_loop`,
//! `run_ollama_tool_loop`, `run_openai_loop`) and profile-model resolution are
//! large, incident-laden `ChatService` methods that encode the fallback ladder,
//! the write guard, empty-completion retries, and the worker/voice split. Moving
//! them wholesale would be a behaviour change dressed as a refactor — exactly
//! what the plan says *not* to do. Instead this service reaches them through the
//! [`SubagentLoopRunner`] trait, which `ChatService` implements by dispatching to
//! its existing loops unchanged. `AgentRunner` owns the *orchestration* (resolve
//! profile → build toolset → run one bounded loop → drain findings); the engine
//! owns the *loop*. The cycle (`ChatService` holds an `Arc<AgentRunner>`;
//! `AgentRunner` holds a `Weak<dyn SubagentLoopRunner>` back to it) is broken
//! with a `Weak`, wired via `Arc::new_cyclic`.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{RwLock, mpsc};

use crate::chat::{ChatEvent, ChatRequest};
use crate::chat::{
    DELEGATE_TOOL, PROBE_STUB_PREFIX, USAGE_TOOL_LABEL, WRITE_TOOLS, build_system_prompt,
    filter_tools,
};
use crate::trajectory::Trajectory;
use agent_brain_knowledge::knowledge::RETRIEVAL_EXCLUDE;
use agent_brain_llm::llm::LlmConfig;
use agent_brain_llm::routing::{Purpose, ROUTE_CONTEXT, RouteContext, RoutingPolicy};
use agent_brain_work::context_builder::ContextBuilderService;
use agent_brain_work::tools::{ToolHandler, ToolRegistry};

/// The engine an [`AgentRunner`] drives. Implemented by `ChatService`, whose
/// provider loops and catalog-aware model resolution are the shared machinery we
/// deliberately do not duplicate. See the module docs for why this is a trait
/// rather than a physical move of the loops.
#[async_trait::async_trait]
pub trait SubagentLoopRunner: Send + Sync {
    /// Run one bounded provider loop, streaming events to `tx`. `allow_delegate`
    /// is `false` for an agent run (depth cap 1); `max_iterations` bounds the
    /// tool rounds. This is exactly the match `ChatService` already performs.
    #[allow(clippy::too_many_arguments)]
    async fn run_bounded_loop(
        &self,
        cfg: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
        allow_delegate: bool,
        max_iterations: usize,
    ) -> LadderReport;

    /// Resolve a profile's `model_preference` (+ optional `provider_hint`) to a
    /// full config against the live model catalog. `None` keeps `base`.
    async fn resolve_agent_model(
        &self,
        base: &LlmConfig,
        model_pref: &str,
        provider_hint: Option<&str>,
    ) -> Option<LlmConfig>;
}

/// One model the fallback ladder tried for a sub-agent run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RungAttempt {
    pub model: String,
    pub provider: String,
    /// Why this rung gave no answer; `None` for the rung that answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the ladder did for one run: the rungs it tried, in order, and the
/// ones the routing policy refused before trying (`"model: reason"`).
#[derive(Debug, Clone, Default)]
pub struct LadderReport {
    pub attempts: Vec<RungAttempt>,
    pub refused: Vec<String>,
}

/// Execution budget for one agent run.
#[derive(Debug, Clone, Copy)]
pub struct AgentBudget {
    /// Tool-use rounds before the loop is asked for a final answer with no tools.
    pub max_iterations: usize,
    /// Whether the inner agent may call write tools. Chat sub-agents are
    /// read-only (one writer per turn); a background agent writes (writing *is*
    /// the job) but still runs the engine's own `TurnWriteGuard`.
    pub allow_writes: bool,
    /// Whether an `explicit_only` profile may run. True only for `run_agent`
    /// (a step a human wrote into a chain); chat `delegate` passes false, so
    /// an orchestrating model can never pick a paid agent on its own.
    pub allow_explicit_only: bool,
}

/// A candidate harness to run an agent under instead of its profile's
/// (HARNESS2_PLAN Phase 1): its prompt and/or its toolset. Only a probe may
/// carry one, so a candidate never acts on the world.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    /// The whole system prompt, in place of the profile's (and its sections).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// The toolset, in place of the profile's allowlist; still only tools the
    /// registry holds, never `delegate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

/// How one run differs from the profile's own: the model, a candidate harness,
/// and whether it is a probe (every call that is not read-only is stubbed).
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub model: Option<String>,
    pub harness: Option<Harness>,
    pub probe: bool,
}

/// One tool the sub-agent actually invoked, with the outcome the loop reported.
/// This is the ground-truth receipt behind a run: an LLM's prose can claim it
/// "ran X and stored Y" when no such call happened (observed live — the toolsmith
/// narrated an execution it never performed), so the trace travels with the
/// findings for the caller to check the narrative against.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub tool: String,
    pub success: bool,
    /// For an `execute_code` call that ran tests and all of them passed: how
    /// many. `None` for every other call, including a run with no tests, a
    /// compile failure, or any failing test. Read from the tool's own result,
    /// never from the agent's prose.
    pub tests_passed: Option<u64>,
    /// For an `execute_code` clippy run: `Some(true)` when it built and
    /// reported no findings, `Some(false)` otherwise. `None` for anything else.
    pub clippy_clean: Option<bool>,
    /// The call's `action` argument, for a multi-action tool (`manage_plugin`
    /// write vs get): what `verify.require_tools` `"tool:action"` checks.
    pub action: Option<String>,
    /// A probe answered it with a stub: it was not executed.
    pub stubbed: bool,
}

/// Whether an `execute_code` result is a clean clippy run, read from the
/// sandbox's own `lint_findings` count (clippy exits 0 on warnings, so the exit
/// code alone says nothing). `None` when the result is not a clippy run.
pub fn clippy_clean(tool: &str, result: &str) -> Option<bool> {
    use regex::Regex;
    use std::sync::LazyLock;
    static CLIPPY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""command"\s*:\s*"clippy""#).expect("regex"));
    static FINDINGS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""lint_findings"\s*:\s*(\d+)"#).expect("regex"));
    static SUCCESS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""success"\s*:\s*true"#).expect("regex"));
    if tool != "execute_code" || !CLIPPY.is_match(result) {
        return None;
    }
    let findings = FINDINGS
        .captures(result)
        .and_then(|c| c[1].parse::<u64>().ok());
    Some(SUCCESS.is_match(result) && findings == Some(0))
}

/// How many sandbox tests passed, from an `execute_code` result payload: only
/// when the run built, at least one test passed, and none failed.
pub fn passing_tests(tool: &str, result: &str) -> Option<u64> {
    use regex::Regex;
    use std::sync::LazyLock;
    static PASSED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""tests_passed"\s*:\s*(\d+)"#).expect("regex"));
    static FAILED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""tests_failed"\s*:\s*(\d+)"#).expect("regex"));
    static COMPILE_FAILED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""compile_failed"\s*:\s*true"#).expect("regex"));
    if tool != "execute_code" || COMPILE_FAILED.is_match(result) {
        return None;
    }
    let count = |re: &Regex| re.captures(result).and_then(|c| c[1].parse::<u64>().ok());
    let passed = count(&PASSED)?;
    (passed > 0 && count(&FAILED).unwrap_or(0) == 0).then_some(passed)
}

/// The Rust code an answer presents: the first ```rust block that holds a
/// `#[test]`, else the longest ```rust block. `None` when there is none.
///
/// A block ends at a line of only backticks, not at the first ``` in it: on
/// 2026-10-23 an agent's passing tests held `"x\n```rust\n…"` in a string, and
/// the re-run compiled the code cut there ("unterminated double quote string").
pub fn answer_rust_code(answer: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut rest = answer;
    while let Some(start) = rest.find("```rust") {
        let after = &rest[start + "```rust".len()..];
        let body_start = after.find('\n').map_or(after.len(), |i| i + 1);
        let body = &after[body_start..];
        let Some((end, next)) = closing_fence(body) else {
            break;
        };
        blocks.push(body[..end].to_string());
        rest = &body[next..];
    }
    blocks
        .iter()
        .find(|b| b.contains("#[test]"))
        .or_else(|| blocks.iter().max_by_key(|b| b.len()))
        .cloned()
}

/// Where a fenced block's body ends: the start of its closing fence line (a
/// line holding only three or more backticks and whitespace) and the offset
/// just past that line.
fn closing_fence(body: &str) -> Option<(usize, usize)> {
    let mut at = 0;
    for line in body.split_inclusive('\n') {
        let t = line.trim();
        if t.len() >= 3 && t.chars().all(|c| c == '`') {
            return Some((at, at + line.len()));
        }
        at += line.len();
    }
    None
}

/// The `[dependencies]` lines of the sandbox's crate manifest
/// (`plugins/sandbox/rust/crates/Cargo.toml`), keyed by the name code `use`s
/// (`ed25519-dalek` → `ed25519_dalek`). Each line is copied verbatim into a
/// generated manifest, so versions and features stay what the cache holds.
pub fn sandbox_dependencies(manifest: &str) -> Vec<(String, String)> {
    let mut in_deps = false;
    let mut deps = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]";
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            let name = name.trim();
            if !name.is_empty() {
                deps.push((name.replace('-', "_"), trimmed.to_string()));
            }
        }
    }
    deps
}

/// A crate that compiles `code` as written: `src/main.rs` when it has a
/// `fn main`, else `src/lib.rs`, with a manifest declaring every sandbox crate
/// the code names (`sha2::…`, `use hex::…`).
pub fn answer_crate(code: &str, deps: &[(String, String)]) -> serde_json::Value {
    let used: Vec<&str> = deps
        .iter()
        .filter(|(name, _)| {
            code.contains(&format!("{name}::")) || code.contains(&format!("use {name}"))
        })
        .map(|(_, line)| line.as_str())
        .collect();
    let manifest = format!(
        "[package]\nname = \"answer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{}\n",
        used.join("\n")
    );
    let src = if code.contains("fn main") {
        "src/main.rs"
    } else {
        "src/lib.rs"
    };
    serde_json::json!({ "Cargo.toml": manifest, src: code })
}

/// One `verify.answer_requires` entry.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AnswerRequirement {
    pub when: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

/// What a caller requires an agent run to have actually done, checked against
/// its tool trace after the run (`run_agent`'s `verify` argument).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct VerifySpec {
    /// Tools that must have succeeded at least once, executed (a probe's
    /// stub does not count). `"tool:action"` requires that action: on
    /// 2026-10-23 a toolsmith ran `manage_plugin get` three times, never
    /// `write`, reported a write, and passed `require_tools: [manage_plugin]`.
    #[serde(default)]
    pub require_tools: Vec<String>,
    /// Tools a claim in the answer stands for: when an entry's `when` (a
    /// regex, multi-line: `^` is a line start) matches the answer, each of its
    /// `tools` must have run, as in `require_tools`. On 2026-10-23 intake
    /// answered "Source: <url>" in about one run in four without the
    /// create_task that starts the brain reading it.
    #[serde(default)]
    pub answer_requires: Vec<AnswerRequirement>,
    /// When a requirement is unmet, run the agent once more on the same task
    /// with what fell short and the calls that already ran (`run_agent`).
    #[serde(default)]
    pub retry: bool,
    /// If the answer contains a ```rust block, an `execute_code` run with
    /// passing tests must be in the trace.
    #[serde(default)]
    pub code_requires_tests: bool,
    /// Every `path:line` the answer cites must be a file in the codebase with
    /// at least that many lines. A path without a line number may name a file
    /// to create, so it is not checked.
    /// If the answer contains a ```rust block, the LAST clippy run in the
    /// trace must be clean. The last, so a clean run followed by edits that
    /// were linted again and failed does not count.
    #[serde(default)]
    pub code_requires_clippy_clean: bool,
    /// Re-run the answer's own Rust code in the sandbox after the agent
    /// finishes (`run_agent`), instead of trusting the trace: the code shown
    /// must pass `cargo test` (and clippy, with `code_requires_clippy_clean`).
    /// On 2026-10-05 an agent ran some other code that passed, then showed a
    /// test with an invented expected hash and "test result: ok. 1 passed" —
    /// the code shown returns a different value and could never pass.
    #[serde(default)]
    pub rerun_answer_code: bool,
    #[serde(default)]
    pub cited_paths_exist: bool,
    /// The question the agent was asked (a workflow passes `{{inputs.question}}`).
    #[serde(default)]
    pub question: String,
    /// If `question` names any of these words (case-insensitive, whole word),
    /// the answer MUST contain a ```rust block backed by a passing sandbox test.
    /// Without this, an agent asked for Rust could write "RUST: not applicable"
    /// and pass every other check (2026-10-05: a Reticulum answer declined the
    /// code as "std-only has no SHA-256", passed, and carried a wrong identity
    /// hash that a test against a known example would have exposed).
    #[serde(default)]
    pub require_code_if_question_mentions: Vec<String>,
}

/// Whether `text` contains any of `words` as a whole word, ignoring case
/// ("rust" matches "Rust" and "rust:", never "trust").
fn mentions_any(text: &str, words: &[String]) -> bool {
    let lower = text.to_lowercase();
    words.iter().any(|w| {
        let w = w.to_lowercase();
        lower.match_indices(&w).any(|(i, _)| {
            let before = lower[..i].chars().next_back();
            let after = lower[i + w.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    })
}

/// `path:line` citations in `answer` that do not exist under `root`: the file
/// is missing, or shorter than the cited line. URLs are not paths and are
/// skipped (a citation must start at a word boundary, never after `://`).
///
/// Why: a 2026-10-03 deep dive placed its code at `crates/app/src/reticulum.rs:12`
/// and `crates/app/src/lib.rs:45` "via the ReticulumSkill" — neither the file
/// nor the skill exists. A citation with a line number asserts the file says
/// something there, and that is checkable.
pub fn missing_cited_paths(answer: &str, root: &std::path::Path) -> Vec<String> {
    use regex::Regex;
    use std::sync::LazyLock;
    static CITED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?:^|[\s(`'"\[])((?:[A-Za-z0-9_.\-]+/)+[A-Za-z0-9_.\-]+\.[A-Za-z0-9]+):(\d+)"#,
        )
        .expect("regex")
    });
    let mut missing = Vec::new();
    for cap in CITED.captures_iter(answer) {
        let path = &cap[1];
        let line: usize = cap[2].parse().unwrap_or(0);
        let cited = format!("{path}:{line}");
        if missing.contains(&cited) {
            continue;
        }
        let lines = std::fs::read_to_string(root.join(path))
            .map(|text| text.lines().count())
            .ok();
        if lines.is_none_or(|n| line == 0 || line > n) {
            missing.push(cited);
        }
    }
    missing
}

/// The first compiler error line in sandbox stderr (`error[E0433]: …`), so a
/// verification failure names what broke instead of "see stderr".
pub fn first_compile_error(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error[") || l.starts_with("error:"))
        .map(|l| l.chars().take(200).collect())
}

/// An answer that failed verification, made safe to store and relay: the
/// UNVERIFIED marker line, and every line OUTSIDE code blocks that reports a
/// passing test run replaced by what the verifier found.
///
/// Prefixing alone left the worker's own "test result: ok. 1 passed" in the
/// body. On 2026-10-07 chat read that line past the marker and told the user
/// the test passed — the verifier had re-run the code and it did not compile.
/// A note that contradicts itself gets relayed by its most specific line.
pub fn mark_unverified_answer(answer: &str, failures: &[String]) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    static PASS_CLAIM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)test result:\s*`?ok\b|\b\d+\s+passed\b").expect("regex"));
    let reason = failures.join("; ");
    let reason = reason.trim_end_matches('.');
    let mut in_code = false;
    let body: Vec<String> = answer
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
                return line.to_string();
            }
            if !in_code && PASS_CLAIM.is_match(line) {
                format!(
                    "Test result: NOT CONFIRMED — the worker reported a passing run, \
                     but verification failed: {reason}."
                )
            } else {
                line.to_string()
            }
        })
        .collect();
    format!(
        "{UNVERIFIED_MARKER} {reason}. Treat everything below as unchecked.\n\n{}",
        body.join("\n")
    )
}

/// The deep-dive `## IN AGENT-BRAIN` heading, renamed to say what it is: a
/// placement proposal. No agent run edits the repository, and on 2026-10-07
/// chat relayed that section as "What was actually built".
pub fn label_proposed_placement(answer: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    // The model writes the hyphen as U+2011 as often as '-'.
    static HEADING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?mi)^(#{1,6})\s*IN AGENT[-\u{2010}\u{2011}\u{2013}]BRAIN\s*$").expect("regex")
    });
    HEADING
        .replace_all(answer, "$1 IN AGENT-BRAIN (proposed — nothing was changed)")
        .into_owned()
}

/// Marker [`mark_unverified_answer`] puts on the first line; `store_note`
/// detects it and stores the answer as failed verification.
pub const UNVERIFIED_MARKER: &str = "UNVERIFIED —";

/// Each requirement of `spec` the run did not meet, in plain words. Empty
/// means verified. A `NO FINDINGS` answer claims nothing and is not checked.
pub fn verification_failures(
    spec: &VerifySpec,
    answer: &str,
    trace: &[ToolInvocation],
    codebase_root: Option<&std::path::Path>,
) -> Vec<String> {
    if answer.contains("NO FINDINGS") {
        return Vec::new();
    }
    let mut failures = Vec::new();
    // `tool` or `tool:action`, run and succeeded (a probe's stub is not).
    let unmet = |required: &str| -> Option<String> {
        let (tool, action) = match required.split_once(':') {
            Some((t, a)) => (t.trim(), Some(a.trim())),
            None => (required.trim(), None),
        };
        let met = trace.iter().any(|t| {
            t.tool == tool
                && t.success
                && !t.stubbed
                && action.is_none_or(|a| t.action.as_deref() == Some(a))
        });
        (!met).then(|| match action {
            Some(a) => format!("no successful `{tool}` call with action `{a}` this run"),
            None => format!("no successful `{tool}` call this run"),
        })
    };
    failures.extend(spec.require_tools.iter().filter_map(|r| unmet(r)));
    for req in &spec.answer_requires {
        match regex::RegexBuilder::new(&req.when).multi_line(true).build() {
            Err(e) => failures.push(format!(
                "answer_requires `{}` does not compile: {e}",
                req.when
            )),
            Ok(re) => {
                let Some(m) = re.find(answer) else { continue };
                // The whole line the claim sits on, as the answer wrote it.
                let start = answer[..m.start()].rfind('\n').map_or(0, |p| p + 1);
                let end = answer[m.end()..]
                    .find('\n')
                    .map_or(answer.len(), |p| m.end() + p);
                let line = answer[start..end].trim();
                for tool in &req.tools {
                    if let Some(why) = unmet(tool) {
                        failures.push(format!("the answer says `{line}`, but {why}"));
                    }
                }
            }
        }
    }
    if spec.code_requires_tests
        && answer.contains("```rust")
        && !trace.iter().any(|t| t.tests_passed.is_some())
    {
        failures.push(
            "the answer shows Rust code, but no sandbox test run passed (execute_code with \
             command \"test\" and at least one passing #[test])"
                .to_string(),
        );
    }
    if spec.code_requires_clippy_clean && answer.contains("```rust") {
        match trace.iter().rev().find_map(|t| t.clippy_clean) {
            Some(true) => {}
            Some(false) => {
                failures.push("the last clippy run on the Rust code reported findings".to_string())
            }
            None => failures.push(
                "the answer shows Rust code, but clippy was never run on it \
                 (execute_code with command \"clippy\")"
                    .to_string(),
            ),
        }
    }
    if mentions_any(&spec.question, &spec.require_code_if_question_mentions)
        && !answer.contains("```rust")
    {
        failures.push(
            "the question asks for Rust, but the answer contains no Rust code (a ```rust \
             block with a passing sandbox test is required)"
                .to_string(),
        );
    }
    if spec.cited_paths_exist {
        match codebase_root {
            Some(root) => {
                let missing = missing_cited_paths(answer, root);
                if !missing.is_empty() {
                    failures.push(format!(
                        "cited file:line that does not exist in the codebase: {}",
                        missing.join(", ")
                    ));
                }
            }
            None => failures.push("cited paths could not be checked: no CODEBASE_DIR".to_string()),
        }
    }
    failures
}

/// Structured result of one agent run — the same information
/// `ChatService::run_subagent` returned as a tuple, named.
#[derive(Debug, Clone)]
pub struct AgentFindings {
    /// The agent produced a non-empty final answer.
    pub ok: bool,
    /// The run was rejected **before** the loop ran — a config/infra error:
    /// an empty task, an unknown or non-`spawnable` agent, a missing engine, or
    /// a profile that failed to load. Distinct from `ok: false` after a real
    /// run (the agent ran but produced nothing). A caller exposing this as a
    /// tool surfaces a rejection as a hard error (it is a bug to fix) but a
    /// ran-but-empty result as a non-fatal step result (so it does not
    /// dead-letter the chain).
    pub rejected: bool,
    /// The agent ran ≥1 successful tool — it actually consulted the world. The
    /// caller uses this to ground its own write guard (a delegated retrieval the
    /// guard cannot otherwise see).
    pub retrieved: bool,
    /// The formatted findings block (on success) or the error text (on failure)
    /// — ready to fold into the caller's turn / chain step. On success the block
    /// ends with a ground-truth receipt of the tools that actually ran.
    pub findings: String,
    /// Every tool the sub-agent actually invoked, in order, with outcome. The
    /// receipt behind `findings`: a caller can check the prose's claims against
    /// what really happened (empty = the agent ran no tools at all).
    pub tool_trace: Vec<ToolInvocation>,
    /// The model that answered, else the last one tried; empty for a
    /// rejection or when no rung could run. Not the profile's preference: on
    /// 2026-10-11 a run reported `gpt-oss:120b-cloud`, which routing had
    /// refused, beside granite4's timeout error.
    pub model: String,
    /// Every rung the fallback ladder tried, in order, with why each failed.
    pub attempts: Vec<RungAttempt>,
    /// Rungs the routing policy refused before trying (`"model: reason"`).
    pub refused: Vec<String>,
    /// The run step by step: each call's arguments, outcome and result head,
    /// then the answer. What a composer contrasts (HARNESS2_PLAN).
    pub trajectory: Trajectory,
    /// The harness the run actually used: the profile's prompt (or the
    /// candidate's) and the tools it was offered. Empty for a rejection.
    pub harness: Harness,
}

impl AgentFindings {
    /// A pre-flight rejection (config/infra), before any loop ran.
    fn rejected(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            rejected: true,
            retrieved: false,
            findings: msg.into(),
            tool_trace: Vec::new(),
            model: String::new(),
            attempts: Vec::new(),
            refused: Vec::new(),
            trajectory: Trajectory::default(),
            harness: Harness::default(),
        }
    }
}

/// The answering model, else the last one tried.
fn model_that_ran(report: &LadderReport) -> String {
    report
        .attempts
        .iter()
        .find(|a| a.error.is_none())
        .or(report.attempts.last())
        .map(|a| a.model.clone())
        .unwrap_or_default()
}

/// Why a run produced no answer, rung by rung: each model tried with its own
/// error, then the refused ones. One error alone hid the cause on 2026-10-11:
/// the report showed granite4's timeout while the Mistral 400s that started
/// the failure were only in the log.
fn no_answer_text(agent: &str, report: &LadderReport, last_error: Option<&str>) -> String {
    let short = |e: &str| -> String {
        let e = e.trim();
        if e.chars().count() > 240 {
            format!("{}…", e.chars().take(240).collect::<String>())
        } else {
            e.to_string()
        }
    };
    let mut out = format!("Sub-agent `{agent}` returned no answer.");
    if report.attempts.is_empty() {
        if let Some(e) = last_error {
            out.push_str(&format!(" {}", short(e)));
        }
    } else {
        out.push_str(" Tried:");
        for a in &report.attempts {
            let why = a.error.as_deref().map(short);
            out.push_str(&format!(
                "\n- {} ({}): {}",
                a.model,
                a.provider,
                why.as_deref().unwrap_or("ran, but its answer was empty")
            ));
        }
    }
    if !report.refused.is_empty() {
        out.push_str(&format!(
            "\nRefused by the routing policy: {}",
            report.refused.join("; ")
        ));
    }
    out
}

/// Runs a profile as a bounded agent loop and returns structured findings.
pub struct AgentRunner {
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
    /// The loop engine (`ChatService`). `Weak` breaks the ownership cycle —
    /// `ChatService` owns the `AgentRunner`.
    engine: Weak<dyn SubagentLoopRunner>,
}

impl AgentRunner {
    /// Call one registered tool directly, outside any agent loop. `None` when
    /// no tool handler is wired yet. Used by `run_agent`'s verifier to re-run
    /// an answer's code in the sandbox.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Option<agent_brain_protocol::ToolCallResult> {
        let handler = self.tool_handler.read().await.clone()?;
        Some(handler.execute(name, Some(arguments)).await)
    }

    pub fn new(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
        engine: Weak<dyn SubagentLoopRunner>,
    ) -> Self {
        Self {
            tool_handler,
            tool_registry,
            llm_config,
            context_builder,
            engine,
        }
    }

    /// Run `agent` (a `spawnable` context profile) against `task`, a
    /// self-contained sub-goal. When `sink` is `Some`, the agent's activity is
    /// forwarded to it (chat streams the worker to the user); when `None`, the
    /// findings are collected silently (the background path).
    ///
    /// This is the body that used to be `ChatService::run_subagent`, unchanged in
    /// logic. The two differences are parameterisation: the loop is reached
    /// through the engine trait, and the progress/finish stream events are gated
    /// on `sink` being present.
    pub async fn run(
        &self,
        agent: &str,
        task: &str,
        budget: AgentBudget,
        sink: Option<mpsc::Sender<ChatEvent>>,
    ) -> AgentFindings {
        self.run_with_model(agent, task, budget, sink, None).await
    }

    /// [`run`](Self::run) on an explicit model (catalog `model` id or `name`)
    /// instead of the profile's `model_preference`. The pin is judged by the
    /// routing policy for this run; an unknown or refused pin rejects the run
    /// with the reason, never falls back to the default (`run_agent`'s `model`).
    pub async fn run_with_model(
        &self,
        agent: &str,
        task: &str,
        budget: AgentBudget,
        sink: Option<mpsc::Sender<ChatEvent>>,
        model: Option<&str>,
    ) -> AgentFindings {
        let opts = RunOptions {
            model: model.map(str::to_string),
            ..Default::default()
        };
        self.run_with(agent, task, budget, sink, opts).await
    }

    /// [`run_with_model`](Self::run_with_model) with a candidate harness and
    /// probe mode as well. A harness without `probe` is refused: a prompt no
    /// one adopted never runs with real effects.
    pub async fn run_with(
        &self,
        agent: &str,
        task: &str,
        budget: AgentBudget,
        sink: Option<mpsc::Sender<ChatEvent>>,
        opts: RunOptions,
    ) -> AgentFindings {
        let RunOptions {
            model,
            harness,
            probe,
        } = opts;
        if harness.is_some() && !probe {
            return AgentFindings::rejected(
                "a candidate `harness` runs only as a probe (`probe: true`), so it acts on nothing.",
            );
        }
        let model = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        let agent = agent.trim();
        if task.trim().is_empty() {
            return AgentFindings::rejected("agent run called with an empty task.");
        }

        // Choose-or-build (plan §4): `agent: "auto"` picks the best-fitting
        // spawnable agent by specialty match — the "choose" half, LLM-free. When
        // nothing matches lexically, it falls back to a generalist rather than
        // rejecting, so an open-ended auto task still runs (the standalone
        // `select_agent` tool stays honest and returns None = a build cue).
        let selected;
        let agent: &str = if agent.eq_ignore_ascii_case("auto") {
            let sel = self.select_agent(task).await;
            match sel.agent.or_else(|| generalist_default(&sel.roster)) {
                Some(name) => {
                    tracing::info!(agent = %name, "run_agent(auto) selected agent");
                    selected = name;
                    &selected
                }
                // Only truly unreachable when the roster is empty (no spawnable
                // agents exist at all) — then there is nothing to run.
                None => {
                    return AgentFindings::rejected(
                        "run_agent(auto): no spawnable agents exist to run.",
                    );
                }
            }
        } else {
            agent
        };

        let Some(engine) = self.engine.upgrade() else {
            return AgentFindings::rejected("Agent unavailable: loop engine dropped.");
        };

        let Some(cb) = self.context_builder.read().await.clone() else {
            return AgentFindings::rejected("Agent unavailable: no context builder.");
        };

        // Spawnability is checked against the profile's `spawnable` flag BEFORE
        // building the bundle, so a rejected agent never runs its pre_load_query.
        // A non-spawnable or unknown name gets the live roster of what it could
        // have picked.
        let profile = cb.get_profile(agent).await;
        if !budget.allow_explicit_only && profile.as_ref().is_some_and(|p| p.explicit_only) {
            return AgentFindings::rejected(format!(
                "'{agent}' runs only from a task a human marked for it (e.g. a \
                 \"deep dive (hard):\" goal), never by delegation. Use a regular agent."
            ));
        }
        let spawnable_ok = profile.is_some_and(|p| p.spawnable);
        if !spawnable_ok {
            let roster = cb
                .spawnable_profiles()
                .await
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            return AgentFindings::rejected(format!(
                "'{agent}' is not a sub-agent you can delegate to. Available: {roster}."
            ));
        }

        let Ok(bundle) = cb.build_bundle_for(agent, Some(task)).await else {
            return AgentFindings::rejected(format!(
                "Sub-agent profile '{agent}' could not be loaded."
            ));
        };

        // Which models this run may use is the routing policy's call, not a
        // provider-type allow-list. `purpose: subagent` keeps paid providers out
        // unless ROUTING_PAID_FOR names subagents and refuses providers whose
        // declared limits cannot carry a multi-round loop (the Gemini 5-RPM
        // trap). The data class is the stricter of the caller's and the
        // profile's: a private chat handing a task to a public agent stays
        // private, because the task text is the caller's data.
        let route_ctx = RouteContext::new(
            Purpose::Subagent,
            RouteContext::current()
                .data_class
                .stricter(bundle.profile.data_class.unwrap_or_default()),
        );
        // Base config = the current chat model; a profile `model_preference` may
        // override it when the policy admits the pin for this run. A refused pin
        // falls back to `base`; the ladder walk then judges base and every rung.
        let base = self.llm_config.read().await.clone().unwrap_or_default();
        let pinned = match model {
            Some(m) => {
                let rows = match self.tool_handler.read().await.as_ref() {
                    Some(h) => h
                        .telemetry()
                        .map(agent_brain_llm::model_router::catalog_models)
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                match agent_brain_llm::model_router::pinned_config(&rows, m, &base, route_ctx) {
                    Ok(cfg) => Some(cfg),
                    Err(reason) => {
                        return AgentFindings::rejected(format!(
                            "'{agent}' was asked to run on a model it cannot use: {reason}."
                        ));
                    }
                }
            }
            None => None,
        };
        let cfg = if let Some(cfg) = pinned {
            cfg
        } else {
            match bundle
                .profile
                .model_preference
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(pref) => {
                    let resolved = engine
                        .resolve_agent_model(&base, pref, bundle.profile.provider_hint.as_deref())
                        .await
                        .unwrap_or_else(|| base.clone());
                    match RoutingPolicy::from_env().admit(&resolved, route_ctx) {
                        Ok(()) => resolved,
                        Err(reason) => {
                            tracing::warn!(
                                agent = %agent,
                                model = %resolved.model,
                                provider = %resolved.provider_label(),
                                data_class = %route_ctx.data_class,
                                reason = %reason,
                                "sub-agent model_preference refused by routing policy; \
                                 using the chat default"
                            );
                            base.clone()
                        }
                    }
                }
                None => base.clone(),
            }
        };

        // Toolset: never `delegate` (enforces depth cap 1). Write tools are
        // stripped unless the budget allows writes (chat sub-agents are
        // read-only; a background agent writes but keeps the engine write guard).
        // A probe keeps the write tools in view (they are stubbed, not run):
        // what the agent would do with them is the point of the probe.
        let all_tools = self.tool_registry.read().await.list();
        let candidate_tools = harness.as_ref().and_then(|h| h.tools.clone());
        let allowed: Vec<String> = candidate_tools
            .as_ref()
            .unwrap_or(&bundle.profile.tools)
            .iter()
            .filter(|t| {
                t.as_str() != DELEGATE_TOOL
                    && (budget.allow_writes || probe || !WRITE_TOOLS.contains(&t.as_str()))
            })
            .cloned()
            .collect();
        let sub_tools = filter_tools(all_tools, &allowed);
        let handler = self.tool_handler.read().await.clone();
        let prompt_used = harness
            .as_ref()
            .and_then(|h| h.system_prompt.clone())
            .unwrap_or_else(|| bundle.profile.system_prompt.clone());
        let harness_used = Harness {
            system_prompt: Some(prompt_used.clone()),
            tools: Some(sub_tools.iter().map(|t| t.name.clone()).collect()),
        };
        let sub_system = build_system_prompt(
            (!prompt_used.is_empty()).then_some(prompt_used.as_str()),
            &bundle.pre_loaded_notes,
        );
        let sub_request = ChatRequest {
            message: task.to_string(),
            history: Vec::new(),
            session_id: None,
            tools: None,
            context_profile: None,
            // The sub-agent speaks for itself — no nested worker/voice split.
            synthesis_provider: None,
            synthesis_model: None,
            // The run's model is `cfg`, chosen above.
            model: None,
        };

        let started = std::time::Instant::now();
        // Who asked (chat or a background job), read before the run scopes
        // its own route context; and the ledger to record the run in.
        let caller = RouteContext::current().purpose;
        let caller = if probe { "probe" } else { caller.as_str() };
        let telemetry = self
            .tool_handler
            .read()
            .await
            .as_ref()
            .and_then(|h| h.telemetry().cloned());
        if let Some(ref tx) = sink {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "🔎 delegating to `{agent}` sub-agent (first choice {})…",
                        cfg.model
                    ),
                })
                .await;
        }

        let (sub_tx, mut sub_rx) = mpsc::channel::<ChatEvent>(128);
        // allow_delegate = false: an agent cannot delegate further (depth cap 1).
        // Scope the usage-ledger label to `subagent:<agent>` around the loop so
        // every LLM call it makes (primary + fallback rungs) is attributed to the
        // sub-agent, not to `chat` — the whole loop chain runs inline under
        // `join!`, so the task-local propagates. `delegate` and `run_agent` both
        // reach the loop through here, so this is the one place that fixes both.
        // ROUTE_CONTEXT is scoped the same way, so the ladder walk and every tool
        // the agent runs are judged as this sub-agent's work.
        // RETRIEVAL_EXCLUDE binds the profile's `retrieval_exclude` to every
        // search the agent makes; an absent one scopes an empty (no-op) value.
        let retrieval_exclude = bundle.profile.retrieval_exclude.clone().unwrap_or_default();
        let provider_loop = agent_brain_llm::ctx::PROBE_RUN.scope(
            probe,
            ROUTE_CONTEXT.scope(
                route_ctx,
                // A probe's calls are `probe:<agent>`, kept out of the agent's
                // scorecard (it reads `subagent:%`).
                USAGE_TOOL_LABEL.scope(
                    if probe {
                        format!("probe:{agent}")
                    } else {
                        format!("subagent:{agent}")
                    },
                    RETRIEVAL_EXCLUDE.scope(
                        retrieval_exclude,
                        agent_brain_llm::ctx::CURRENT_AGENT.scope(
                            Some(agent.to_string()),
                            engine.run_bounded_loop(
                                cfg,
                                sub_tools,
                                handler,
                                sub_request,
                                sub_system,
                                sub_tx,
                                false,
                                budget.max_iterations,
                            ),
                        ),
                    ),
                ),
            ),
        );

        let drain = async {
            let mut answer = String::new();
            let mut saw_error: Option<String> = None;
            // Successful tool-result previews, kept to extract the sources the
            // caller must cite. The worker's own tool outputs are the only
            // trustworthy provenance — its prose can paraphrase or drop URLs.
            let mut gathered: Vec<String> = Vec::new();
            // The real tool-call trace — receipts to check the prose against.
            let mut trace: Vec<ToolInvocation> = Vec::new();
            // Each call's arguments wait here for its result (results come
            // back in call order).
            let mut pending: std::collections::VecDeque<(String, serde_json::Value)> =
                std::collections::VecDeque::new();
            let mut trajectory = Trajectory::default();
            while let Some(ev) = sub_rx.recv().await {
                match ev {
                    ChatEvent::Message { content } => answer = content,
                    ChatEvent::Error { message } => {
                        if saw_error.is_none() {
                            saw_error = Some(message);
                        }
                    }
                    ChatEvent::Done => {}
                    ChatEvent::ToolCall { tool, args } => {
                        pending.push_back((tool.clone(), args.clone()));
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::ToolCall {
                                    tool: format!("{agent}:{tool}"),
                                    args,
                                })
                                .await;
                        }
                    }
                    ChatEvent::ToolResult {
                        tool,
                        success,
                        preview,
                        model_saw_chars,
                    } => {
                        let args = match pending.iter().position(|(t, _)| *t == tool) {
                            Some(i) => pending.remove(i).map(|(_, a)| a).unwrap_or_default(),
                            None => serde_json::Value::Null,
                        };
                        trajectory.push(
                            &tool,
                            &args,
                            success,
                            preview.starts_with(PROBE_STUB_PREFIX),
                            &preview,
                        );
                        trace.push(ToolInvocation {
                            tool: tool.clone(),
                            success,
                            tests_passed: passing_tests(&tool, &preview),
                            clippy_clean: clippy_clean(&tool, &preview),
                            action: args["action"].as_str().map(str::to_string),
                            stubbed: preview.starts_with(PROBE_STUB_PREFIX),
                        });
                        if success {
                            gathered.push(preview.clone());
                        }
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::ToolResult {
                                    tool: format!("{agent}:{tool}"),
                                    success,
                                    // Forwarded intact: a sub-agent's tool
                                    // result reaches the user's stream in full,
                                    // carrying how much of it the WORKER read.
                                    preview,
                                    model_saw_chars,
                                })
                                .await;
                        }
                    }
                    ChatEvent::Thinking { content } => {
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::Thinking {
                                    content: format!("[{agent}] {content}"),
                                })
                                .await;
                        }
                    }
                    ChatEvent::Token { .. } => {}
                }
            }
            (answer, saw_error, gathered, trace, trajectory)
        };

        let (report, (answer, saw_error, gathered, trace, mut trajectory)) =
            tokio::join!(provider_loop, drain);
        trajectory.set_answer(&answer);
        let model = model_that_ran(&report);
        // The worker actually consulted the world iff it ran ≥1 successful tool.
        let retrieved = !gathered.is_empty();

        let ok = !answer.trim().is_empty();
        if let Some(tel) = &telemetry {
            let failed = trace.iter().filter(|t| !t.success).count();
            if let Err(e) = tel.record_agent_run(
                agent,
                caller,
                &model,
                ok,
                trace.len(),
                failed,
                started.elapsed().as_millis() as u64,
                agent_brain_llm::ctx::current_owner_task().as_deref(),
                &crate::trajectory::cut(task, crate::trajectory::ANSWER_CHARS),
                &trajectory.to_json(),
            ) {
                tracing::warn!(error = %e, agent, "agent run not recorded");
            }
        }
        if let Some(ref tx) = sink {
            // Close the delegation bracket in the stream so the whole sub-agent
            // turn reads as one unit that started and finished.
            let elapsed = started.elapsed().as_secs_f64();
            let finish = if ok {
                format!(
                    "✓ `{agent}` finished — {} result(s) in {elapsed:.1}s",
                    gathered.len()
                )
            } else {
                format!("✗ `{agent}` finished with no answer ({elapsed:.1}s)")
            };
            let _ = tx.send(ChatEvent::Thinking { content: finish }).await;
        }

        if !ok {
            // Ran, but produced nothing — not a rejection. A tool caller reports
            // this as a non-fatal step result, not a hard error.
            return AgentFindings {
                ok: false,
                rejected: false,
                retrieved,
                findings: no_answer_text(agent, &report, saw_error.as_deref()),
                tool_trace: trace,
                model,
                attempts: report.attempts,
                refused: report.refused,
                trajectory,
                harness: harness_used,
            };
        }
        AgentFindings {
            ok: true,
            rejected: false,
            retrieved,
            findings: format_subagent_findings(agent, &answer, &gathered, &trace),
            tool_trace: trace,
            model,
            attempts: report.attempts,
            refused: report.refused,
            trajectory,
            harness: harness_used,
        }
    }

    /// Deterministically pick the spawnable agent whose specialty/description
    /// best matches `goal` by token overlap. LLM-free (plan §4, decision 5), so a
    /// chain or schedule step can route to an agent without a model call.
    ///
    /// `agent` is `None` when the roster is empty or nothing clears
    /// [`MIN_SELECTION_OVERLAP`]. The full roster is always returned so a caller
    /// with no match can show the options.
    pub async fn select_agent(&self, goal: &str) -> AgentSelection {
        let roster_profiles = match self.context_builder.read().await.clone() {
            Some(cb) => cb.spawnable_profiles().await,
            None => Vec::new(),
        };
        let roster: Vec<(String, String)> = roster_profiles
            .iter()
            .map(|p| {
                let blurb = p
                    .specialty
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(p.description.as_str())
                    .to_string();
                (p.name.clone(), blurb)
            })
            .collect();

        let goal_tokens = tokenize(goal);
        // Score = NAME_WEIGHT × (name-token overlap) + (specialty-token overlap).
        // The name is weighted higher because it is the stronger signal: a goal
        // that says "research …" should pick `researcher` over `knowledge-worker`
        // even though both specialties contain "search" (and `research` ⊃
        // `search` under substring matching). Ties break by roster order (stable);
        // `>` (not `>=`) keeps the first seen.
        const NAME_WEIGHT: usize = 2;
        let mut best: Option<(String, usize)> = None;
        for p in &roster_profiles {
            let name_tokens = tokenize(&p.name);
            let spec_tokens = tokenize(p.specialty.as_deref().unwrap_or(&p.description));
            let score = NAME_WEIGHT * token_overlap(&goal_tokens, &name_tokens)
                + token_overlap(&goal_tokens, &spec_tokens);
            match &best {
                Some((_, best_score)) if score <= *best_score => {}
                _ => best = Some((p.name.clone(), score)),
            }
        }

        match best {
            Some((name, overlap)) if overlap >= MIN_SELECTION_OVERLAP => AgentSelection {
                agent: Some(name),
                score: overlap,
                roster,
            },
            _ => AgentSelection {
                agent: None,
                score: 0,
                roster,
            },
        }
    }
}

/// Minimum goal/specialty token overlap for [`AgentRunner::select_agent`] to
/// return a match rather than `None`. One shared meaningful word is enough to
/// prefer a curated agent over constructing one; below that, build.
const MIN_SELECTION_OVERLAP: usize = 1;

/// The outcome of a deterministic agent selection.
#[derive(Debug, Clone)]
pub struct AgentSelection {
    /// The best-matching spawnable agent, or `None` when nothing fit — build one.
    pub agent: Option<String>,
    /// Token-overlap score of the chosen agent (0 when none).
    pub score: usize,
    /// The full spawnable roster as `(name, specialty)` — always returned.
    pub roster: Vec<(String, String)>,
}

/// The generalist to fall back to when `run_agent(auto)` finds no lexical match.
/// A web+graph researcher is the safe default for an open-ended task the matcher
/// could not place (many topical goals — "off-grid mesh hardware" — share no
/// words with any curated specialty). Prefers a spawnable whose name mentions
/// "research"; else the first in the roster. `select_agent` itself stays honest
/// and returns `None` — only `auto` applies this default, so the standalone
/// choose-or-build signal (None = consider constructing) is preserved.
fn generalist_default(roster: &[(String, String)]) -> Option<String> {
    roster
        .iter()
        .map(|(n, _)| n)
        .find(|n| n.to_ascii_lowercase().contains("research"))
        .or_else(|| roster.first().map(|(n, _)| n))
        .cloned()
}

/// Count goal tokens that match a profile's tokens, by **bidirectional
/// substring** (a goal token matches if it contains, or is contained by, a
/// profile token). Substring rather than equality so `research` hooks
/// `researcher`, `code` hooks `code-analyst`, `schedule` hooks
/// `schedule-author` — the agent NAME is usually the strongest signal, and goals
/// name the domain, not the specialty's exact phrasing. Both sides are already
/// length-≥3 (see [`tokenize`]), which bounds spurious substring hits.
fn token_overlap(
    goal: &std::collections::HashSet<String>,
    haystack: &std::collections::HashSet<String>,
) -> usize {
    goal.iter()
        .filter(|g| {
            haystack
                .iter()
                .any(|h| h.contains(g.as_str()) || g.contains(h.as_str()))
        })
        .count()
}

/// Lowercase, split on non-alphanumerics, drop short/stopword tokens. A tiny
/// stopword set keeps generic words ("the", "and", "with") from matching every
/// agent; a min length of 3 drops the rest of the noise.
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "into", "your", "you", "how", "what",
        "why", "who", "was", "are", "its", "our", "out", "get", "use", "any", "all", "can",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Extract unique http(s) URLs from an agent's tool-result previews.
///
/// The worker's own tool outputs are the trustworthy provenance — its prose can
/// paraphrase or drop links (the exact laundering the epistemics guards against).
/// Pulling the URLs straight out of what its tools returned gives the caller a
/// concrete list to cite regardless of how the worker wrote them up. Split on
/// whitespace/brackets (no byte indexing, so multibyte text is safe), capped.
fn extract_sources(gathered: &[String]) -> Vec<String> {
    const MAX_SOURCES: usize = 8;
    let mut out: Vec<String> = Vec::new();
    for text in gathered {
        for tok in text.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>')
        }) {
            if (tok.starts_with("http://") || tok.starts_with("https://")) && tok.len() > 10 {
                let url = tok.trim_end_matches(['.', ',', ';', ':']).to_string();
                if !out.contains(&url) {
                    out.push(url);
                    if out.len() >= MAX_SOURCES {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Assemble an agent's answer + its sources into the structured findings block
/// the caller reads. The header instructs faithful relay, source citation, and
/// passing along any stated limitation rather than filling it in — the
/// epistemics the worker/voice split alone does not enforce.
fn format_subagent_findings(
    agent: &str,
    answer: &str,
    gathered: &[String],
    trace: &[ToolInvocation],
) -> String {
    let sources = extract_sources(gathered);
    let mut out = format!(
        "[FINDINGS FROM THE `{agent}` SUB-AGENT — relay these faithfully. Cite the sources \
         listed below as markdown links when you use them. If the findings say something \
         could not be found or established, say so plainly rather than filling it in from your \
         own knowledge. Do not add facts the worker did not report.]\n\n{answer}"
    );
    out.push_str(&format_tool_receipt(agent, trace));
    if !sources.is_empty() {
        out.push_str("\n\nSOURCES:");
        for (i, url) in sources.iter().enumerate() {
            out.push_str(&format!("\n  [S{}] {url}", i + 1));
        }
    }
    out
}

/// Render the ground-truth tool receipt: what the sub-agent ACTUALLY ran,
/// aggregated per tool in first-seen order. This is the check against a
/// fabricated narrative — an answer that claims an action whose tool is absent
/// here did not perform it. Deliberately terse and machine-legible so it survives
/// relay through `{{_prev}}` and the chat stream.
fn format_tool_receipt(agent: &str, trace: &[ToolInvocation]) -> String {
    if trace.is_empty() {
        return format!(
            "\n\n[TOOLS `{agent}` ACTUALLY RAN: none. This sub-agent called no tools — any \
             action its answer claims (a search, a write, an execution) did NOT happen; treat \
             the answer as ungrounded assertion.]"
        );
    }
    // Aggregate per tool, first-seen order, counting ok vs failed.
    let mut order: Vec<String> = Vec::new();
    let mut ok: HashMap<&str, usize> = HashMap::new();
    let mut fail: HashMap<&str, usize> = HashMap::new();
    for inv in trace {
        if !order.iter().any(|t| t == &inv.tool) {
            order.push(inv.tool.clone());
        }
        if inv.success {
            *ok.entry(inv.tool.as_str()).or_insert(0) += 1;
        } else {
            *fail.entry(inv.tool.as_str()).or_insert(0) += 1;
        }
    }
    let parts: Vec<String> = order
        .iter()
        .map(|t| {
            let o = ok.get(t.as_str()).copied().unwrap_or(0);
            let f = fail.get(t.as_str()).copied().unwrap_or(0);
            match (o, f) {
                (o, 0) => format!("{t} ✓×{o}"),
                (0, f) => format!("{t} ✗×{f}"),
                (o, f) => format!("{t} ✓×{o} ✗×{f}"),
            }
        })
        .collect();
    format!(
        "\n\n[TOOLS `{agent}` ACTUALLY RAN (ground truth — trust this over the prose above; if \
         the answer claims an action whose tool is not listed here, it did not happen): {}]",
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rung(model: &str, error: Option<&str>) -> RungAttempt {
        RungAttempt {
            model: model.into(),
            provider: "p".into(),
            error: error.map(String::from),
        }
    }

    #[test]
    fn the_reported_model_is_the_one_that_ran() {
        let failed = LadderReport {
            attempts: vec![
                rung("ministral", Some("400")),
                rung("granite4", Some("timeout")),
            ],
            refused: vec!["gpt-oss:120b-cloud: cooling down".into()],
        };
        assert_eq!(
            model_that_ran(&failed),
            "granite4",
            "the last one tried, not the refused preference"
        );
        let answered = LadderReport {
            attempts: vec![rung("ministral", Some("400")), rung("codestral", None)],
            refused: vec![],
        };
        assert_eq!(model_that_ran(&answered), "codestral");
        assert_eq!(model_that_ran(&LadderReport::default()), "");
    }

    #[test]
    fn a_failed_run_names_every_rungs_error() {
        let r = LadderReport {
            attempts: vec![
                rung(
                    "ministral-14b-latest",
                    Some("400 Unexpected role 'tool' after role 'user'"),
                ),
                rung("granite4:latest", Some("Ollama stream read error")),
            ],
            refused: vec!["gpt-oss:120b-cloud: cooling down".into()],
        };
        let t = no_answer_text("code-analyst", &r, Some("Ollama stream read error"));
        assert!(
            t.contains("ministral-14b-latest (p): 400 Unexpected role"),
            "{t}"
        );
        assert!(
            t.contains("granite4:latest (p): Ollama stream read error"),
            "{t}"
        );
        assert!(
            t.contains("Refused by the routing policy: gpt-oss:120b-cloud"),
            "{t}"
        );
    }

    #[test]
    fn extract_sources_pulls_unique_urls_from_tool_previews() {
        let gathered = vec![
            "[SEARCH SOURCES]\n  [S1] Title — https://example.com/a\nbody".to_string(),
            "see (https://example.com/b) and https://example.com/a again".to_string(),
            "no urls here".to_string(),
        ];
        let got = extract_sources(&gathered);
        assert_eq!(got, vec!["https://example.com/a", "https://example.com/b"]);
    }

    #[test]
    fn extract_sources_trims_trailing_punctuation_and_brackets() {
        // A URL wrapped in parens or followed by a period must come out clean.
        let gathered = vec!["The source (https://example.com/x). Next.".to_string()];
        assert_eq!(extract_sources(&gathered), vec!["https://example.com/x"]);
    }

    fn inv(tool: &str, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_string(),
            success,
            tests_passed: None,
            clippy_clean: None,
            action: None,
            stubbed: false,
        }
    }

    #[test]
    fn a_claim_in_the_answer_needs_the_call_behind_it() {
        let spec = VerifySpec {
            answer_requires: vec![AnswerRequirement {
                when: r"^Source: https?://".into(),
                tools: vec!["create_task".into()],
            }],
            ..Default::default()
        };
        let named = "What: x\nSource: https://arxiv.org/abs/1\nProgram: y";
        let f = verification_failures(&spec, named, &[inv("read_page", true)], None);
        assert_eq!(f.len(), 1);
        assert!(
            f[0].contains("Source: https://arxiv.org/abs/1") && f[0].contains("create_task"),
            "{f:?}"
        );
        assert!(verification_failures(&spec, named, &[inv("create_task", true)], None).is_empty());
        // No source named: nothing required.
        let none = "What: x\nSource: no source found\nProgram: y";
        assert!(verification_failures(&spec, none, &[], None).is_empty());
    }

    #[test]
    fn a_required_action_must_have_run_not_just_its_tool() {
        let spec = VerifySpec {
            require_tools: vec!["manage_plugin:write".into(), "search_web".into()],
            ..Default::default()
        };
        let get = ToolInvocation {
            action: Some("get".into()),
            ..inv("manage_plugin", true)
        };
        let f = verification_failures(&spec, "done", &[get.clone(), inv("search_web", true)], None);
        assert_eq!(
            f,
            vec!["no successful `manage_plugin` call with action `write` this run"]
        );
        let write = ToolInvocation {
            action: Some("write".into()),
            ..inv("manage_plugin", true)
        };
        assert!(
            verification_failures(
                &spec,
                "done",
                &[get, write.clone(), inv("search_web", true)],
                None
            )
            .is_empty()
        );
        // A probe's stub is not the call having run.
        let stub = ToolInvocation {
            stubbed: true,
            ..write
        };
        let f = verification_failures(&spec, "done", &[stub, inv("search_web", true)], None);
        assert_eq!(f.len(), 1, "{f:?}");
    }

    #[test]
    fn the_answer_crate_is_built_from_the_code_shown() {
        let answer = "## RUST\n```rust\nfn helper() {}\n```\nand\n```rust\nuse sha2::{Digest, Sha256};\npub fn h() -> Vec<u8> { hex::decode(\"00\").unwrap() }\n#[test]\nfn t() {}\n```\n";
        let code = answer_rust_code(answer).unwrap();
        assert!(code.contains("#[test]") && code.contains("sha2"));

        let manifest = "[package]\nname = \"seed\"\n\n[dependencies]\n# comment\nsha2 = \"0.10\"\ned25519-dalek = \"2\"\nhex = \"0.4\"\nregex = \"1\"\n";
        let deps = sandbox_dependencies(manifest);
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[1].0, "ed25519_dalek");

        let files = answer_crate(&code, &deps);
        let cargo = files["Cargo.toml"].as_str().unwrap();
        assert!(cargo.contains("sha2 = \"0.10\"") && cargo.contains("hex = \"0.4\""));
        assert!(!cargo.contains("regex") && !cargo.contains("dalek"));
        assert!(files.get("src/lib.rs").is_some());
        assert!(answer_rust_code("no code here").is_none());
    }

    #[test]
    fn a_fence_inside_a_string_does_not_end_the_block() {
        let answer = "```rust\nfn f() {}\n#[test]\nfn t() { let s = \"x\\n```rust\\ny\\n```\\n\"; }\n  ```\nafter";
        let code = answer_rust_code(answer).unwrap();
        assert!(code.ends_with("\"; }\n"), "{code}");
        assert!(!code.contains("after"));
    }

    #[test]
    fn clippy_clean_reads_the_finding_count_not_the_exit_code() {
        let clean = r#"{"success": true, "command": "clippy", "lint_findings": 0}"#;
        assert_eq!(clippy_clean("execute_code", clean), Some(true));
        // clippy exits 0 on warnings: success alone is not clean.
        let warned = r#"{"success": true, "command": "clippy", "lint_findings": 3}"#;
        assert_eq!(clippy_clean("execute_code", warned), Some(false));
        let test_run = r#"{"success": true, "command": "test", "tests_passed": 2}"#;
        assert_eq!(clippy_clean("execute_code", test_run), None);
    }

    #[test]
    fn the_last_clippy_run_decides() {
        let spec = VerifySpec {
            code_requires_clippy_clean: true,
            ..Default::default()
        };
        let answer = "## RUST\n```rust\nfn f() {}\n```";
        let run = |clean: bool| {
            let mut t = inv("execute_code", true);
            t.clippy_clean = Some(clean);
            t
        };
        assert_eq!(verification_failures(&spec, answer, &[], None).len(), 1);
        assert!(verification_failures(&spec, answer, &[run(false), run(true)], None).is_empty());
        assert_eq!(
            verification_failures(&spec, answer, &[run(true), run(false)], None).len(),
            1
        );
    }

    #[test]
    fn passing_tests_reads_the_sandbox_result_not_prose() {
        let ok = r#"{"success": true, "command": "test", "tests_passed": 3, "tests_failed": 0}"#;
        assert_eq!(passing_tests("execute_code", ok), Some(3));
        let failing = r#"{"tests_passed": 2, "tests_failed": 1}"#;
        assert_eq!(passing_tests("execute_code", failing), None);
        let no_tests = r#"{"success": true, "stdout": "Destination hash: e301"}"#;
        assert_eq!(passing_tests("execute_code", no_tests), None);
        let broken = r#"{"compile_failed": true, "tests_passed": 1}"#;
        assert_eq!(passing_tests("execute_code", broken), None);
        assert_eq!(passing_tests("search_web", ok), None);
    }

    #[test]
    fn verification_flags_a_copied_answer_with_untested_code() {
        // The 2026-10-03 re-run: a note found, a program run, no page read, no test.
        let spec = VerifySpec {
            require_tools: vec!["mcp_call".into()],
            code_requires_tests: true,
            cited_paths_exist: false,
            ..Default::default()
        };
        let trace = vec![inv("search_notes", true), inv("execute_code", true)];
        let answer = "## ANSWER\n- x\n## RUST\n```rust\nfn main(){}\n```";
        let failures = verification_failures(&spec, answer, &trace, None);
        assert_eq!(failures.len(), 2, "{failures:?}");

        let mut tested = inv("execute_code", true);
        tested.tests_passed = Some(4);
        let trace = vec![inv("mcp_call", true), tested];
        assert!(verification_failures(&spec, answer, &trace, None).is_empty());
    }

    #[test]
    fn a_cited_line_must_exist_but_a_new_path_or_url_is_not_checked() {
        let root = std::env::temp_dir().join(format!("cited-{}", std::process::id()));
        std::fs::create_dir_all(root.join("crates/app/src")).unwrap();
        std::fs::write(root.join("crates/app/src/real.rs"), "a\nb\nc\n").unwrap();
        let answer = "- `crates/app/src/real.rs:2` fine\n\
                      - crates/app/src/real.rs:9 past the end\n\
                      - (crates/app/src/reticulum.rs:12) invented\n\
                      - add crates/app/src/new_module.rs\n\
                      - https://github.com/o/r/blob/main/x.py:10";
        let missing = missing_cited_paths(answer, &root);
        assert_eq!(
            missing,
            vec!["crates/app/src/real.rs:9", "crates/app/src/reticulum.rs:12"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rust_question_cannot_opt_out_of_code() {
        let spec = VerifySpec {
            code_requires_tests: true,
            question: "How is a Reticulum destination hash computed, in Rust?".into(),
            require_code_if_question_mentions: vec!["rust".into()],
            ..Default::default()
        };
        let declined = "## ANSWER\n- name hash …\n## RUST\nnot applicable";
        assert_eq!(verification_failures(&spec, declined, &[], None).len(), 1);

        let trust = VerifySpec {
            question: "How does zero trust networking work?".into(),
            ..spec.clone()
        };
        assert!(verification_failures(&trust, declined, &[], None).is_empty());
    }

    #[test]
    fn a_no_findings_answer_is_not_verified() {
        let spec = VerifySpec {
            require_tools: vec!["mcp_call".into()],
            code_requires_tests: true,
            cited_paths_exist: false,
            ..Default::default()
        };
        assert!(verification_failures(&spec, "NO FINDINGS — nothing", &[], None).is_empty());
    }

    #[test]
    fn findings_carry_the_relay_instruction_and_sources() {
        let out = format_subagent_findings(
            "researcher",
            "Reticulum has a 500-byte MTU.",
            &["[S1] RNS docs — https://reticulum.network/manual".to_string()],
            &[inv("search_web", true)],
        );
        // The relay header must be present (cite + relay limits).
        assert!(out.contains("relay these faithfully"));
        assert!(out.contains("researcher"));
        // The answer is included verbatim...
        assert!(out.contains("500-byte MTU"));
        // ...and the source is lifted out for citation.
        assert!(out.contains("https://reticulum.network/manual"));
        assert!(out.contains("SOURCES:"));
    }

    #[test]
    fn findings_omit_the_sources_block_when_there_are_none() {
        let out = format_subagent_findings(
            "knowledge-worker",
            "From the graph: X.",
            &[],
            &[inv("search_notes", true)],
        );
        assert!(out.contains("From the graph: X."));
        assert!(!out.contains("SOURCES:"));
    }

    #[test]
    fn receipt_aggregates_tools_and_counts_outcomes() {
        let out = format_tool_receipt(
            "toolsmith",
            &[
                inv("analyze_own_structure", true),
                inv("analyze_own_structure", true),
                inv("manage_plugin", true),
                inv("http_request", false),
            ],
        );
        assert!(out.contains("ACTUALLY RAN"));
        assert!(out.contains("analyze_own_structure ✓×2"), "got: {out}");
        assert!(out.contains("manage_plugin ✓×1"), "got: {out}");
        assert!(out.contains("http_request ✗×1"), "got: {out}");
    }

    #[test]
    fn receipt_flags_a_run_that_called_no_tools() {
        // The toolsmith case: an answer that narrates work while no tool ran.
        let out = format_tool_receipt("toolsmith", &[]);
        assert!(out.contains("none"));
        assert!(out.contains("did NOT happen"), "got: {out}");
    }

    #[test]
    fn tokenize_drops_stopwords_and_short_words() {
        let toks = tokenize("What are the latest Reticulum mesh developments?");
        assert!(toks.contains("latest"));
        assert!(toks.contains("reticulum"));
        assert!(toks.contains("mesh"));
        // Stopwords and short words are filtered.
        assert!(!toks.contains("what"));
        assert!(!toks.contains("are"));
        assert!(!toks.contains("the"));
    }

    #[test]
    fn token_overlap_matches_by_substring() {
        // `research` (goal) must hook `researcher` (agent name) — the exact-token
        // matching this replaced returned zero here, the bug the smoke test found.
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let researcher = tokenize("researcher current external info web search");
        assert!(
            token_overlap(&goal, &researcher) >= 1,
            "research goal should hook the researcher via substring"
        );
    }

    #[test]
    fn token_overlap_discriminates_research_from_code() {
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let researcher = tokenize("researcher current external info web search");
        // code-analyst specialty AFTER the reword (no ambiguous "source").
        let coder = tokenize(
            "code-analyst questions about this codebase code structure git history static analysis",
        );
        assert!(
            token_overlap(&goal, &researcher) > token_overlap(&goal, &coder),
            "a research goal must prefer the researcher over the code analyst"
        );
    }

    #[test]
    fn name_weight_prefers_researcher_over_knowledge_worker_for_research() {
        // Both specialties contain "search" (and research ⊃ search), so a raw
        // overlap ties them; weighting the NAME match must break the tie toward
        // the researcher. Mirrors the scoring in select_agent.
        const NAME_WEIGHT: usize = 2;
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let score = |name: &str, spec: &str| {
            NAME_WEIGHT * token_overlap(&goal, &tokenize(name))
                + token_overlap(&goal, &tokenize(spec))
        };
        let researcher = score(
            "researcher",
            "current external info web search knowledge graph",
        );
        let kworker = score(
            "knowledge-worker",
            "what the brain already knows search synthesise stored notes knowledge graph",
        );
        assert!(
            researcher > kworker,
            "research goal should prefer researcher ({researcher}) over knowledge-worker ({kworker})"
        );
    }

    #[test]
    fn generalist_default_prefers_researcher() {
        let roster = vec![
            ("code-analyst".to_string(), "codebase".to_string()),
            ("researcher".to_string(), "web".to_string()),
        ];
        assert_eq!(generalist_default(&roster).as_deref(), Some("researcher"));
        // Falls back to the first spawnable when there is no researcher.
        let roster = vec![("code-analyst".to_string(), "codebase".to_string())];
        assert_eq!(generalist_default(&roster).as_deref(), Some("code-analyst"));
        assert_eq!(generalist_default(&[]), None);
    }

    // The 2026-10-07 deep dive: code that did not compile, a claimed passing
    // run, and a placement section chat relayed as work already done.
    const CLAIMED_PASS: &str = "## ANSWER\n- VEC-DEST-HASH is 07d4…\n\n## RUST\n```rust\n// test result: ok inside code stays\n#[test]\nfn t() {}\n```\nTest result: `test result: ok. 1 passed`  \nChecks against the official vector.\n\n## IN AGENT\u{2011}BRAIN\n- crates/reticulum-sdk/src/destination.rs:42 – add a comment\n";

    #[test]
    fn unverified_answer_drops_the_claimed_pass() {
        let failures = vec![
            "the answer's own Rust code, re-run by the verifier, did not pass: Compilation failed (error[E0433]: cannot find crate `reticulum_sdk`)".to_string(),
        ];
        let out = mark_unverified_answer(CLAIMED_PASS, &failures);
        assert!(out.starts_with("UNVERIFIED — the answer's own Rust code"));
        assert!(out.contains(". Treat everything below as unchecked.\n\n## ANSWER"));
        assert!(!out.contains("1 passed"), "claimed pass survived: {out}");
        assert!(out.contains("Test result: NOT CONFIRMED"));
        assert!(out.contains("error[E0433]"));
        // Lines inside the code block are the code as shown — untouched.
        assert!(out.contains("// test result: ok inside code stays"));
        assert!(out.contains("Checks against the official vector."));
    }

    #[test]
    fn placement_heading_says_it_is_a_proposal() {
        let out = label_proposed_placement(CLAIMED_PASS);
        assert!(out.contains("## IN AGENT-BRAIN (proposed — nothing was changed)\n- crates/"));
        let plain = label_proposed_placement("## IN AGENT-BRAIN\n- no fit");
        assert_eq!(
            plain,
            "## IN AGENT-BRAIN (proposed — nothing was changed)\n- no fit"
        );
        // Prose mentioning the phrase is not a heading.
        assert_eq!(
            label_proposed_placement("see IN AGENT-BRAIN below"),
            "see IN AGENT-BRAIN below"
        );
    }

    #[test]
    fn first_compile_error_finds_the_rustc_line() {
        let stderr = "   Compiling hex v0.4.3\nerror[E0433]: cannot find module or crate `reticulum_sdk` in this scope\n --> src/lib.rs:4:5\n";
        assert_eq!(
            first_compile_error(stderr).as_deref(),
            Some("error[E0433]: cannot find module or crate `reticulum_sdk` in this scope")
        );
        assert_eq!(first_compile_error("   Compiling hex\n"), None);
    }
}
