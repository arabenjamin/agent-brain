//! Deterministic write-boundary validation — the "classic automated pushback".
//!
//! Objects the LLM writes into the graph (schedules, tasks, chains, agents) are
//! validated here BEFORE they are persisted, independent of how confidently the
//! model narrated success. A HARD violation rejects the write outright; a SOFT
//! violation is advisory (the existing `lint_chain_steps` warnings).
//!
//! The high-value check is [`validate_step_arguments`]: every registered tool
//! already ships a JSON Schema (`ToolDefinition.input_schema`), so a step's
//! `arguments` are validated against the *tool's own* schema with zero drift.
//! This catches the class that killed the AI Dev Board Job Search schedule —
//! `query_params: "[object Object]"` (string where the schema says object),
//! `timeout_ms: "15000"` (string where it says integer), `pick` as a
//! comma-string — none of which surfaced at write time before.
//!
//! See `project-docs/VALIDATION_AND_APPLY_PLAN.md`.

use crate::queue::ChainStep;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// How hard a violation bites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Reject the write. The object is structurally invalid and would fail at runtime.
    Hard,
    /// Advisory. The object is legal but probably not what the author intended.
    Soft,
}

/// One problem found in a candidate object.
#[derive(Debug, Clone)]
pub struct Violation {
    pub severity: Severity,
    /// Dotted/indexed location, e.g. `steps[0].arguments/query_params`.
    pub path: String,
    pub message: String,
}

impl Violation {
    pub fn hard(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Hard,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn soft(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Soft,
            path: path.into(),
            message: message.into(),
        }
    }

    /// Human-readable one-liner for a rejection message.
    pub fn render(&self) -> String {
        format!("{}: {}", self.path, self.message)
    }
}

/// True if any violation is [`Severity::Hard`].
pub fn has_hard(violations: &[Violation]) -> bool {
    violations.iter().any(|v| v.severity == Severity::Hard)
}

/// Render only the hard violations into a single rejection string.
pub fn render_hard(violations: &[Violation]) -> String {
    violations
        .iter()
        .filter(|v| v.severity == Severity::Hard)
        .map(Violation::render)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Validate a JSON instance against a JSON Schema, one HARD [`Violation`] per error.
///
/// A schema that fails to compile is OUR bug, not the caller's data — it logs
/// and returns no violations, so a broken schema never blocks a legitimate
/// write. We fail *closed* on the caller's data and *open* on our own mistakes.
pub fn validate_against_schema(prefix: &str, schema: &Value, instance: &Value) -> Vec<Violation> {
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, prefix, "validation: schema failed to compile — skipping");
            return Vec::new();
        }
    };
    validator
        .iter_errors(instance)
        .map(|err| {
            let ptr = err.instance_path.to_string();
            let path = if ptr.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}{ptr}")
            };
            Violation::hard(path, err.to_string())
        })
        .collect()
}

/// Validate one chain step's `arguments` against the registered tool's
/// `input_schema`. Absent `arguments` validate as `{}` so a missing *required*
/// field still fires. A tool with no schema in the map is skipped (not every
/// runtime/dynamic tool is guaranteed present); the caller flags an
/// *unregistered* tool separately as its own hard violation.
pub fn validate_step_arguments(
    step_index: usize,
    tool_name: &str,
    arguments: Option<&Value>,
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let Some(schema) = tool_schemas.get(tool_name) else {
        return Vec::new();
    };
    let empty = Value::Object(serde_json::Map::new());
    let args = arguments.unwrap_or(&empty);
    let prefix = format!("steps[{step_index}] (`{tool_name}`).arguments");
    validate_against_schema(&prefix, schema, args)
}

/// Load every `*.json` JSON Schema from `dir`, keyed by file stem
/// (`workflow.json` → `"workflow"`). Unreadable or unparseable files
/// are logged and skipped — a broken schema file must never wedge startup. The
/// schemas are not compiled here; each is compiled at use in
/// [`validate_against_schema`], which fails open on a bad schema. A missing dir
/// returns an empty map (envelope validation simply disabled). This is the
/// envelope-layer loader — the per-step argument check needs no disk.
pub fn load_schema_dir(dir: &Path) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(), error = %e,
                "validation: schemas dir unreadable — envelope validation disabled"
            );
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    out.insert(stem.to_string(), v);
                }
                Err(e) => tracing::error!(
                    path = %path.display(), error = %e,
                    "validation: schema file is not valid JSON — skipping"
                ),
            },
            Err(e) => tracing::error!(
                path = %path.display(), error = %e,
                "validation: cannot read schema file — skipping"
            ),
        }
    }
    tracing::info!(count = out.len(), "validation: loaded envelope schemas");
    out
}

