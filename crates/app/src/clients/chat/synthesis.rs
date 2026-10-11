//! The worker/voice split: composing the final reply from gathered results.

use super::*;

/// The voice model could not compose the reply. The worker's own answer is
/// still a real answer, so send that with a visible note rather than ending the
/// turn on an error: on 2026-09-26 a `gemini-3.6-flash` 503 in the voice pass
/// discarded a finished 1,312-token worker answer and the user got nothing.
/// Only when the worker produced no prose does the failure surface as an error,
/// which lets the ladder hand the gathered tool results to the next model.
pub(super) async fn voice_fallback(
    tx: &mpsc::Sender<ChatEvent>,
    voice_model: &str,
    reason: &str,
    worker_answer: &str,
) {
    let reason: String = reason.chars().take(200).collect();
    warn!(voice_model, reason = %reason, has_worker_answer = !worker_answer.trim().is_empty(), "Synthesis failed");
    if worker_answer.trim().is_empty() {
        let _ = tx
            .send(ChatEvent::Error {
                message: format!("Synthesis failed ({voice_model}): {reason}"),
            })
            .await;
        return;
    }
    let _ = tx
        .send(ChatEvent::Thinking {
            content: format!(
                "⚠ voice model {voice_model} failed ({reason}) — showing the worker's own answer"
            ),
        })
        .await;
    let _ = tx
        .send(ChatEvent::Message {
            content: worker_answer.to_string(),
        })
        .await;
}

