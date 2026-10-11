//! Alias verdicts: a local classifier's opinion on each merge candidate
//! (HBI plan §11.10). The person still decides every merge; the verdict only
//! sorts the list and says what the classifier saw.
//!
//! - **Model:** `nimble` (Bespoke Labs, a 9B decision model fine-tuned from
//!   Qwen3.5) on host Ollama's `/v1/systemone`, which answers a fixed choice
//!   with a probability per option and no free text. `ALIAS_JUDGE_MODEL`
//!   names it; `off` turns the judge off.
//! - **Measured** on 40 live candidates graded by hand (2026-10-09): 33 right,
//!   and no pair of different things called "same". Its misses are cautious
//!   (Prince William → related, Columbus → different), so `different` means
//!   "not shown to be the same", and the HBI says so.
//! - **Stored:** `(from)-[:ALIAS_VERDICT {verdict, p_same, p_related,
//!   p_different, model, version, at}]->(into)`, one per candidate pair. A
//!   verdict from another prompt version or model is judged again.
//! - **Runs** on idle time, one pair at a time, with a short `keep_alive` so
//!   the model leaves the GPU right after a batch.
//! - **Sorting** by the stored verdict (`rank`) is the core's
//!   (`entity_resolution`): with this plugin off, candidates are unranked.

use std::sync::Arc;
use std::time::Duration;

use agent_brain_plugin_api::graph::query;
use agent_brain_plugin_api::{Graph, PluginContext};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

/// Host Ollama when `OLLAMA_LOCAL_URL` is unset (the core's default).
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Bumped whenever the question, the criteria or their order change, so old
/// verdicts are redone. `v2` was sent with alphabetical keys by mistake.
pub const PROMPT_VERSION: &str = "v2-ordered";

const INSTRUCTIONS: &str = "Do entity_a and entity_b name the same real-world thing in these texts? \
Judge by what the texts say about each, not by the names alone. A surname, first name or title used \
for a person is the same person when the texts are about that person. A plural and its singular are \
the same thing. A transcription misspelling of a name is the same thing.";
const SAME: &str =
    "One thing: an acronym, alias, short form, title, plural or spelling of the other";
const RELATED: &str = "Two different things that are connected: two members of one family, an agency and its parent, a product and its maker";
const DIFFERENT: &str = "Two different things, or the texts don't say enough to tell";

/// Texts shown per side, and their length: the 40-pair measurement's sizes.
const TEXTS_PER_SIDE: i64 = 2;
const TEXT_CHARS: usize = 500;
const KEEP_ALIVE: &str = "60s";
/// Pairs judged per idle pass, re-checking for real work between pairs.
const BATCH: usize = 10;

/// The judge's model, or `None` when `ALIAS_JUDGE_MODEL=off` (or empty).
pub fn model() -> Option<String> {
    let m = std::env::var("ALIAS_JUDGE_MODEL").unwrap_or_else(|_| "nimble".to_string());
    let m = m.trim();
    (!m.is_empty() && !m.eq_ignore_ascii_case("off")).then(|| m.to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub verdict: String,
    pub p_same: f64,
    pub p_related: f64,
    pub p_different: f64,
}

/// What the judge reads about one pair.
///
/// **Field order is part of the prompt.** nimble's answer depends on the order
/// of the texts and of the options: sent alphabetically (as `serde_json`'s
/// `json!` map does), "Mills" and "Doug Mills" went from different (0.81) to
/// same (0.95). Structs serialize in declaration order, so the request is
/// built from structs, never from a `Value` map.
#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub entity_a: String,
    pub entity_b: String,
    pub texts_mentioning_both: Vec<String>,
    pub texts_mentioning_a: Vec<String>,
    pub texts_mentioning_b: Vec<String>,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    keep_alive: &'a str,
    state: &'a State,
    questions: Questions,
}

#[derive(Serialize)]
struct Questions {
    same: Question,
}

#[derive(Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'static str,
    criteria: Criteria,
}