/// Validate a whole step array (raw JSON `Value` objects, as stored/authored),
/// returning every HARD violation across steps: a missing/blank `tool_name`, an
/// unregistered tool (only when `live_tools` is non-empty), and each step's
/// `arguments` against the tool's own `input_schema`. Shared by the scheduler's
/// `manage_workflow` write path and its `audit` action so a
/// stored (e.g. YAML-seeded) task is checked by exactly the same rules as a new one.
/// Fields a step carries beside `arguments` (`services/queue.rs::ChainStep`).
/// Keep in step with that struct; it is Deserialize-only, so it cannot list
/// them itself.
const CHAIN_STEP_FIELDS: &[&str] = &[
    "tool_name",
    "arguments",
    "priority",
    "max_attempts",
    "provider_hint",
    "context_profile",
    "ttl_secs",
    "description",
    "confidence_threshold",
    "is_evaluator",
    "min_score",
    "evaluator_task_id",
    "is_adversarial",
    "n_hypotheses",
    "min_robustness",
    "adversarial_task_id",
    "required_capabilities",
    "data_class",
    "distill_prev",
    "distill_max_chars",
    "distill_focus",
    "optional",
    "id",
];

/// A tool argument written at step level instead of inside `arguments`.
///
/// Serde drops an unknown step field, so the argument never reaches the tool
/// and nothing errors. Seen twice: `provider_hint` inside `arguments` (the
/// 2026-09-08 GDELT schedule) and the reverse — `context: "{{_prev}}"` beside
/// `arguments` on the Trump Benefit monitor, whose `reason` step therefore
/// never saw its search results (found 2026-09-26). Only keys the tool's own
/// schema declares are flagged, so harmless extra metadata passes.
fn misplaced_arguments(
    i: usize,
    tool: &str,
    step: &Value,
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let (Some(obj), Some(props)) = (
        step.as_object(),
        tool_schemas
            .get(tool)
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object()),
    ) else {
        return vec![];
    };
    obj.keys()
        .filter(|k| !CHAIN_STEP_FIELDS.contains(&k.as_str()) && props.contains_key(*k))
        .map(|k| {
            Violation::hard(
                format!("steps[{i}] (`{tool}`)"),
                format!(
                    "`{k}` is an argument of `{tool}` but sits at step level, where it is \
                     silently ignored — move it inside `arguments`"
                ),
            )
        })
        .collect()
}

pub fn validate_step_array(
    steps: &[Value],
    live_tools: &[String],
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
        if tool.is_empty() {
            out.push(Violation::hard(
                format!("steps[{i}]"),
                "missing `tool_name` — cannot dispatch this step",
            ));
            continue;
        }
        if !live_tools.is_empty() && !live_tools.iter().any(|t| t == tool) {
            out.push(Violation::hard(
                format!("steps[{i}] (`{tool}`)"),
                "no such tool is registered — this step would fail every run",
            ));
        }
        out.extend(validate_step_arguments(
            i,
            tool,
            step.get("arguments"),
            tool_schemas,
        ));
        out.extend(misplaced_arguments(i, tool, step, tool_schemas));
    }
    out.extend(step_ref_violations(steps));
    out
}

/// Every `{{steps.<id>}}` must name an EARLIER step's `id`, and ids must be
/// unique. A forward or unknown reference would resolve to a marker on every
/// run, so it is rejected when written, not discovered in a digest.
fn step_ref_violations(steps: &[Value]) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let args = step
            .get("arguments")
            .map(Value::to_string)
            .unwrap_or_default();
        for r in step_refs(&args) {
            if !seen.contains(&r.as_str()) {
                let named: Vec<&str> = seen.clone();
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!(
                        "`{{{{steps.{r}}}}}` names no earlier step (earlier step ids: {}) \
                         — a step can only use the output of a step before it; give the \
                         step you mean a top-level `\"id\": \"{r}\"`, or use `{{{{_prev}}}}` \
                         for the step right before",
                        if named.is_empty() {
                            "none".to_string()
                        } else {
                            named.join(", ")
                        }
                    ),
                ));
            }
        }
        if let Some(id) = step.get("id").and_then(Value::as_str) {
            if id.is_empty() || id.contains('.') || id.contains('}') {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!("step id `{id}` must be non-empty with no `.` or `}}`"),
                ));
            } else if seen.contains(&id) {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!("step id `{id}` is used twice"),
                ));
            } else {
                seen.push(id);
            }
        }
    }
    out
}

/// Every `{{inputs.<name>}}` a step uses must be a declared input of its
/// workflow; an undeclared one would never be supplied and resolve to nothing.
pub fn validate_input_refs(steps: &[Value], declared: &[String]) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let text = step.to_string();
        for name in text
            .split("{{inputs.")
            .skip(1)
            .filter_map(|r| r.find("}}").map(|e| r[..e].trim().to_string()))
        {
            if !declared.contains(&name) {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!(
                        "`{{{{inputs.{name}}}}}` is not a declared input (declared: {})",
                        if declared.is_empty() {
                            "none".to_string()
                        } else {
                            declared.join(", ")
                        }
                    ),
                ));
            }
        }
    }
    out
}

/// The step ids a string refers to via `{{steps.<id>}}` / `{{steps.<id>.<path>}}`.
fn step_refs(s: &str) -> Vec<String> {
    s.split("{{steps.")
        .skip(1)
        .filter_map(|rest| {
            let token = &rest[..rest.find("}}")?];
            Some(token.split('.').next().unwrap_or("").trim().to_string())
        })
        .collect()
}

