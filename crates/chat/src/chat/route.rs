//! The front door's router (GRAPH_CONTEXT_PLAN Phase 3, step 4).
//!
//! Before a front-door turn runs, the local model picks where the turn goes
//! first: a spawnable agent, one of the front door's own tools, or `answer`.
//! The pick is put in front of the front door's model as a short block, and
//! shown in the stream. The profile's `routing:` block holds the rules and the
//! non-agent choices; the agents come from the live roster.
//!
//! - **Picker:** the local background model (`LOCAL_LLM_BACKEND`: the llama.cpp
//!   E4B sidecar), its output held to the choice list. Measured on
//!   `eval/routing_golden.yaml` (66 graded turns): 52 right first.
//! - **Stored first:** a pick of the web agent for a turn that does not ask for
//!   the web or for facts that change goes to the stored agent first, then the
//!   web. Rule, not model: both models sent "Have you looked at Nebula" and
//!   "tell me more about Adam Kinzinger" straight to the web. With it, 56.
//! - **Checker (off unless `CHAT_ROUTER_CHECKER` names a model):** Nimble on
//!   host Ollama answers the same choice with a probability; agreement or a
//!   confident pick goes, a split runs both, stored first (59 tried, 5
//!   confidently wrong vs 9). Off by default: on the shared 8 GB GPU it evicts
//!   bge-m3 every turn, and on the CPU it adds ~8.5 s.
//! - A router failure or timeout leaves the turn as it was, and says so in the
//!   log: routing is advice to the front door, never a reason to fail a turn.
//! - **Behaviors (Phase B):** a pick of `lookup:<name>` is run by code. E4B
//!   fills only the behavior's slots, held to the view's slot schema; `lookup`
//!   runs; its rows go into the front door's prompt as the turn's context. No
//!   sub-agent, no tool choice at runtime. A behavior that fails falls back to
//!   the stored agent.

use std::time::{Duration, Instant};

use agent_brain_graph::topics::TopicDef;
use agent_brain_llm::{LlmConfig, LlmProviderType};
use agent_brain_work::context_builder::{Behavior, ContextProfile, RoutingSpec};
use serde_json::{Value, json};

const PICK_TIMEOUT: Duration = Duration::from_secs(12);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(12);
/// A behavior is offered to the picker as `lookup:<name>`.
pub(super) const BEHAVIOR: &str = "lookup:";
/// The behavior context put in the prompt: rows past this are counted, not shown.
const CONTEXT_CHARS: usize = 12_000;
/// Chars of one row's content shown.
const ROW_CHARS: usize = 700;
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Chars of the previous turn's reply the router reads.
const PREV_REPLY_CHARS: usize = 400;

/// Where a turn goes, and why.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Route {
    /// In order: one stop, or two when the first may come back thin.
    pub plan: Vec<String>,
    pub picked: String,
    pub checked: Option<(String, f64)>,
    pub why: String,
    pub ms: u64,
    /// Our own topics the turn names, `(name, as said)`: the agent reads
    /// them with `lookup`, not a word search (the 2026-10-14 live test sent
    /// "Opensource Intelligence Agency" to `search_notes` as words).
    pub named: Vec<(String, String)>,
}

/// Our topics a message names by name, label or alias, any case, as whole
/// words. Longest names first, so "the off-grid project" wins over a shorter
/// name inside it.
pub(super) fn named_topics(message: &str, topics: &[TopicDef]) -> Vec<(String, String)> {
    let mut said: Vec<(&str, &str)> = topics
        .iter()
        .flat_map(|t| {
            std::iter::once(t.name.as_str())
                .chain(std::iter::once(t.label.as_str()))
                .chain(t.aliases.iter().map(String::as_str))
                .map(move |s| (t.name.as_str(), s.trim()))
        })
        .filter(|(_, s)| s.chars().count() >= 2)
        .collect();
    said.sort_by_key(|(_, s)| std::cmp::Reverse(s.len()));
    let mut found: Vec<(String, String)> = Vec::new();
    for (name, s) in said {
        if found.iter().any(|(n, _)| n == name) {
            continue;
        }
        let re = regex::RegexBuilder::new(&format!(r"(^|\W){}($|\W)", regex::escape(s)))
            .case_insensitive(true)
            .build();
        if let Ok(re) = re
            && let Some(m) = re.find(message)
        {
            let at = message[m.start()..m.end()].trim_matches(|c: char| !c.is_alphanumeric());
            found.push((name.to_string(), at.to_string()));
        }
    }
    found
}