/// In the measured order: same, related, different.
#[derive(Serialize)]
struct Criteria {
    same: &'static str,
    related: &'static str,
    different: &'static str,
}

/// The `/v1/systemone` request body for one pair, as JSON text in field order.
pub fn request(model: &str, state: &State) -> String {
    serde_json::to_string(&Request {
        model,
        keep_alive: KEEP_ALIVE,
        state,
        questions: Questions {
            same: Question {
                kind: "choice",
                instructions: INSTRUCTIONS,
                criteria: Criteria {
                    same: SAME,
                    related: RELATED,
                    different: DIFFERENT,
                },
            },
        },
    })
    .unwrap_or_default()
}

/// Read the answer to the `same` question. An error names what came back.
pub fn parse_answer(resp: &Value) -> Result<Verdict> {
    let a = &resp["answers"]["same"];
    let Some(choice) = a["choice"].as_str() else {
        bail!(
            "no answer to 'same' in {}",
            truncate(&resp.to_string(), 200)
        );
    };
    if !matches!(choice, "same" | "related" | "different") {
        bail!("unexpected choice '{choice}'");
    }
    let p = |k: &str| a["probabilities"][k].as_f64().unwrap_or(0.0);
    Ok(Verdict {
        verdict: choice.to_string(),
        p_same: p("same"),
        p_related: p("related"),
        p_different: p("different"),
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // The first call loads ~9.5 GB from disk.
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default()
    })
}

/// The texts the judge reads: notes that mention both, then each alone.
async fn state(graph: &Arc<dyn Graph>, from: &str, into: &str) -> Result<State> {
    let rows = graph
        .execute(
            query(
                "MATCH (a:Entity {id: $a}), (b:Entity {id: $b}) \
                 RETURN a.name AS an, b.name AS bn, \
                   [(a)<-[:MENTIONS]-(n:Note)-[:MENTIONS]->(b) | n.content][..$k] AS both, \
                   [(a)<-[:MENTIONS]-(n:Note) WHERE NOT (n)-[:MENTIONS]->(b) | n.content][..$k] AS only_a, \
                   [(b)<-[:MENTIONS]-(n:Note) WHERE NOT (n)-[:MENTIONS]->(a) | n.content][..$k] AS only_b",
            )
            .param("a", from)
            .param("b", into)
            .param("k", TEXTS_PER_SIDE),
        )
        .await?;
    let r = rows.first().context("entity not found")?;
    let texts = |k: &str| -> Vec<String> {
        r.get::<Vec<String>>(k)
            .unwrap_or_default()
            .iter()
            .map(|t| t.chars().take(TEXT_CHARS).collect())
            .collect()
    };
    Ok(State {
        entity_a: r.get::<String>("an").unwrap_or_default(),
        entity_b: r.get::<String>("bn").unwrap_or_default(),
        texts_mentioning_both: texts("both"),
        texts_mentioning_a: texts("only_a"),
        texts_mentioning_b: texts("only_b"),
    })
}