/// Whether the current call is driven by a model: a chat turn or an agent loop
/// (`Purpose::Chat` / `Subagent`). Queued workflow steps and an operator's
/// direct MCP or REST call run as `Background`.
pub fn model_driven_call() -> bool {
    agent_brain_llm::routing::RouteContext::current().purpose
        != agent_brain_llm::routing::Purpose::Background
}

/// A step that writes the graph with raw Cypher (`neo4j_query` with
/// `readonly: false`). Refused when a model saves the workflow: the 2026-10-10
/// draft that would `SET` a property on every node created in the last week,
/// nightly, passed every shape check, because `readonly: false` is a valid
/// argument. Who may write is not a question of shape. Workflows the owner
/// writes (YAML, the HBI, a direct call) keep their cleanup steps, and a model
/// editing one (pausing it, changing another step) may carry such a step over
/// unchanged: only a raw write not already in `stored` is refused.
pub fn validate_raw_graph_writes(steps: &[Value], stored: &[Value]) -> Vec<Violation> {
    let is_raw_write = |s: &Value| {
        s.get("tool_name").and_then(Value::as_str) == Some("neo4j_query")
            && s.pointer("/arguments/readonly").and_then(Value::as_bool) == Some(false)
    };
    // Compared without the `__` keys the queue stamps (data class, owner).
    let args = |s: &Value| -> Value {
        let mut a = s.get("arguments").cloned().unwrap_or_default();
        if let Some(m) = a.as_object_mut() {
            m.retain(|k, _| !k.starts_with("__"));
        }
        a
    };
    let kept: Vec<Value> = stored
        .iter()
        .filter(|s| is_raw_write(s))
        .map(args)
        .collect();
    steps
        .iter()
        .enumerate()
        .filter(|(_, s)| is_raw_write(s) && !kept.contains(&args(s)))
        .map(|(i, _)| {
            Violation::hard(
                format!("steps[{i}] (`neo4j_query`).arguments.readonly"),
                "a workflow drafted by a model cannot write the graph with raw Cypher; use \
                 the tool for the change (store_note, update_task, manage_todo, ...), or ask \
                 the user to add the step",
            )
        })
        .collect()
}

/// Every step's pinned `model` must be in the catalog, and a `provider_hint`
/// written beside it must agree with that model's provider. A hint that
/// contradicts its model is the Zero Trust schedule: `ollama-cloud` on a step
/// naming the host-local `gemma4:latest`, which ran on cloud while it said
/// local. An empty catalog disables the check (fail-open on our own wiring).
pub fn validate_step_models(
    steps: &[Value],
    catalog: &[agent_brain_llm::model_router::CatalogModel],
) -> Vec<Violation> {
    use agent_brain_llm::model_router::{find_model, hint_agrees, provider_hint_for};
    if catalog.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let Some(model) = step
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            continue;
        };
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let path = format!("steps[{i}] (`{tool}`).model");
        let Some(row) = find_model(catalog, model) else {
            out.push(Violation::hard(
                path,
                format!(
                    "`{model}` is not in the model catalog (models.yaml) — this step would \
                     fail every run; use a catalog model id or name, or remove `model`"
                ),
            ));
            continue;
        };
        if let Some(hint) = step.get("provider_hint").and_then(Value::as_str)
            && !hint_agrees(hint, &row.provider)
        {
            out.push(Violation::hard(
                path,
                format!(
                    "`provider_hint: {hint}` contradicts `{}` (provider {}) — set \
                     `provider_hint: {}` or remove it (it is derived from the model)",
                    row.model,
                    row.provider,
                    provider_hint_for(&row.provider)
                ),
            ));
        }
    }
    out
}

// ── Step lint (advisory) ─────────────────────────────────────────────────
//
// Moved from `skills/scheduler.rs` (2026-10-16) so `workflows::save` and
// `POST /workflows/check` run it too, not only `manage_workflow`: the HBI
// editor never saw these warnings.

/// Tools whose entire purpose is to persist or hand on the material they are
/// given. Mid-chain, one of these with no `{{_prev}}` anywhere in its arguments
/// stores a fixed literal and silently drops the upstream step's output.
const PERSISTING_TOOLS: &[&str] = &[
    "store_note",
    "push_context",
    "write_workspace_file",
    "create_task",
    "notify_user",
];

/// `note_type` values retrieval and the epistemics layer actually understand.
/// `store_note` takes a free-form string, so an invented type (a chat-authored
/// schedule stored an `analysis` note on 2026-09-08) is persisted silently and
/// then never labelled or selected correctly — `label_claims`, consolidation,
/// and `recall_recent` all key off these names. `news_raw` is the pre-classified
/// news feed type; kept here so the lint doesn't flag a legitimate one.
const VALID_NOTE_TYPES: &[&str] = &[
    "semantic",
    "episodic",
    "reflection",
    "consolidated",
    "outcome",
    "inference",
    "claim",
    "source_record",
    "news",
    "news_raw",
    "unsourced_synthesis",
    // Written by the queue's meta-learning chain (services/queue.rs) and
    // excluded from consolidation source selection by name in
    // services/knowledge/. It is a real type; it was simply never added here,
    // so the lint warned "not a recognized type" about the brain's own output.
    "meta_learning_result",
];

