//! Grounding check: a second opinion on every "supports" before it counts.
//!
//! The judge (`assess_evidence`) reads the claim's search results and names
//! the ones that support it; the independence gate counts their domains. Both
//! trust the judge on whether the cited results actually say what the claim
//! says. On 25 corroborated claims graded by hand (2026-10-12), 7 were not:
//! "the NewsNation segment said X" corroborated by any article about X, a
//! claim pointing at "this maritime junction", "Hurricane Isa" by articles
//! about Hurricane Isaias.
//!
//! - **Model:** IBM's `granite4.1-guardian`, a section of `llamacpp/preset.ini`,
//!   criterion `groundedness`: the cited results are the document, the claim is
//!   the answer, and it says `yes` (risk: not grounded) or `no`.
//! - **The prompt is ours to render** (`groundedness_prompt.txt`): the GGUF has no
//!   usable template, so this is the one branch of the model's 14 KB Ollama
//!   template we use, byte for byte what Ollama 0.40.2 rendered (Apache 2.0, IBM).
//!   Sent to `/v1/completions`, with the prompt cache off so a verdict does not
//!   depend on what ran before it.
//! - **All cited results in one document**, never one at a time: a snippet is
//!   a fragment, so per result it flagged 37 of 60 good citations. Together it
//!   caught 5 of the 7 bad claims and flagged 6 of 18 good ones, 4 of those
//!   for real wording faults ("embargo" for temporary restrictions, "Activist
//!   Post claims" for a MintPress article).
//! - **Thinking off** (the prompt's `<no-think>` form): seconds a check instead
//!   of ~57 s, and no worse on the measured sets (crude 72/72 either way; hard 9
//!   vs 10 of 14).
//! - It does not check who said it (0 of 4 attribution faults caught), and it
//!   passes vague claims; the stand-alone check at extraction owns those.
//! - **It also vetoes a contradiction** (`claim_skill.rs`): results that
//!   ground the claim do not refute it. On 68 hand-graded refutations
//!   (2026-10-22) it found 8 of the 53 wrong ones grounded and none of the 15
//!   real ones.
//! - `GROUNDING_CHECK_MODEL` names the model; `off` turns the check off.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The router when `LLAMACPP_URL` is unset (compose sets it).
const DEFAULT_URL: &str = "http://localhost:8080";
const DEFAULT_MODEL: &str = "guardian";
/// The answer is `<score> yes </score>`; anything longer is not an answer.
const MAX_TOKENS: u32 = 20;
/// The model's prompt for this criterion, with `{{document}}` and `{{claim}}`.
const PROMPT: &str = include_str!("groundedness_prompt.txt");
/// Per result, so one long page cannot crowd out the rest.
const RESULT_CHARS: usize = 1200;

/// The check's model, or `None` when `GROUNDING_CHECK_MODEL=off` (or empty).
pub fn model() -> Option<String> {
    let m = std::env::var("GROUNDING_CHECK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let m = m.trim();
    (!m.is_empty() && !m.eq_ignore_ascii_case("off")).then(|| m.to_string())
}

/// The cited results as one numbered document: title and snippet of each, in
/// the order cited. Results the evidence does not contain are skipped (the
/// caller only passes links `resolve_cited_links` matched).
pub fn cited_document(evidence: &str, cited: &[String]) -> String {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(evidence.trim()) else {
        return String::new();
    };
    let field = |i: &Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| i.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let mut parts = Vec::new();
    for link in cited {
        let Some(item) = items
            .iter()
            .find(|i| field(i, &["link", "url"]) == link.as_str())
        else {
            continue;
        };
        let text = format!(
            "{}\n{}",
            field(item, &["title"]),
            field(item, &["snippet", "content", "description"])
        );
        parts.push(format!(
            "[{}] {}",
            parts.len() + 1,
            text.chars().take(RESULT_CHARS).collect::<String>().trim()
        ));
    }
    parts.join("\n\n")
}

/// The prompt for this document and claim. Each is placed once and not
/// re-read, so a marker inside a search result stays text. Not JSON-escaped:
/// the template puts the document in its `text` field as it stands.
pub fn prompt(document: &str, claim: &str) -> String {
    let (head, rest) = PROMPT.split_once("{{document}}").unwrap_or((PROMPT, ""));
    let (middle, tail) = rest.split_once("{{claim}}").unwrap_or((rest, ""));
    [head, document, middle, claim, tail].concat()
}

/// `true` when the claim is grounded in the document (guardian answered
/// `no`: no risk), `false` when it is not (`yes`). An answer without a score
/// is an error naming what came back.
pub fn parse_answer(resp: &Value) -> Result<bool> {
    let content = resp["choices"][0]["text"].as_str().unwrap_or("");
    let score = content
        .split("<score>")
        .nth(1)
        .and_then(|s| s.split("</score>").next())
        .map(|s| s.trim().to_lowercase());
    match score.as_deref() {
        Some("no") => Ok(true),
        Some("yes") => Ok(false),
        _ => bail!(
            "no <score> in the answer: {}",
            content.chars().take(200).collect::<String>()
        ),
    }
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // The first call may load the model from a cold disk.
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default()
    })
}