impl ChatService {
    /// Compose the turn's reply from what the worker gathered.
    ///
    /// `gathered` is passed explicitly rather than scraped back out of the
    /// message history because `run_openai_loop` trims that history to
    /// `MAX_HISTORY_MESSAGES` to keep the context from overflowing. Reading
    /// tool results from it would silently lose the earliest ones on exactly
    /// the long research turns synthesis exists to serve.
    pub(super) async fn run_synthesis(
        &self,
        request: &ChatRequest,
        gathered: &[String],
        weak_answer: &str,
        tool_trace: &[(String, bool)],
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let provider_str = match &request.synthesis_provider {
            Some(p) => p.to_lowercase(),
            None => return,
        };

        // Read the current live config so we can fall back to its API key if the
        // env var isn't set (e.g. key was supplied via use_model, not via env).
        let live_config = self.llm_config.read().await.clone();

        let (provider, default_model, api_key, base_url) = match provider_str.as_str() {
            "gemini" => {
                let key = std::env::var("GEMINI_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        live_config
                            .as_ref()
                            .filter(|c| c.provider == LlmProviderType::Gemini)
                            .and_then(|c| c.api_key.clone())
                            .filter(|k| !k.is_empty())
                    });
                (
                    LlmProviderType::Gemini,
                    std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".into()),
                    key,
                    None,
                )
            }
            "anthropic" | "claude" => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        live_config
                            .as_ref()
                            .filter(|c| c.provider == LlmProviderType::Anthropic)
                            .and_then(|c| c.api_key.clone())
                            .filter(|k| !k.is_empty())
                    });
                (
                    LlmProviderType::Anthropic,
                    "claude-haiku-4-5-20251001".to_string(),
                    key,
                    None,
                )
            }
            // Ollama and Ollama Cloud are the $0 rungs, and the reason this
            // arm exists: the split is only useful if the *voice* model can be
            // one we already run. `config_for_catalog_entry` is the same
            // primitive capability routing and the fallback ladder use to turn
            // a catalog name into a callable config — endpoint and credentials
            // come from the environment, never from a checked-in file.
            "ollama" | "ollama-cloud" | "ollamacloud" => {
                let model = request
                    .synthesis_model
                    .clone()
                    .or_else(|| std::env::var("CHAT_SYNTHESIS_MODEL").ok())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "gemma4:31b-cloud".to_string());
                let cfg = crate::services::model_router::config_for_catalog_entry(
                    &LlmConfig::default(),
                    &provider_str,
                    &model,
                );
                (
                    cfg.provider,
                    model,
                    cfg.api_key.clone(),
                    cfg.base_url.clone(),
                )
            }
            other => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!(
                            "Unknown synthesis provider: {other}. \
                             Use 'ollama', 'ollama-cloud', 'gemini', or 'anthropic'."
                        ),
                    })
                    .await;
                return;
            }
        };

        let model = request.synthesis_model.clone().unwrap_or(default_model);

        let provider_label = provider.to_string();

        let synth_config = LlmConfig {
            provider,
            model: model.clone(),
            api_key,
            base_url,
            temperature: 0.7,
            timeout: Duration::from_secs(120),
            ..LlmConfig::default()
        };

        // The voice model reads everything the turn gathered, so it answers to
        // the same routing policy as the turn.
        if let Err(reason) = RoutingPolicy::from_env().admit(&synth_config, RouteContext::current())
        {
            voice_fallback(
                &tx,
                &model,
                &format!("refused by routing policy: {reason}"),
                weak_answer,
            )
            .await;
            return;
        }

        let llm = match LlmClient::with_config(synth_config) {
            Ok(c) => c,
            Err(e) => {
                voice_fallback(
                    &tx,
                    &model,
                    &format!("could not initialise: {e}"),
                    weak_answer,
                )
                .await;
                return;
            }
        };

        let tool_results: Vec<&str> = gathered
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let research_block = if tool_results.is_empty() {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "⚠ Research phase called 0 tools — the local model did not invoke any \
                     tool. Synthesis will proceed with the model's direct answer only \
                     (weak_answer len={}).",
                        weak_answer.len()
                    ),
                })
                .await;
            "(no tool results gathered)".to_string()
        } else {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "Synthesizing {} tool result(s) with {model}…",
                        tool_results.len()
                    ),
                })
                .await;
            tool_results.join("\n\n---\n\n")
        };

        let synthesis_prompt = format!(
            "You are a research synthesizer. An AI research agent gathered the following \
             information using tools to answer a question. \
             Your job is to synthesize the gathered material into a comprehensive, \
             well-structured, and clearly written response.\n\n\
             {receipt}\n\n\
             Original question: {question}\n\n\
             Research gathered:\n{research}\n\n\
             {analysis}\
             Synthesize the above into a clear, informative, and complete response to the \
             original question. Report actions in the past tense ONLY when they appear in the \
             ground-truth tool list above; for anything the agent should do but the tools did \
             not do this turn, say what still needs to happen — do not narrate it as done.",
            receipt = tool_trace_receipt(tool_trace),
            question = request.message,
            research = research_block,
            analysis = if !weak_answer.is_empty() {
                format!(
                    "Initial analysis from research agent (UNVERIFIED — it may describe actions \
                     that never ran; keep only what the ground-truth tool list supports):\n\
                     {weak_answer}\n\n"
                )
            } else {
                String::new()
            },
        );

        // (synthesis-start thinking event already emitted in the research_block block above)

        let messages = vec![ChatMessage::user(&synthesis_prompt)];
        let call_start = std::time::Instant::now();
        let chat_result = llm.chat(&messages).await;
        self.record_llm_call(
            &provider_label,
            &model,
            chat_result.is_ok(),
            call_start.elapsed().as_millis() as i64,
            chat_result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_in)
                .map(i64::from),
            chat_result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_out)
                .map(i64::from),
            chat_result.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
        match chat_result {
            Ok(response) if !response.text.is_empty() => {
                let _ = tx
                    .send(ChatEvent::Message {
                        content: response.text,
                    })
                    .await;
            }
            Ok(_) => voice_fallback(&tx, &model, "returned an empty reply", weak_answer).await,
            Err(e) => voice_fallback(&tx, &model, &e.to_string(), weak_answer).await,
        }
    }

    // ========================================================================
    // Text-based loop (Gemini fallback)
    // ========================================================================
}