/// True when a step's arguments reference the previous step's output in any
/// form — `{{_prev}}`, a dotted `{{_prev.id}}` path, the `{{result…}}` alias,
/// or a named step's `{{steps.<id>}}`.
fn references_prev(args: Option<&Value>) -> bool {
    args.is_some_and(|v| {
        let s = v.to_string();
        s.contains("{{_prev") || s.contains("{{result") || s.contains("{{steps.")
    })
}

/// Lint a chain for steps that are structurally valid but discard their own
/// inputs. Returns human-readable warnings, most severe first; empty means clean.
///
/// This exists because a workflow's steps are enqueued verbatim — no checks
/// unless the workflow opts in. Deserializing
/// as `Vec<ChainStep>` proves the shape is legal and nothing more, so a chain
/// authored from chat could be accepted, dispatched, and run weekly while doing
/// nothing at all. `Off-Grid Networking Monitor` (created 2026-08-12) did
/// exactly that: a `reason` step with no `context` answered "there is no
/// information" seconds after its own `search_web` returned five results, and a
/// `store_note` whose content was a fixed sentence stored 55 characters a week,
/// forever. Every rule below is one of those three defects.
///
/// Warnings, never rejections. A first step may legitimately store a literal,
/// and a `reason` step may legitimately stand alone — the caller knows which.
/// A false positive that blocks a valid schedule is worse than a warning the
/// author reads and dismisses.
pub fn lint_chain_steps(steps: &[ChainStep], live_tools: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();

    let has_search = steps.iter().any(|s| s.tool_name == "search_web");

    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        let tool = step.tool_name.as_str();
        let args = step.arguments.as_ref();

        if !live_tools.is_empty() && !live_tools.contains(&step.tool_name) {
            warnings.push(format!(
                "step {n} (`{tool}`): no such tool is registered — this step will fail every run."
            ));
        }

        // `reason` reads its retrieval context from `context`. Given only a
        // `question`, it runs RAG over the graph and never sees the upstream
        // step, however carefully that step was written.
        if tool == "reason"
            && i > 0
            && args
                .and_then(|a| a.get("context"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
        {
            let prev = &steps[i - 1].tool_name;
            warnings.push(format!(
                "step {n} (`reason`): no `context` argument, so it will reason over the \
                 knowledge graph and never see step {} (`{prev}`) output. Pass \
                 `context: \"{{{{_prev}}}}\"` to hand the upstream result in.",
                i
            ));
        }

        // Routing by omission. `use_local` in queue.rs is
        // `provider_hint == Some("ollama")`, so leaving the field out is not
        // "default to local" — it falls through to the active config, which is
        // a cloud model. A chat-authored schedule almost never sets the field,
        // so its every step lands on cloud: six near-duplicate supply-chain
        // schedules created on 2026-08-25 put four searches and a store_note
        // each on a cloud model that had no reason to be involved, and the
        // clobbered hardware tripwire did the same for two weeks.
        //
        // Only *omission* is flagged. An explicit `ollama-cloud` is a decision
        // someone made and is left alone — the failure being caught is the
        // author who never knew the field existed. A step that pins a `model`
        // needs no hint: it runs on that model, and the hint is derived from it.
        if step.provider_hint.is_none() && step.model.as_deref().is_none_or(|m| m.trim().is_empty())
        {
            warnings.push(format!(
                "step {n} (`{tool}`): no `provider_hint`, so it routes to the active \
                 model (a cloud model) rather than local. Set `provider_hint: \"ollama\"` \
                 unless this step genuinely needs the cloud, in which case set it \
                 explicitly to `\"ollama-cloud\"`."
            ));
        }

        // `run_agent` runs a whole sub-agent for the step. Its `agent` and
        // `task` must be present and non-empty: a blank `agent` cannot name a
        // spawnable worker, and a blank `task` gives the agent nothing to do —
        // both fail at runtime. The spawnable-membership check is enforced live
        // by `run_agent` itself (it returns the roster for a bad name); lint has
        // no roster, so it verifies the fields are populated at all.
        if tool == "run_agent" {
            if args
                .and_then(|a| a.get("agent"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`run_agent`): no `agent` argument, so it cannot name a spawnable \
                     worker and will fail every run. Set `agent` to a spawnable agent name."
                ));
            }
            if args
                .and_then(|a| a.get("task"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`run_agent`): no `task` argument, so the agent has nothing to do. \
                     Set `task` to a complete, self-contained instruction (use `{{{{_prev}}}}` to \
                     hand in the upstream result)."
                ));
            }
        }

        if PERSISTING_TOOLS.contains(&tool) && i > 0 && !references_prev(args) {
            warnings.push(format!(
                "step {n} (`{tool}`): arguments contain no `{{{{_prev}}}}`, so it will \
                 persist a fixed literal and discard step {} output.",
                i
            ));
        }

        // A note with no retrieval key cannot be found by the next run, which
        // is what makes a watch schedule a watch rather than a weekly restatement.
        if tool == "store_note" {
            if args
                .and_then(|a| a.get("source_context"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`store_note`): no `source_context`, so a later run has no key \
                     to retrieve this note by and cannot build on it."
                ));
            }
            // An invented note_type is stored verbatim and then never labelled,
            // consolidated, or recalled correctly — retrieval and the epistemics
            // layer only understand the canonical set.
            if let Some(nt) = args
                .and_then(|a| a.get("note_type"))
                .and_then(|c| c.as_str())
                .filter(|t| !t.is_empty())
                && !VALID_NOTE_TYPES.contains(&nt)
            {
                warnings.push(format!(
                    "step {n} (`store_note`): `note_type: \"{nt}\"` is not a recognized type, so \
                     the note is stored but never labelled or recalled correctly. Use one of: {}.",
                    VALID_NOTE_TYPES.join(", ")
                ));
            }
            // Web-derived material is what a source asserted, not what the brain
            // established. Typed `semantic` it comes back out of retrieval
            // unlabelled and gets relayed as settled fact.
            if has_search
                && args
                    .and_then(|a| a.get("note_type"))
                    .and_then(|c| c.as_str())
                    .is_none_or(|t| t == "semantic")
            {
                warnings.push(format!(
                    "step {n} (`store_note`): this chain searches the web, so its output is \
                     what sources asserted, not established knowledge. Use \
                     `note_type: \"source_record\"` so retrieval labels it as unverified."
                ));
            }
        }
    }

    // A chain whose FINAL step only computes has nowhere to put its result.
    //
    // Deliberately narrow. The first version of this rule asked whether any step
    // was in PERSISTING_TOOLS, and flagged five of six live schedules: three
    // persist through their own side effects (`poll_media_sources` creates
    // Tasks, `discover_media_sources` creates MediaSource nodes,
    // `write_codebase_doc` writes a file) and one ends in `reason` with
    // `store_inference: true`. An enumerate-the-persisters rule cannot see any
    // of that, and a lint that is wrong five times out of six is one the next
    // author learns to skip past. So: only the two terminal-compute tools that
    // actually occur, and `reason` only when it is not storing.
    if let Some(last) = steps.last() {
        let computes_only = match last.tool_name.as_str() {
            "search_web" => true,
            "reason" => {
                last.arguments
                    .as_ref()
                    .and_then(|a| a.get("store_inference"))
                    .and_then(|v| v.as_bool())
                    != Some(true)
            }
            _ => false,
        };
        if computes_only {
            warnings.push(format!(
                "final step (`{}`) only computes — the chain's result is not stored, \
                 handed on, or notified anywhere, so every run throws its output away. \
                 Add a `store_note`/`notify_user` step, or set `store_inference: true`.",
                last.tool_name
            ));
        }
    }

    warnings
}