/// Ask the model whether `claim` is grounded in `document`.
pub async fn is_grounded(model: &str, document: &str, claim: &str) -> Result<bool> {
    let base = std::env::var("LLAMACPP_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let resp = http()
        .post(format!("{}/v1/completions", base.trim_end_matches('/')))
        .json(&json!({
            "model": model,
            "prompt": prompt(document, claim),
            "temperature": 0,
            "max_tokens": MAX_TOKENS,
            "cache_prompt": false,
        }))
        .send()
        .await
        .context("grounding check: request failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "grounding check: {model} answered {status}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
    parse_answer(&serde_json::from_str(&text).context("grounding check: not JSON")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_score_of_no_is_grounded_and_yes_is_not() {
        let r = |c: &str| json!({ "choices": [{ "text": c }] });
        assert!(parse_answer(&r("\n<score> no </score>")).unwrap());
        assert!(!parse_answer(&r("<score> yes </score>")).unwrap());
        let err = parse_answer(&r("I think so")).unwrap_err().to_string();
        assert!(err.contains("I think so"), "{err}");
    }

    #[test]
    fn the_document_holds_only_the_cited_results_in_cited_order() {
        let evidence = json!([
            { "link": "https://a.com/1", "title": "A", "snippet": "alpha" },
            { "link": "https://b.com/2", "title": "B", "snippet": "beta" },
            { "url": "https://c.com/3", "title": "C", "content": "gamma" }
        ])
        .to_string();
        let doc = cited_document(
            &evidence,
            &["https://c.com/3".to_string(), "https://a.com/1".to_string()],
        );
        assert_eq!(doc, "[1] C\ngamma\n\n[2] A\nalpha");
        assert_eq!(cited_document("not json", &["x".to_string()]), "");
    }

    /// What Ollama 0.40.2 rendered for this input (`_debug_render_only`).
    #[test]
    fn the_prompt_is_what_ollama_rendered() {
        let p = prompt(
            "[1] DOC \"ONE\"\nbody <b>&amp;\n\n[2] é two",
            "CLAIM \"x\" <y> é",
        );
        assert_eq!(p, include_str!("groundedness_rendered_by_ollama.txt"));
    }

    #[test]
    fn a_marker_inside_a_result_stays_text() {
        let p = prompt("about {{claim}} here", "THE CLAIM");
        assert!(p.contains("about {{claim}} here"), "{p}");
        assert_eq!(p.matches("THE CLAIM").count(), 1, "{p}");
    }

    /// Against the real model on the router (`LLAMACPP_URL`):
    /// `cargo test -p agent-brain-plugin-epistemics --lib grounding -- --ignored`
    #[tokio::test]
    #[ignore = "needs the router with the guardian model"]
    async fn the_live_model_passes_a_stated_fact_and_flags_an_invented_link() {
        let doc = "[1] UK lawmaker says Russian attacks cost Britain $3.3bn\n\
            Graeme Downie and a government spokesperson claim sustained Russian hybrid and \
            cyber attacks cost the UK roughly $3.3 billion.\n\n\
            [2] Russian-aligned group upgrades malware\n\
            CERT-UA reports that UAC-0099 upgraded malware used against Ukrainian transport \
            and energy firms.";
        let m = model().unwrap_or_else(|| DEFAULT_MODEL.to_string());
        assert!(
            is_grounded(&m, doc, "UK lawmaker Graeme Downie claims Russian hybrid and cyber attacks cost the UK roughly $3.3 billion.")
                .await
                .unwrap()
        );
        assert!(
            !is_grounded(&m, doc, "UAC-0099's malware upgrade against Ukrainian firms cost the UK roughly $3.3 billion.")
                .await
                .unwrap()
        );
    }
}