/// The choices in the order the models read them: the agents by name, then
/// the profile's own.
pub(super) fn choices(spec: &RoutingSpec, agents: &[ContextProfile]) -> Vec<(String, String)> {
    let mut a: Vec<(String, String)> = agents
        .iter()
        .map(|p| {
            let says = p
                .specialty
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(p.description.as_str());
            (p.name.clone(), one_line(says))
        })
        .collect();
    a.sort_by(|x, y| x.0.cmp(&y.0));
    a.extend(
        spec.behaviors
            .iter()
            .map(|b| (format!("{BEHAVIOR}{}", b.name), one_line(&b.says))),
    );
    a.extend(
        spec.choices
            .iter()
            .map(|c| (c.name.clone(), one_line(&c.says))),
    );
    a
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The turn as the router reads it: the one before it (for a follow-up) and
/// this message.
pub(super) fn turn_text(prev: Option<(&str, &str)>, message: &str) -> String {
    let mut t = String::new();
    if let Some((q, a)) = prev {
        let a: String = a.chars().take(PREV_REPLY_CHARS).collect();
        t.push_str(&format!("Previous turn - user: {q}\nassistant: {a}\n\n"));
    }
    t.push_str(&format!("User: {message}"));
    t
}

pub(super) fn picker_prompt(
    spec: &RoutingSpec,
    choices: &[(String, String)],
    turn: &str,
) -> String {
    let menu = choices
        .iter()
        .map(|(n, s)| format!("- {n}: {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You route a chat turn for agent-brain's front door. Choose where it goes first.\n\
         Choices:\n{menu}\n{}\n\n{turn}\n\n\
         Answer with JSON: {{\"route\": <one choice>}}.",
        spec.rules.trim()
    )
}

/// The plan from the picks: agreement or a confident check goes; a split runs
/// both, the stored agent first; a web pick the turn did not ask for goes to
/// the stored agent first.
pub(super) fn plan(
    spec: &RoutingSpec,
    picked: &str,
    checked: Option<(&str, f64)>,
    message: &str,
) -> (Vec<String>, String) {
    let (mut plan, mut why) = match checked {
        None => (vec![picked.to_string()], "picked".to_string()),
        Some((c, _)) if c == picked => {
            (vec![picked.to_string()], "picker and checker agree".into())
        }
        Some((c, p)) if p >= spec.confident => (
            vec![c.to_string()],
            format!("checker {p:.2} over the picker's {picked}"),
        ),
        Some((c, p)) => {
            let mut two = vec![picked.to_string(), c.to_string()];
            two.sort_by_key(|x| *x != spec.stored);
            (two, format!("split: picker {picked}, checker {c} {p:.2}"))
        }
    };
    let asks_web = !spec.web_words.trim().is_empty()
        && regex::RegexBuilder::new(&spec.web_words)
            .case_insensitive(true)
            .build()
            .is_ok_and(|r| r.is_match(message));
    if plan.first().is_some_and(|f| *f == spec.web) && !asks_web {
        plan.retain(|x| *x != spec.stored);
        plan.insert(0, spec.stored.clone());
        why.push_str("; stored first (the turn does not ask for the web)");
    }
    (plan, why)
}

/// The block the front door's model reads.
pub(super) fn hint(plan: &[String], named: &[(String, String)]) -> String {
    let stop = |n: &str| match n.strip_prefix("self:") {
        Some(tool) => format!("use your own `{tool}` tool"),
        None if n == "answer" => "answer it yourself from the conversation and what you know; \
             check a fact you are not sure of"
            .to_string(),
        None => format!("delegate to `{n}`"),
    };
    let mut s = String::from(
        "\n\nROUTING FOR THIS TURN (chosen before the turn, from the graded routing rules):\n",
    );
    s.push_str(&format!("- First: {}.\n", stop(&plan[0])));
    if let Some(second) = plan.get(1) {
        s.push_str(&format!(
            "- If that comes back thin or says nothing is stored: {}.\n",
            stop(second)
        ));
    }
    for (name, at) in named {
        s.push_str(&format!(
            "- The turn names our topic `{name}` (as \"{at}\"). Put that in the task: the agent reads it with `lookup` (topic: {name}), not a word search.\n"
        ));
    }
    s.push_str("Follow it unless the turn plainly needs something else; then say why.");
    s
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// One call to the local model, its output held to `schema`; the raw text.
async fn constrained(
    cfg: &LlmConfig,
    prompt: &str,
    schema: &Value,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let base = cfg
        .base_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let text = match cfg.provider {
        LlmProviderType::Ollama => {
            let body = json!({
                "model": cfg.model, "stream": false, "think": false, "format": schema,
                "messages": [{"role": "user", "content": prompt}],
                "options": {"temperature": 0, "num_predict": max_tokens, "num_ctx": 8192},
            });
            let v: Value = http()
                .post(format!("{base}/api/chat"))
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            v["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
        _ => {
            let body = json!({
                "model": cfg.model, "temperature": 0, "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": prompt}],
                "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": schema}},
            });
            let v: Value = http()
                .post(format!("{base}/v1/chat/completions"))
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            v["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };
    Ok(text)
}

/// The picker: the local model, held to the choices.
async fn pick(cfg: &LlmConfig, prompt: &str, names: &[String]) -> anyhow::Result<String> {
    let schema = json!({
        "type": "object", "required": ["route"], "additionalProperties": false,
        "properties": {"route": {"type": "string", "enum": names}}
    });
    let text = constrained(cfg, prompt, &schema, 40).await?;
    let route = agent_brain_llm::llm::first_json_object(&text, &["route"])
        .and_then(|v| v["route"].as_str().map(String::from))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "picker answered no route: {}",
                text.chars().take(120).collect::<String>()
            )
        })?;
    if !names.contains(&route) {
        anyhow::bail!("picker chose '{route}', not a choice");
    }
    Ok(route)
}

/// The behavior a pick names, if it names one.
pub(super) fn behavior<'a>(spec: &'a RoutingSpec, pick: &str) -> Option<&'a Behavior> {
    let name = pick.strip_prefix(BEHAVIOR)?;
    spec.behaviors.iter().find(|b| b.name == name)
}