/// Judge one candidate pair and store the verdict.
pub async fn judge(graph: &Arc<dyn Graph>, from: &str, into: &str) -> Result<Verdict> {
    let model = model().context("the alias judge is off (ALIAS_JUDGE_MODEL=off)")?;
    let base = std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
    let body = request(&model, &state(graph, from, into).await?);
    let resp = http()
        .post(format!("{}/v1/systemone", base.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("alias judge: request failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "alias judge: {model} answered {status}: {}",
            truncate(&text, 200)
        );
    }
    let v = parse_answer(&serde_json::from_str(&text).context("alias judge: not JSON")?)?;
    graph
        .run(
            query(
                "MATCH (a:Entity {id: $a}), (b:Entity {id: $b}) \
                 MERGE (a)-[v:ALIAS_VERDICT]->(b) \
                 SET v.verdict = $verdict, v.p_same = $ps, v.p_related = $pr, v.p_different = $pd, \
                     v.model = $model, v.version = $version, v.at = datetime()",
            )
            .param("a", from)
            .param("b", into)
            .param("verdict", v.verdict.clone())
            .param("ps", v.p_same)
            .param("pr", v.p_related)
            .param("pd", v.p_different)
            .param("model", model)
            .param("version", PROMPT_VERSION),
        )
        .await?;
    Ok(v)
}

/// The idle-time judge: candidates with no current verdict, a few per pass.
pub fn spawn(ctx: &PluginContext) {
    if !ctx.background_allowed() {
        return;
    }
    let (Some(graph), Some(entities)) = (ctx.graph.clone(), ctx.entities.clone()) else {
        warn!("alias judge: the host gave no graph or entities; not started");
        return;
    };
    let ctx = ctx.clone();
    let Some(model) = model() else {
        info!("Alias judge disabled (ALIAS_JUDGE_MODEL=off)");
        return;
    };
    info!(model = %model, "Alias judge started");
    tokio::spawn(async move {
        const IDLE_POLL: Duration = Duration::from_secs(120);
        loop {
            tokio::time::sleep(IDLE_POLL).await;
            if ctx.busy().await {
                continue;
            }
            let cands = match entities.merge_candidates(None, usize::MAX).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "alias judge: could not list candidates");
                    continue;
                }
            };
            let waiting: Vec<(String, String)> = cands["candidates"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| {
                    c["judge"]["version"].as_str() != Some(PROMPT_VERSION)
                        || c["judge"]["model"].as_str() != Some(model.as_str())
                })
                .filter_map(|c| {
                    Some((
                        c["from"]["id"].as_str()?.to_string(),
                        c["into"]["id"].as_str()?.to_string(),
                    ))
                })
                .collect();
            let mut done = 0;
            for (a, b) in waiting.iter().take(BATCH) {
                if ctx.busy().await {
                    break;
                }
                match judge(&graph, a, b).await {
                    Ok(_) => done += 1,
                    Err(e) => {
                        // Usually the model is missing or Ollama is down: wait a pass.
                        warn!(from = %a, into = %b, error = %e, "alias judge: failed");
                        break;
                    }
                }
            }
            if done > 0 {
                info!(
                    judged = done,
                    waiting = waiting.len() - done,
                    "alias judge: pass done"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_choice_with_probabilities() {
        let r = json!({"answers": {"same": {"type": "choice", "choice": "related",
            "probabilities": {"same": 0.1, "related": 0.8, "different": 0.1}}}});
        let v = parse_answer(&r).unwrap();
        assert_eq!(v.verdict, "related");
        assert!((v.p_related - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_missing_or_unknown_answer_is_an_error_naming_it() {
        let e = parse_answer(&json!({"error": "model not found"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("model not found"), "{e}");
        let r = json!({"answers": {"same": {"choice": "maybe", "probabilities": {}}}});
        assert!(parse_answer(&r).unwrap_err().to_string().contains("maybe"));
    }

    #[test]
    fn the_request_keeps_the_measured_order() {
        let st = State {
            entity_a: "who".into(),
            entity_b: "world health organization".into(),
            texts_mentioning_both: vec!["both".into()],
            texts_mentioning_a: vec![],
            texts_mentioning_b: vec![],
        };
        let r = request("nimble", &st);
        let at = |k: &str| r.find(k).unwrap_or_else(|| panic!("{k} missing in {r}"));
        // Options: same, related, different. Texts: both, then a, then b.
        assert!(
            at("\"same\":\"One thing") < at("\"related\":")
                && at("\"related\":") < at("\"different\":")
        );
        assert!(
            at("\"texts_mentioning_both\":") < at("\"texts_mentioning_a\":")
                && at("\"texts_mentioning_a\":") < at("\"texts_mentioning_b\":")
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["keep_alive"], KEEP_ALIVE);
        assert_eq!(v["questions"]["same"]["type"], "choice");
        assert_eq!(v["state"]["entity_a"], "who");
    }
}
