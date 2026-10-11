//! The LLM interface skills and services depend on, rather than a concrete
//! client, so they can be tested in isolation and swapped at runtime.

use async_trait::async_trait;
use serde_json::Value;

// ============================================================================
// LlmProvider
// ============================================================================

/// Minimal LLM interface used by skills.
///
/// The concrete implementation is [`crate::shared_llm::SharedLlm`]
/// which wraps the live `Arc<RwLock<Option<LlmConfig>>>`.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Generate text from a prompt with an optional system message.
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String>;

    /// Generate a dense embedding vector for `text`.
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Human-readable model identifier (e.g. `"granite3.3:8b"`).
    fn model_name(&self) -> &str;

    /// Return `true` if the backing LLM is currently configured.
    fn is_available(&self) -> bool;

    /// Generate strict JSON with self-correcting retries.
    ///
    /// Local models routinely wrap JSON in prose or markdown fences, emit
    /// trailing commentary, or drop required fields. This helper calls
    /// [`generate`](Self::generate), extracts the JSON payload, and parses it.
    /// On a parse failure — or when a required top-level key is missing from a
    /// JSON object — it re-prompts the model with the *specific* error appended
    /// (the "targeted self-correction" pattern) up to `max_retries` extra
    /// attempts, then returns the parsed [`Value`] or the last error.
    ///
    /// `required_keys` is only enforced for JSON objects; a bare array or
    /// scalar passes as long as it parses (pass `&[]` to accept any valid JSON).
    async fn generate_json(
        &self,
        prompt: &str,
        system: Option<&str>,
        required_keys: &[&str],
        max_retries: u32,
    ) -> anyhow::Result<Value> {
        let mut current = prompt.to_string();
        let mut attempt = 0u32;
        loop {
            let raw = self.generate(&current, system).await?;
            // With required keys, take the first complete object that has them.
            // Anything after it (a repeated answer, a second fence, prose) is
            // ignored instead of costing a retry — see `first_json_object`.
            if !required_keys.is_empty()
                && let Some(v) = crate::llm::first_json_object(&raw, required_keys)
            {
                return Ok(v);
            }
            let candidate = crate::llm::extract_json(&raw);
            let correction = match serde_json::from_str::<Value>(candidate) {
                Ok(value) => {
                    let missing: Vec<&str> = value
                        .as_object()
                        .map(|obj| {
                            required_keys
                                .iter()
                                .copied()
                                .filter(|k| !obj.contains_key(*k))
                                .collect()
                        })
                        .unwrap_or_default();
                    if missing.is_empty() {
                        return Ok(value);
                    }
                    format!(
                        "Your previous response was missing required field(s): {missing:?}. \
                         Return ONLY a JSON object containing every required field. \
                         No prose, no markdown fences."
                    )
                }
                Err(e) => format!(
                    "Your previous response was not valid JSON ({e}). \
                     Return ONLY a valid JSON value — no prose, no markdown fences, no comments."
                ),
            };

            if attempt >= max_retries {
                let preview: String = raw.chars().take(200).collect();
                anyhow::bail!(
                    "LLM failed to produce valid JSON after {} attempt(s): {correction} \
                     Last output: {preview}",
                    attempt + 1
                );
            }
            current = format!("{prompt}\n\n{correction}");
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod generate_json_tests {
    use super::*;
    use std::sync::Mutex;

    /// LLM stub that returns a scripted response per call, in order. Records the
    /// prompts it received so tests can assert the self-correction re-prompt.
    struct ScriptedLlm {
        responses: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: Mutex::new(responses.iter().rev().map(|s| s.to_string()).collect()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedLlm {
        async fn generate(&self, prompt: &str, _system: Option<&str>) -> anyhow::Result<String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow::anyhow!("no more scripted responses"))
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![])
        }
        fn model_name(&self) -> &str {
            "scripted"
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn clean_json_first_try_no_retry() {
        let llm = ScriptedLlm::new(&[r#"{"answer": "42"}"#]);
        let v = llm.generate_json("q", None, &["answer"], 2).await.unwrap();
        assert_eq!(v["answer"], "42");
        assert_eq!(
            llm.prompts.lock().unwrap().len(),
            1,
            "should not retry on success"
        );
    }

    /// A claim extraction whose model repeats its answer unfenced must not
    /// burn a retry (or, after two, lose the claims).
    #[tokio::test]
    async fn a_repeated_answer_is_accepted_without_a_retry() {
        let one = r#"{"claims": [{"claim": "X happened", "asserted_by": "Reuters"}]}"#;
        let llm = ScriptedLlm::new(&[&format!("{one}\n{one}")]);
        let v = llm.generate_json("q", None, &["claims"], 2).await.unwrap();
        assert_eq!(v["claims"][0]["claim"], "X happened");
        assert_eq!(llm.prompts.lock().unwrap().len(), 1, "no retry");
    }

    #[tokio::test]
    async fn strips_markdown_fences_and_prose() {
        let llm = ScriptedLlm::new(&["Sure! Here you go:\n```json\n{\"answer\": \"ok\"}\n```"]);
        let v = llm.generate_json("q", None, &[], 2).await.unwrap();
        assert_eq!(v["answer"], "ok");
    }

    #[tokio::test]
    async fn self_corrects_invalid_then_valid() {
        let llm = ScriptedLlm::new(&["not json at all", r#"{"answer": "recovered"}"#]);
        let v = llm
            .generate_json("original prompt", None, &["answer"], 2)
            .await
            .unwrap();
        assert_eq!(v["answer"], "recovered");
        let prompts = llm.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2, "should re-prompt once");
        assert!(
            prompts[1].contains("not valid JSON"),
            "retry prompt names the error"
        );
        assert!(
            prompts[1].contains("original prompt"),
            "retry prompt keeps original task"
        );
    }

    #[tokio::test]
    async fn self_corrects_missing_required_key() {
        let llm = ScriptedLlm::new(&[r#"{"other": 1}"#, r#"{"needs_clarification": true}"#]);
        let v = llm
            .generate_json("q", None, &["needs_clarification"], 2)
            .await
            .unwrap();
        assert_eq!(v["needs_clarification"], true);
        assert!(llm.prompts.lock().unwrap()[1].contains("missing required field"));
    }

    #[tokio::test]
    async fn bare_array_passes_when_no_required_keys() {
        let llm = ScriptedLlm::new(&[r#"[{"name": "x"}]"#]);
        let v = llm.generate_json("q", None, &[], 1).await.unwrap();
        assert!(v.is_array());
    }

    #[tokio::test]
    async fn errors_after_exhausting_retries() {
        let llm = ScriptedLlm::new(&["nope", "still nope", "nope again"]);
        let err = llm
            .generate_json("q", None, &["answer"], 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("after 3 attempt"));
        // 1 initial + 2 retries = 3 calls, all consumed.
        assert_eq!(llm.prompts.lock().unwrap().len(), 3);
    }
}