/// Does the turn match the behavior's `when` and not its `unless`? An empty
/// pattern does not constrain.
pub(super) fn fits(b: &Behavior, message: &str) -> bool {
    let matches = |p: &str| {
        regex::RegexBuilder::new(p)
            .case_insensitive(true)
            .build()
            .is_ok_and(|r| r.is_match(message))
    };
    (b.when.trim().is_empty() || matches(&b.when))
        && (b.unless.trim().is_empty() || !matches(&b.unless))
}

/// The schema the extractor is held to: only the behavior's slots, as the
/// view's live catalog entry declares them. A window is spelled out (the
/// catalog gives it as a described object).
pub(super) fn slot_schema(b: &Behavior, view_slots: &Value) -> Value {
    let mut props = serde_json::Map::new();
    for name in &b.slots {
        let Some(slot) = view_slots.get(name) else {
            continue;
        };
        let schema = if name == "window" {
            json!({"type": "object", "additionalProperties": false, "properties": {
                "hours": {"type": "integer"}, "days": {"type": "integer"},
                "day": {"type": "string"}, "month": {"type": "string"},
                "since": {"type": "string"}, "until": {"type": "string"}}})
        } else {
            Value::Object(
                slot.as_object()
                    .map(|o| {
                        o.iter()
                            .filter(|(k, _)| {
                                matches!(
                                    k.as_str(),
                                    "type" | "enum" | "items" | "minimum" | "maximum"
                                )
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        };
        props.insert(name.clone(), schema);
    }
    json!({"type": "object", "required": ["slots"], "additionalProperties": false,
           "properties": {"slots": {"type": "object", "additionalProperties": false, "properties": props}}})
}

pub(super) fn extract_prompt(
    b: &Behavior,
    view_slots: &Value,
    turn: &str,
    today: &str,
    tz: &str,
) -> String {
    let lines = b
        .slots
        .iter()
        .filter_map(|n| {
            let s = view_slots.get(n)?;
            let mut line = format!("- {n}: {}", s["description"].as_str().unwrap_or(""));
            let vals = s
                .get("enum")
                .or_else(|| s.get("items").and_then(|i| i.get("enum")));
            if let Some(v) = vals.and_then(Value::as_array) {
                let labels = s.get("labels");
                let shown: Vec<String> = v
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|x| match labels.and_then(|l| l[x].as_str()) {
                        Some(l) => format!("{x} ({l})"),
                        None => x.to_string(),
                    })
                    .collect();
                line.push_str(&format!(" One of: {}", shown.join(", ")));
            }
            Some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Fill the slots of the view `{}` ({}) for this chat turn. Today is {today} (local time zone {tz}).\n\
         Slots:\n{lines}\n\n{turn}\n\n\
         Answer with JSON: {{\"slots\": {{...}}}}. Leave a slot out unless the turn needs it.",
        b.view,
        one_line(&b.says)
    )
}

/// The rows `lookup` returned, as the front door's model reads them: what
/// was asked, what the answer says about itself (empty, cut, widened,
/// resolved), then the rows, each note with its kind, standing and source.
pub(super) fn context_block(args: &Value, result: &Value) -> String {
    let mut s =
        format!("\n\nCONTEXT FOR THIS TURN (the brain ran lookup {args} before the turn):\n");
    for k in ["resolved", "wide", "truncated", "empty", "thin"] {
        if let Some(v) = result[k].as_str() {
            s.push_str(&format!("[{k}] {v}\n"));
        }
    }
    let rows = result["rows"].as_array().cloned().unwrap_or_default();
    let mut shown = 0;
    for r in &rows {
        let line = if let Some(c) = r["content"].as_str() {
            let c: String = c.split_whitespace().collect::<Vec<_>>().join(" ");
            let c: String = if c.chars().count() > ROW_CHARS {
                format!("{}…", c.chars().take(ROW_CHARS).collect::<String>())
            } else {
                c
            };
            format!(
                "- [{} · {} · from {} · {}] {c}\n",
                r["kind"].as_str().unwrap_or("note"),
                r["standing"].as_str().unwrap_or("not checked"),
                r["from"].as_str().unwrap_or("unknown"),
                r["at"]
                    .as_str()
                    .map(|a| a.chars().take(16).collect::<String>())
                    .unwrap_or_default(),
            )
        } else {
            format!("- {r}\n")
        };
        if s.len() + line.len() > CONTEXT_CHARS {
            break;
        }
        s.push_str(&line);
        shown += 1;
    }
    if shown < rows.len() {
        s.push_str(&format!(
            "({} more rows not shown here; {} rows came back)\n",
            rows.len() - shown,
            rows.len()
        ));
    }
    s.push_str(
        "Answer from this context; it is what is stored. Write the answer in your own words for the user: \
         say how many there are, then summarize them; never paste the rows or their [bracketed] headers. \
         Keep each note's standing: an unverified claim is said to be unverified, and \"not checked\" \
         means no check has looked at it. If it is empty, say so and what does exist. If it is [thin], \
         say it is far fewer than usual and give the days before.",
    );
    s
}

/// Run a behavior for a turn: the local model fills its slots, `lookup` runs.
/// Returns the call and its result.
pub(super) async fn run_behavior(
    b: &Behavior,
    handler: &agent_brain_work::tools::ToolHandler,
    turn: &str,
) -> anyhow::Result<(Value, Value)> {
    let text_of = |r: &agent_brain_protocol::ToolCallResult| match r.content.first() {
        Some(agent_brain_protocol::Content::Text { text }) => text.clone(),
        _ => String::new(),
    };
    let catalog: Value =
        serde_json::from_str(&text_of(&handler.execute("lookup", Some(json!({}))).await))?;
    let view_slots = catalog["views"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["view"] == b.view.as_str()))
        .map(|v| v["slots"].clone())
        .ok_or_else(|| anyhow::anyhow!("lookup has no view '{}'", b.view))?;
    let mut args = json!({"view": b.view});
    if !b.slots.is_empty() {
        let cfg = agent_brain_llm::llm::build_local_llm_config();
        let prompt = extract_prompt(
            b,
            &view_slots,
            turn,
            &agent_brain_graph::clock::today(),
            agent_brain_graph::clock::timezone_name(),
        );
        let text = tokio::time::timeout(
            EXTRACT_TIMEOUT,
            constrained(&cfg, &prompt, &slot_schema(b, &view_slots), 120),
        )
        .await
        .map_err(|_| anyhow::anyhow!("slot extraction timed out"))??;
        let filled =
            agent_brain_llm::llm::first_json_object(&text, &["slots"]).ok_or_else(|| {
                anyhow::anyhow!(
                    "no slots in: {}",
                    text.chars().take(120).collect::<String>()
                )
            })?;
        if let Some(o) = filled["slots"].as_object() {
            for (k, v) in o {
                let empty = v.is_null()
                    || v.as_str().is_some_and(|s| s.trim().is_empty())
                    || v.as_object().is_some_and(|m| m.is_empty())
                    || v.as_array().is_some_and(|a| a.is_empty());
                if b.slots.contains(k) && !empty {
                    args[k] = v.clone();
                }
            }
        }
    }
    let r = handler.execute("lookup", Some(args.clone())).await;
    let text = text_of(&r);
    if r.is_error == Some(true) {
        anyhow::bail!(
            "lookup {args}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    Ok((args, serde_json::from_str(&text)?))
}

/// The checker's `/v1/systemone` body. Written by hand: the criteria must keep
/// their order, and serde_json maps sort keys (the alias judge's lesson).
pub(super) fn check_body(
    model: &str,
    choices: &[(String, String)],
    spec: &RoutingSpec,
    turn: &str,
) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let criteria = choices
        .iter()
        .map(|(n, s)| format!("{}:{}", q(n), q(s)))
        .collect::<Vec<_>>()
        .join(",");
    let instructions = format!(
        "Where should agent-brain's front door send this chat turn first?\n{}",
        spec.rules.trim()
    );
    format!(
        "{{\"model\":{},\"keep_alive\":\"15m\",\"state\":{{\"turn\":{}}},\"questions\":{{\"route\":{{\"type\":\"choice\",\"instructions\":{},\"criteria\":{{{criteria}}}}}}}}}",
        q(model),
        q(turn),
        q(&instructions)
    )
}

async fn check(model: &str, body: String, names: &[String]) -> anyhow::Result<(String, f64)> {
    let base =
        std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let v: Value = http()
        .post(format!("{}/v1/systemone", base.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let a = &v["answers"]["route"];
    let choice = a["choice"].as_str().unwrap_or_default().to_string();
    if !names.contains(&choice) {
        anyhow::bail!("checker {model} chose '{choice}', not a choice");
    }
    let p = a["probabilities"][&choice].as_f64().unwrap_or(0.0);
    Ok((choice, p))
}

/// The checker's model, or `None` (the default: no checker).
fn checker_model() -> Option<String> {
    std::env::var("CHAT_ROUTER_CHECKER")
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("off"))
}

/// Off with `CHAT_ROUTER=off`.
pub(super) fn enabled() -> bool {
    !std::env::var("CHAT_ROUTER").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

/// Route one turn. `None` when the router fails or times out (logged).
pub(super) async fn route(
    spec: &RoutingSpec,
    agents: &[ContextProfile],
    topics: &[TopicDef],
    prev: Option<(&str, &str)>,
    message: &str,
) -> Option<Route> {
    let t0 = Instant::now();
    let choices = choices(spec, agents);
    let names: Vec<String> = choices.iter().map(|(n, _)| n.clone()).collect();
    let turn = turn_text(prev, message);
    let cfg = agent_brain_llm::llm::build_local_llm_config();
    let prompt = picker_prompt(spec, &choices, &turn);
    let picking = tokio::time::timeout(PICK_TIMEOUT, pick(&cfg, &prompt, &names));
    let checking = async {
        let model = checker_model()?;
        let body = check_body(&model, &choices, spec, &turn);
        match tokio::time::timeout(CHECK_TIMEOUT, check(&model, body, &names)).await {
            Ok(Ok(c)) => Some(c),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "router: the checker failed; the picker decides alone");
                None
            }
            Err(_) => {
                tracing::warn!(model = %model, "router: the checker timed out; the picker decides alone");
                None
            }
        }
    };
    let (picked, checked) = tokio::join!(picking, checking);
    let picked = match picked {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "router: the picker failed; the turn runs unrouted");
            return None;
        }
        Err(_) => {
            tracing::warn!("router: the picker timed out; the turn runs unrouted");
            return None;
        }
    };
    let (mut plan, mut why) = plan(
        spec,
        &picked,
        checked.as_ref().map(|(c, p)| (c.as_str(), *p)),
        message,
    );
    if let Some(b) = behavior(spec, &plan[0])
        && !fits(b, message)
    {
        why.push_str(&format!(
            "; behavior {} does not fit the turn (when), so stored",
            b.name
        ));
        plan[0] = spec.stored.clone();
        plan.dedup();
    }
    Some(Route {
        plan,
        picked,
        checked,
        why,
        ms: t0.elapsed().as_millis() as u64,
        named: named_topics(message, topics),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_brain_work::context_builder::RouteChoice;

    fn spec() -> RoutingSpec {
        RoutingSpec {
            rules: "Rules.".into(),
            choices: vec![RouteChoice {
                name: "answer".into(),
                says: "no tool".into(),
            }],
            stored: "knowledge-worker".into(),
            web: "researcher".into(),
            web_words: r"\b(web|latest|news)\b".into(),
            confident: 0.85,
            behaviors: vec![],
        }
    }

    #[test]
    fn a_web_pick_the_turn_did_not_ask_for_goes_to_the_stored_agent_first() {
        let (p, why) = plan(&spec(), "researcher", None, "Have you looked at Nebula?");
        assert_eq!(p, ["knowledge-worker", "researcher"]);
        assert!(why.contains("stored first"), "{why}");
        let (p, _) = plan(
            &spec(),
            "researcher",
            None,
            "Search the web for the latest Meshtastic firmware",
        );
        assert_eq!(p, ["researcher"]);
        let (p, _) = plan(&spec(), "answer", None, "What is 2+2?");
        assert_eq!(p, ["answer"]);
    }

    #[test]
    fn the_checker_confirms_overrides_when_confident_or_splits_stored_first() {
        let s = spec();
        assert_eq!(
            plan(&s, "answer", Some(("answer", 0.4)), "hi").0,
            ["answer"]
        );
        assert_eq!(
            plan(&s, "answer", Some(("knowledge-worker", 0.9)), "x").0,
            ["knowledge-worker"]
        );
        assert_eq!(
            plan(&s, "answer", Some(("knowledge-worker", 0.6)), "x").0,
            ["knowledge-worker", "answer"]
        );
        // A split whose first stop is the web still checks what is stored first.
        assert_eq!(
            plan(&s, "researcher", Some(("answer", 0.5)), "x").0,
            ["knowledge-worker", "researcher", "answer"]
        );
    }

    #[test]
    fn choices_list_the_agents_by_name_then_the_profile_s_own_in_order() {
        let agent = |n: &str, s: &str| -> ContextProfile {
            serde_json::from_value(json!({"name": n, "description": "d", "specialty": s})).unwrap()
        };
        let c = choices(
            &spec(),
            &[
                agent("researcher", "the  web"),
                agent("knowledge-worker", "stored"),
            ],
        );
        let names: Vec<&str> = c.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["knowledge-worker", "researcher", "answer"]);
        assert_eq!(c[1].1, "the web");
    }

    #[test]
    fn the_checker_body_keeps_the_criteria_in_order() {
        let c = vec![
            ("zeta".to_string(), "z".to_string()),
            ("alpha".to_string(), "a \"q\"".to_string()),
        ];
        let body = check_body("nimble", &c, &spec(), "User: hi");
        let v: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["questions"]["route"]["criteria"]["alpha"], "a \"q\"");
        assert!(body.find("\"zeta\"").unwrap() < body.find("\"alpha\"").unwrap());
    }

    #[test]
    fn the_hint_names_the_stops() {
        let h = hint(&["knowledge-worker".into(), "researcher".into()], &[]);
        assert!(
            h.contains("First: delegate to `knowledge-worker`") && h.contains("thin"),
            "{h}"
        );
        assert!(hint(&["self:manage_workflow".into()], &[]).contains("your own `manage_workflow`"));
        assert!(hint(&["answer".into()], &[]).contains("answer it yourself"));
        let h = hint(
            &["knowledge-worker".into()],
            &[("nia".into(), "Opensource Intelligence Agency".into())],
        );
        assert!(h.contains("topic `nia`") && h.contains("lookup"), "{h}");
    }

    fn learned() -> Behavior {
        Behavior {
            name: "learned".into(),
            view: "learned".into(),
            says: "what the brain learned in a time window".into(),
            slots: vec!["window".into(), "topic".into()],
            when: r"\b(today|yesterday|last|past|this (week|month)|hours?|days?)\b".into(),
            unless: String::new(),
        }
    }

    #[test]
    fn a_behavior_runs_only_when_the_turn_fits() {
        assert!(fits(&learned(), "What did we learn today?"));
        assert!(fits(&learned(), "What have we learned in the last 24 hrs?"));
        assert!(!fits(
            &learned(),
            "What have we learned about mesh networks?"
        ));
        let mut b = learned();
        b.when = String::new();
        assert!(fits(&b, "anything"));
    }

    #[test]
    fn a_behavior_is_a_choice_and_its_pick_finds_it() {
        let mut s = spec();
        s.behaviors = vec![learned()];
        let names: Vec<String> = choices(&s, &[]).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["lookup:learned", "answer"]);
        assert_eq!(
            behavior(&s, "lookup:learned").map(|b| b.view.as_str()),
            Some("learned")
        );
        assert!(behavior(&s, "knowledge-worker").is_none());
        assert!(behavior(&s, "lookup:nope").is_none());
    }

    #[test]
    fn the_extractor_may_fill_only_the_behavior_s_slots() {
        let view_slots = json!({
            "window": {"type": "object", "description": "a window"},
            "topic": {"type": "string", "enum": ["nia", "ai-ml"], "description": "a topic", "labels": {"nia": "Niche Intelligence Agency"}},
            "status": {"type": "string", "enum": ["unverified"]},
        });
        let schema = slot_schema(&learned(), &view_slots);
        let props = &schema["properties"]["slots"]["properties"];
        assert!(props.get("status").is_none(), "{schema}");
        assert_eq!(props["topic"]["enum"], json!(["nia", "ai-ml"]));
        assert!(props["topic"].get("labels").is_none());
        assert_eq!(props["window"]["properties"]["day"]["type"], "string");
        let prompt = extract_prompt(
            &learned(),
            &view_slots,
            "User: what did we learn today?",
            "2026-10-14",
            "America/Detroit",
        );
        assert!(
            prompt.contains("Today is 2026-10-14")
                && prompt.contains("nia (Niche Intelligence Agency)"),
            "{prompt}"
        );
        assert!(!prompt.contains("status"));
    }

    #[test]
    fn the_context_keeps_standing_and_says_what_was_cut() {
        let rows: Vec<Value> = (0..40)
            .map(|i| json!({"at": "2026-10-14T13:00:00Z", "kind": "claim", "from": "daily_news",
                            "standing": "unverified", "content": format!("claim {i} {}", "x".repeat(900))}))
            .collect();
        let c = context_block(
            &json!({"view": "learned", "window": {"day": "today"}}),
            &json!({"count": 40, "rows": rows, "truncated": "40 of 55 rows"}),
        );
        assert!(
            c.contains("[claim · unverified · from daily_news · 2026-10-14T13:00]"),
            "{c}"
        );
        assert!(c.contains("[truncated] 40 of 55 rows"));
        assert!(c.contains("more rows not shown here"));
        assert!(c.len() < CONTEXT_CHARS + 1000);
        let e = context_block(
            &json!({"view": "learned"}),
            &json!({"count": 0, "rows": [], "empty": "No notes today. Without the window: 2303."}),
        );
        assert!(e.contains("[empty] No notes today. Without the window: 2303."));
        let t = context_block(
            &json!({"view": "learned"}),
            &json!({"count": 1, "rows": [], "thin": "1 found, against about 120 usual"}),
        );
        assert!(t.contains("[thin] 1 found, against about 120 usual"), "{t}");
    }

    #[test]
    fn a_delegated_task_says_where_a_named_topic_is_read() {
        let t = super::super::delegate::with_named_topics(
            "Status of the Opensource Intelligence Agency?",
            &[("nia".into(), "Opensource Intelligence Agency".into())],
        );
        assert!(
            t.contains(r#"lookup {"view": "notes", "topic": "nia"}"#),
            "{t}"
        );
        assert_eq!(super::super::delegate::with_named_topics("x", &[]), "x");
    }

    #[test]
    fn a_topic_is_named_by_its_name_label_or_alias_as_whole_words() {
        let t = |name: &str, label: &str, aliases: &[&str]| TopicDef {
            name: name.into(),
            label: label.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };
        let topics = [
            t(
                "nia",
                "Niche Intelligence Agency",
                &["Opensource Intelligence Agency", "our intelligence agency"],
            ),
            t("ai-ml", "AI & machine learning", &["AI"]),
            t(
                "off-grid-networking",
                "Off-grid & mesh networking",
                &["mesh networks"],
            ),
        ];
        assert_eq!(
            named_topics(
                "What about our Opensource Intelligence Agency we were working on ?",
                &topics
            ),
            [(
                "nia".to_string(),
                "Opensource Intelligence Agency".to_string()
            )]
        );
        assert_eq!(
            named_topics("What do we know about Ai", &topics)[0].0,
            "ai-ml"
        );
        assert_eq!(
            named_topics("What have we learned about mesh networks ?", &topics)[0].0,
            "off-grid-networking"
        );
        // Whole words only: "said" holds "ai", "niacin" holds "nia".
        assert!(named_topics("He said niacin helps", &topics).is_empty());
    }
}
