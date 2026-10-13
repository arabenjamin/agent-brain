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
//! - **Model:** IBM's `granite4.1-guardian` on host Ollama, criterion
//!   `groundedness`: the cited results are the document, the claim is the
//!   answer, and it says `yes` (risk: not grounded) or `no`.
//! - **All cited results in one document**, never one at a time: a snippet is
//!   a fragment, so per result it flagged 37 of 60 good citations. Together it
//!   caught 5 of the 7 bad claims and flagged 6 of 18 good ones, 4 of those
//!   for real wording faults ("embargo" for temporary restrictions, "Activist
//!   Post claims" for a MintPress article).
//! - **Thinking off** (`think: false`): 3-5 s a check instead of ~57 s, and no
//!   worse on the measured sets (crude 72/72 either way; hard 9 vs 10 of 14).
//! - It does not check who said it (0 of 4 attribution faults caught), and it
//!   passes vague claims; the stand-alone check at extraction owns those.
//! - `GROUNDING_CHECK_MODEL` names the model; `off` turns the check off.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

/// Host Ollama when `OLLAMA_LOCAL_URL` is unset (the core's default).
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "granite4.1-guardian:8b";
/// Leaves the GPU soon after a sweep (20 claims per 6 h).
const KEEP_ALIVE: &str = "60s";
/// Eight results' titles and snippets fit with room to spare.
const NUM_CTX: u32 = 8192;
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

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct Options {
    temperature: f32,
    num_ctx: u32,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    stream: bool,
    think: bool,
    keep_alive: &'a str,
    options: Options,
}

/// The `/api/chat` body: guardian's template reads the criterion from the
/// system turn and the source from the `document` turn, and scores the last
/// assistant turn.
pub fn request(model: &str, document: &str, claim: &str) -> String {
    serde_json::to_string(&Request {
        model,
        messages: vec![
            Message {
                role: "system",
                content: "groundedness",
            },
            Message {
                role: "document",
                content: document,
            },
            Message {
                role: "user",
                content: "What do these sources report?",
            },
            Message {
                role: "assistant",
                content: claim,
            },
        ],
        stream: false,
        think: false,
        keep_alive: KEEP_ALIVE,
        options: Options {
            temperature: 0.0,
            num_ctx: NUM_CTX,
        },
    })
    .unwrap_or_default()
}

/// `true` when the claim is grounded in the document (guardian answered
/// `no`: no risk), `false` when it is not (`yes`). An answer without a score
/// is an error naming what came back.
pub fn parse_answer(resp: &Value) -> Result<bool> {
    let content = resp["message"]["content"].as_str().unwrap_or("");
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
            // The first call loads ~8 GB from disk, mostly onto the CPU.
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default()
    })
}

/// Ask the model whether `claim` is grounded in `document`.
pub async fn is_grounded(model: &str, document: &str, claim: &str) -> Result<bool> {
    let base = std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
    let resp = http()
        .post(format!("{}/api/chat", base.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(request(model, document, claim))
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
    use serde_json::json;

    #[test]
    fn a_score_of_no_is_grounded_and_yes_is_not() {
        let r = |c: &str| json!({ "message": { "content": c } });
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

    #[test]
    fn the_request_turns_thinking_off_and_puts_the_claim_last() {
        let v: Value = serde_json::from_str(&request("g", "DOC", "CLAIM")).unwrap();
        assert_eq!(v["think"], false);
        assert_eq!(v["messages"][0]["content"], "groundedness");
        assert_eq!(v["messages"][1]["role"], "document");
        assert_eq!(v["messages"][3]["role"], "assistant");
        assert_eq!(v["messages"][3]["content"], "CLAIM");
    }

    /// Against the real model on host Ollama:
    /// `cargo test -p agent-brain-plugin-epistemics --lib grounding -- --ignored`
    #[tokio::test]
    #[ignore = "needs host Ollama with the guardian model"]
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