/// The bundles' object-envelope schemas (`schemas/*.json`), set once at
/// startup by `brain_core` so a path that cannot reach the plugin host (the
/// REST workflow save) checks against the same `workflow.json` as
/// `manage_workflow`. Unset (tests, no bundles): no envelope check.
static ENVELOPES: std::sync::OnceLock<std::sync::Arc<HashMap<String, Value>>> =
    std::sync::OnceLock::new();

pub fn set_envelope_schemas(schemas: std::sync::Arc<HashMap<String, Value>>) {
    let _ = ENVELOPES.set(schemas);
}

pub fn envelope_schema(name: &str) -> Option<Value> {
    ENVELOPES.get().and_then(|m| m.get(name).cloned())
}

// ── The dispatch check: every tool call is a form ─────────────────────────

/// How the dispatch check acts on a call whose arguments do not fit the
/// tool's schema (`TOOL_ARG_CHECK`): `off`, `shadow` (record it, run the call
/// anyway; the default) or `enforce` (refuse it, saying what fits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgCheckMode {
    Off,
    Shadow,
    Enforce,
}

impl ArgCheckMode {
    pub fn from_env() -> Self {
        static MODE: std::sync::OnceLock<ArgCheckMode> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            match std::env::var("TOOL_ARG_CHECK")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "off" => ArgCheckMode::Off,
                "enforce" => ArgCheckMode::Enforce,
                _ => ArgCheckMode::Shadow,
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ArgCheckMode::Off => "off",
            ArgCheckMode::Shadow => "shadow",
            ArgCheckMode::Enforce => "enforce",
        }
    }
}

/// What is wrong with one call's `arguments` against the tool's registered
/// `input_schema`, in words a model can act on; empty when they fit.
///
/// A key the schema does not declare is the failure this exists for: serde
/// drops it, the call succeeds, and the model believes it filtered, tagged or
/// limited something (measured 2026-10-16: ~230 calls a month, `list_tasks`
/// `status` alone 73). Types, enums and required fields come from the schema
/// itself. Keys starting `__` are the queue's own stamps and always pass. A
/// schema with `additionalProperties: true` (lookup's per-view slots) or with
/// no `properties` is not checked for unknown keys.
pub fn check_tool_arguments(tool: &str, schema: &Value, args: Option<&Value>) -> Vec<String> {
    let mut given = match args {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(v) => v.clone(),
    };
    if let Some(m) = given.as_object_mut() {
        m.retain(|k, _| !k.starts_with("__"));
    }
    let mut out = Vec::new();
    let props: Option<Vec<String>> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect());
    // `false` is the schema validator's to report; absent is ours.
    let open = schema.get("additionalProperties").is_some();
    if let (Some(props), false, Some(m)) = (&props, open, given.as_object()) {
        let unknown: Vec<&String> = m.keys().filter(|k| !props.contains(k)).collect();
        for k in &unknown {
            let hint = closest(k, props)
                .map(|p| format!(" (did you mean `{p}`?)"))
                .unwrap_or_default();
            out.push(format!("`{k}` is not an argument of {tool}{hint}"));
        }
        if !unknown.is_empty() {
            let mut takes = props.clone();
            takes.sort();
            out.push(if takes.is_empty() {
                format!("{tool} takes no arguments")
            } else {
                format!("{tool} takes: {}", takes.join(", "))
            });
        }
    }
    for v in validate_against_schema("arguments", schema, &given) {
        // An unknown key reported twice (once above, once by a schema that
        // sets additionalProperties itself) would read as two problems.
        if !v.message.contains("Additional properties") || open {
            out.push(v.render());
        }
    }
    out
}

