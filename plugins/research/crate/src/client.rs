//! Parallel Task API client (docs.parallel.ai, read 2026-10-16).
//!
//! A run is created, then its result is long-polled: `GET …/result?timeout=N`
//! answers 200 with the output, 408 while the run is still active, and 404
//! when it failed (or the id is unknown; `GET …/runs/{id}` tells which). The
//! output is requested as a markdown report (`output_schema: text`); its
//! `basis` holds every cited source with excerpts and a confidence.
//!
//! Parallel keeps request logs (zero retention is Enterprise-only), so only
//! the question leaves the brain: no context is passed through (owner's
//! decision 2026-10-16, the same exposure as a `search_web` query).

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BASE: &str = "https://api.parallel.ai";

/// Characters kept from each citation's first excerpt.
const EXCERPT_CHARS: usize = 300;

pub struct ParallelClient {
    http: reqwest::Client,
    key: String,
}

/// One cited source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

/// A finished run's output.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub content: String,
    pub confidence: Option<String>,
    pub citations: Vec<Citation>,
}

/// Where a run stands.
#[derive(Debug)]
pub enum Poll {
    Done(Report),
    Running,
    Failed(String),
}

impl ParallelClient {
    pub fn new(key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            key,
        }
    }

    /// Start a run; returns its id.
    pub async fn create(&self, question: &str, processor: &str) -> Result<String> {
        let resp = self
            .http
            .post(format!("{BASE}/v1/tasks/runs"))
            .header("x-api-key", &self.key)
            .timeout(Duration::from_secs(60))
            .json(&json!({
                "input": question,
                "processor": processor,
                "task_spec": {"output_schema": {"type": "text"}},
            }))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status.as_u16() == 402 {
            return Err(anyhow!(
                "Parallel refused the run: the account is out of credit (402). \
                 Top up at platform.parallel.ai or wait for the monthly free credit."
            ));
        }
        if !status.is_success() {
            return Err(anyhow!("Parallel {status}: {}", error_message(&body)));
        }
        body["run_id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("Parallel accepted the run but returned no run_id: {body}"))
    }

    /// Wait up to `timeout_secs` (Parallel caps it at 600) for the result.
    pub async fn result(&self, run_id: &str, timeout_secs: u64) -> Result<Poll> {
        let timeout_secs = timeout_secs.clamp(1, 600);
        let resp = self
            .http
            .get(format!("{BASE}/v1/tasks/runs/{run_id}/result"))
            .query(&[("timeout", timeout_secs)])
            .header("x-api-key", &self.key)
            .timeout(Duration::from_secs(timeout_secs + 30))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        match status {
            200 => Ok(Poll::Done(parse_report(&body))),
            408 => Ok(Poll::Running),
            404 => self.why_not_found(run_id).await,
            _ => Err(anyhow!("Parallel {status}: {}", error_message(&body))),
        }
    }

    /// A 404 on the result is a failed run or an unknown id; the run itself
    /// says which, and why.
    async fn why_not_found(&self, run_id: &str) -> Result<Poll> {
        let resp = self
            .http
            .get(format!("{BASE}/v1/tasks/runs/{run_id}"))
            .header("x-api-key", &self.key)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Ok(Poll::Failed(format!(
                "Parallel does not know run {run_id} ({status})"
            )));
        }
        Ok(match body["status"].as_str() {
            Some("failed") => Poll::Failed(run_errors(&body)),
            Some("completed") => Poll::Failed(format!(
                "run {run_id} completed but its result was not found"
            )),
            _ => Poll::Running,
        })
    }
}

fn error_message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| body.to_string())
}

fn run_errors(run: &Value) -> String {
    let msgs: Vec<String> = run["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            e["message"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| e.to_string())
        })
        .collect();
    if msgs.is_empty() {
        "the run failed; Parallel gave no reason".into()
    } else {
        format!("the run failed: {}", msgs.join("; "))
    }
}

fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The report and its sources from a result body. A text-schema output is a
/// markdown string; an auto-schema one (not requested, but tolerated) is
/// rendered as pretty JSON so nothing is dropped.
pub fn parse_report(body: &Value) -> Report {
    let out = &body["output"];
    let content = match &out["content"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    };
    let basis = out["basis"].as_array().cloned().unwrap_or_default();
    let confidence = basis
        .iter()
        .find_map(|b| b["confidence"].as_str())
        .map(String::from);
    let mut citations: Vec<Citation> = Vec::new();
    for c in basis
        .iter()
        .flat_map(|b| b["citations"].as_array().cloned().unwrap_or_default())
    {
        let Some(url) = c["url"].as_str() else {
            continue;
        };
        if citations.iter().any(|k| k.url == url) {
            continue;
        }
        citations.push(Citation {
            url: url.to_string(),
            title: c["title"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from),
            excerpt: c["excerpts"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(|e| clip(e, EXCERPT_CHARS))
                .filter(|e| !e.is_empty()),
        });
    }
    Report {
        content,
        confidence,
        citations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_report_keeps_its_sources_once_each() {
        let body = json!({"output": {
            "type": "text",
            "content": "# Report\n\nFindings [1].",
            "basis": [{
                "field": "output",
                "confidence": "medium",
                "citations": [
                    {"url": "https://a.example/x", "title": " A ", "excerpts": ["first   line\nsecond"]},
                    {"url": "https://a.example/x", "excerpts": ["again"]},
                    {"url": "https://b.example/y", "excerpts": []},
                    {"title": "no url"}
                ]
            }]
        }});
        let r = parse_report(&body);
        assert_eq!(r.content, "# Report\n\nFindings [1].");
        assert_eq!(r.confidence.as_deref(), Some("medium"));
        assert_eq!(r.citations.len(), 2);
        assert_eq!(r.citations[0].title.as_deref(), Some("A"));
        assert_eq!(r.citations[0].excerpt.as_deref(), Some("first line second"));
        assert_eq!(r.citations[1].excerpt, None);
    }

    #[test]
    fn an_auto_schema_output_is_kept_as_json() {
        let body = json!({"output": {"content": {"output": "answer"}, "basis": []}});
        let r = parse_report(&body);
        assert!(r.content.contains("\"output\": \"answer\""));
        assert_eq!(r.confidence, None);
        assert!(r.citations.is_empty());
    }

    #[test]
    fn a_failed_run_names_its_errors() {
        let run = json!({"status": "failed", "errors": [{"message": "upstream"}, {"code": 1}]});
        assert_eq!(run_errors(&run), "the run failed: upstream; {\"code\":1}");
        assert_eq!(
            run_errors(&json!({"status": "failed"})),
            "the run failed; Parallel gave no reason"
        );
    }

    #[test]
    fn long_excerpts_are_clipped() {
        let s = "x".repeat(EXCERPT_CHARS + 5);
        assert_eq!(clip(&s, EXCERPT_CHARS).chars().count(), EXCERPT_CHARS + 1);
    }
}