/// The declared argument a misspelled key most likely meant: edit distance
/// at most 2, or one name containing the other (`top_n` → `count` does not
/// qualify; `querry` → `query` and `max_results` → `results` would).
pub fn closest<'a>(key: &str, props: &'a [String]) -> Option<&'a String> {
    let k = key.to_ascii_lowercase();
    props
        .iter()
        .map(|p| (levenshtein(&k, &p.to_ascii_lowercase()), p))
        .filter(|(d, p)| {
            *d <= 2 || (p.len() > 3 && (k.contains(p.as_str()) || p.contains(k.as_str())))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, p)| p)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {

    fn schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
                "status": {"type": "string", "enum": ["open", "done"]}
            },
            "required": ["query"]
        })
    }

    #[test]
    fn fitting_arguments_pass_and_queue_stamps_are_ignored() {
        let a = serde_json::json!({"query": "x", "limit": 3, "__owner_task_id": "t", "__data_class": "public"});
        assert!(check_tool_arguments("search_notes", &schema(), Some(&a)).is_empty());
    }

    /// The silent drop: a misspelled or invented key ran as if it were not there.
    #[test]
    fn an_unknown_key_is_named_with_a_suggestion_and_what_fits() {
        let a = serde_json::json!({"querry": "x", "tags": ["a"]});
        let p = check_tool_arguments("search_notes", &schema(), Some(&a));
        assert!(
            p.iter()
                .any(|m| m.contains("`querry`") && m.contains("did you mean `query`")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|m| m.contains("`tags` is not an argument")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|m| m == "search_notes takes: limit, query, status"),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|m| m.contains("query") && m.contains("required")),
            "{p:?}"
        );
    }

    #[test]
    fn a_tool_with_no_arguments_says_so() {
        let s = serde_json::json!({"type": "object", "properties": {}});
        let a = serde_json::json!({"status": "failed", "limit": 5});
        let p = check_tool_arguments("list_tasks", &s, Some(&a));
        assert!(
            p.contains(&"list_tasks takes no arguments".to_string()),
            "{p:?}"
        );
    }

    #[test]
    fn types_and_enums_come_from_the_schema() {
        let a = serde_json::json!({"query": "x", "limit": "five", "status": "bogus"});
        let p = check_tool_arguments("t", &schema(), Some(&a));
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p.iter().any(|m| m.contains("/limit")), "{p:?}");
        assert!(p.iter().any(|m| m.contains("/status")), "{p:?}");
    }

    #[test]
    fn open_schemas_are_not_checked_for_unknown_keys() {
        let mut s = schema();
        s["additionalProperties"] = serde_json::json!(true);
        let a = serde_json::json!({"query": "x", "api": "github"});
        assert!(check_tool_arguments("lookup", &s, Some(&a)).is_empty());
        let free = serde_json::json!({"type": "object"});
        assert!(check_tool_arguments("t", &free, Some(&a)).is_empty());
    }

    #[test]
    fn a_schema_that_closes_itself_reports_an_unknown_key_once() {
        let mut s = schema();
        s["additionalProperties"] = serde_json::json!(false);
        let a = serde_json::json!({"query": "x", "tags": 1});
        let p = check_tool_arguments("t", &s, Some(&a));
        assert_eq!(p.len(), 1, "{p:?}");
    }

    #[test]
    fn a_raw_graph_write_step_is_found() {
        let steps = vec![
            serde_json::json!({"tool_name": "neo4j_query", "arguments": {"cypher": "MATCH (n) RETURN n"}}),
            serde_json::json!({"tool_name": "neo4j_query", "arguments": {"cypher": "MATCH (n) SET n.external = false", "readonly": false}}),
            serde_json::json!({"tool_name": "store_note", "arguments": {"content": "x", "readonly": false}}),
        ];
        let v = validate_raw_graph_writes(&steps, &[]);
        assert_eq!(v.len(), 1);
        assert!(v[0].path.starts_with("steps[1]"));
        assert!(has_hard(&v));
    }

    /// Pausing an owner's workflow from chat re-saves its steps: a cleanup
    /// step it already had is carried over, a new or changed one is not.
    #[test]
    fn a_stored_raw_write_step_carries_over_unchanged() {
        let cleanup = serde_json::json!({"tool_name": "neo4j_query", "arguments": {
            "cypher": "MATCH (w:WorkingMemory {session_id: 'x'}) DETACH DELETE w", "readonly": false}});
        let mut stamped = cleanup.clone();
        stamped["arguments"]["__data_class"] = serde_json::json!("public");
        assert!(
            validate_raw_graph_writes(
                std::slice::from_ref(&stamped),
                std::slice::from_ref(&cleanup)
            )
            .is_empty()
        );
        let mut changed = cleanup.clone();
        changed["arguments"]["cypher"] = serde_json::json!("MATCH (n) DETACH DELETE n");
        assert_eq!(validate_raw_graph_writes(&[changed], &[cleanup]).len(), 1);
    }

    #[tokio::test]
    async fn only_background_calls_are_not_model_driven() {
        use agent_brain_llm::routing::{DataClass, Purpose, ROUTE_CONTEXT, RouteContext};
        assert!(
            !model_driven_call(),
            "unscoped = a queue step or an operator call"
        );
        for p in [Purpose::Chat, Purpose::Subagent] {
            let driven = ROUTE_CONTEXT
                .scope(RouteContext::new(p, DataClass::Private), async {
                    model_driven_call()
                })
                .await;
            assert!(driven, "{p:?}");
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn a_step_model_must_exist_and_agree_with_its_hint() {
        use agent_brain_llm::model_router::CatalogModel;
        let catalog = vec![
            CatalogModel {
                name: "gemma4:latest".into(),
                provider: "ollama".into(),
                model: "gemma4:latest".into(),
            },
            CatalogModel {
                name: "gpt-oss:120b-cloud".into(),
                provider: "ollama-cloud".into(),
                model: "gpt-oss:120b-cloud".into(),
            },
        ];
        // The Zero Trust schedule as stored on 2026-10-09.
        let zero_trust = vec![
            json!({"tool_name": "search_web", "provider_hint": "ollama-cloud", "model": "gemma4:latest"}),
            json!({"tool_name": "reason", "provider_hint": "ollama", "model": "gemma4:latest"}),
        ];
        let v = validate_step_models(&zero_trust, &catalog);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].path.starts_with("steps[0]"), "{v:?}");
        assert!(
            render_hard(&v).contains("contradicts"),
            "{}",
            render_hard(&v)
        );

        let unknown = vec![json!({"tool_name": "reason", "model": "gemma4"})];
        assert!(
            render_hard(&validate_step_models(&unknown, &catalog))
                .contains("not in the model catalog")
        );

        let ok = vec![
            json!({"tool_name": "reason", "model": "gpt-oss:120b-cloud"}),
            json!({"tool_name": "reason", "model": "GPT-OSS:120B-CLOUD", "provider_hint": "ollama-cloud"}),
            json!({"tool_name": "reason", "provider_hint": "ollama"}),
        ];
        assert!(validate_step_models(&ok, &catalog).is_empty());
        // No catalog: nothing to check against.
        assert!(validate_step_models(&unknown, &[]).is_empty());
    }

    /// Every schema checked into `schemas/` must be valid JSON *and* a schema
    /// the validator can compile — otherwise the envelope layer fails open and
    /// silently validates nothing. This guards against a typo landing in a
    /// committed schema.
    #[test]
    fn all_committed_schemas_compile() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/core/schemas");
        let loaded = load_schema_dir(Path::new(dir));
        assert!(
            !loaded.is_empty(),
            "expected committed schemas under {dir}, found none"
        );
        for (name, schema) in &loaded {
            assert!(
                jsonschema::validator_for(schema).is_ok(),
                "schema `{name}` does not compile"
            );
        }
    }

    /// The Trump Benefit monitor as it was: `context` beside `arguments`.
    #[test]
    fn a_tool_argument_at_step_level_is_rejected() {
        let schemas = HashMap::from([(
            "reason".to_string(),
            json!({"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"}}}),
        )]);
        let steps = vec![json!({
            "tool_name": "reason", "provider_hint": "ollama", "description": "d",
            "arguments": {"question": "q"}, "context": "{{_prev}}"
        })];
        let v = validate_step_array(&steps, &[], &schemas);
        assert!(has_hard(&v), "{v:?}");
        assert!(render_hard(&v).contains("`context`"), "{}", render_hard(&v));
        // Step fields and unknown metadata are fine.
        let ok = vec![
            json!({"tool_name": "reason", "arguments": {"question": "q", "context": "x"},
                             "description": "d", "note": "free-form"}),
        ];
        assert!(!has_hard(&validate_step_array(&ok, &[], &schemas)));
    }

    fn http_request_schema() -> Value {
        // The shape the real `http_request` tool registers.
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET","POST","PUT","PATCH","DELETE","HEAD"] },
                "url": { "type": "string" },
                "query_params": { "type": "object" },
                "timeout_ms": { "type": "integer" },
                "pick": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["method", "url"]
        })
    }

    fn schemas() -> HashMap<String, Value> {
        HashMap::from([("http_request".to_string(), http_request_schema())])
    }

    #[test]
    fn rejects_the_stringified_object_corruption() {
        // Exactly what the HBI editor produced and what killed the job schedule.
        let args = json!({
            "method": "GET",
            "url": "https://aidevboard.com/api/v1/jobs",
            "query_params": "[object Object]",
            "timeout_ms": "15000",
            "pick": "jobs[].url,jobs[].apply_url"
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(has_hard(&v), "must reject: {:?}", v);
        let msg = render_hard(&v);
        assert!(
            msg.contains("query_params"),
            "should name query_params: {msg}"
        );
        assert!(msg.contains("timeout_ms"), "should name timeout_ms: {msg}");
        assert!(msg.contains("pick"), "should name pick: {msg}");
    }

    #[test]
    fn accepts_the_correctly_typed_args() {
        let args = json!({
            "method": "GET",
            "url": "https://aidevboard.com/api/v1/jobs",
            "query_params": { "workplace": "remote", "posted_within_days": "3" },
            "timeout_ms": 15000,
            "pick": ["jobs[].url", "jobs[].apply_url"]
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(!has_hard(&v), "clean args must pass: {:?}", v);
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let args = json!({ "url": "https://example.com" }); // no method
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(has_hard(&v), "missing required must reject: {:?}", v);
    }

    #[test]
    fn unknown_tool_is_skipped_here() {
        // A tool with no schema is not this function's job to reject.
        let args = json!({ "anything": 1 });
        let v = validate_step_arguments(0, "not_a_real_tool", Some(&args), &schemas());
        assert!(v.is_empty(), "unknown tool should be skipped, got: {:?}", v);
    }

    #[test]
    fn step_array_flags_unregistered_tool_and_bad_args() {
        let live = vec!["http_request".to_string()];
        let steps = vec![
            json!({ "tool_name": "http_request", "arguments": { "method": "GET", "url": "https://x" } }),
            json!({ "tool_name": "no_such_tool", "arguments": {} }),
            json!({ "tool_name": "http_request", "arguments": { "method": "GET", "url": "https://x", "timeout_ms": "nope" } }),
            json!({ "arguments": {} }),
        ];
        let v = validate_step_array(&steps, &live, &schemas());
        let msg = render_hard(&v);
        assert!(msg.contains("no_such_tool"), "unregistered tool: {msg}");
        assert!(msg.contains("timeout_ms"), "bad arg type: {msg}");
        assert!(msg.contains("steps[3]"), "missing tool_name: {msg}");
        // The clean step 0 produced nothing.
        assert!(!msg.contains("steps[0]"), "clean step flagged: {msg}");
    }

    fn committed_schemas() -> HashMap<String, Value> {
        load_schema_dir(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/core/schemas"
        )))
    }

    #[test]
    fn workflow_envelope_rejects_empty_steps_bad_names_and_bad_periods() {
        let schema = committed_schemas().remove("workflow").unwrap();
        for bad in [
            json!({"name": "x", "steps": [], "managed_by": "somebody"}),
            json!({"name": "Daily News", "steps": [{"tool_name": "a"}]}),
            json!({"name": "x", "every": "daily", "steps": [{"tool_name": "a"}]}),
            json!({"name": "x", "checks": ["adversarial"], "steps": [{"tool_name": "a"}]}),
        ] {
            let v = validate_against_schema("workflow", &schema, &bad);
            assert!(has_hard(&v), "must reject: {bad}");
        }
    }

    #[test]
    fn workflow_envelope_accepts_a_valid_object() {
        let schema = committed_schemas().remove("workflow").unwrap();
        let good = json!({
            "action": "upsert",
            "name": "ai-dev-board-job-search",
            "every": "1d",
            "inputs": [{"name": "topic"}],
            "checks": ["evaluator"],
            "managed_by": "runtime",
            "steps": [ { "id": "fetch", "tool_name": "http_request", "provider_hint": "ollama",
                         "arguments": { "method": "GET", "url": "https://x" } } ]
        });
        let v = validate_against_schema("workflow", &schema, &good);
        assert!(
            !has_hard(&v),
            "valid workflow must pass: {}",
            render_hard(&v)
        );
    }

    #[test]
    fn template_vars_in_string_fields_are_fine() {
        // `{{_prev}}` etc. stay strings; a string field accepts them.
        let args = json!({
            "method": "GET",
            "url": "https://example.com/{{inputs.topic}}",
            "query_params": { "q": "{{date}}" }
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(!has_hard(&v), "templated strings must pass: {:?}", v);
    }

    #[test]
    fn step_refs_must_name_an_earlier_step() {
        let steps = vec![
            json!({"tool_name": "a", "id": "news", "arguments": {}}),
            json!({"tool_name": "b", "arguments": {"x": "{{steps.news.items}} {{steps.later}}"}}),
            json!({"tool_name": "c", "id": "later", "arguments": {}}),
            json!({"tool_name": "d", "id": "news", "arguments": {}}),
        ];
        let v = step_ref_violations(&steps);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].render().contains("steps.later"));
        assert!(v[0].render().contains("earlier step ids: news"));
        assert!(v[1].render().contains("used twice"));
    }

    #[test]
    fn input_refs_must_be_declared() {
        let steps =
            vec![json!({"tool_name": "a", "arguments": {"q": "{{inputs.topic}} {{inputs.url}}"}})];
        let v = validate_input_refs(&steps, &["topic".to_string()]);
        assert_eq!(v.len(), 1);
        assert!(v[0].render().contains("inputs.url"));
    }
}
