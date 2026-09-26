//! OpenAI-compatible front door: `GET /v1/models` + `POST /v1/chat/completions`.
//!
//! Lets any OpenAI-wire chat client (Conduit on a phone, LM Studio-style apps,
//! scripts using an OpenAI SDK) talk to the brain's own chat loop — the same
//! `ChatService::run` behind `/chat`, with its profiles, tools, delegation and
//! routing policy. It is an ADAPTER, not a model proxy: the "model" a client
//! picks is a context profile, and the brain decides which LLM actually runs.
//!
//! Mapping:
//! - `model: "brain"` → the deployment's front door (`CHAT_DEFAULT_PROFILE`);
//!   `model: "brain:<profile>"` → that context profile.
//! - The last `user` message is the turn; earlier user/assistant messages are
//!   `history`. Client `system` messages are dropped — the profile owns the
//!   system prompt, and a client's generic "You are a helpful assistant" would
//!   only fight it.
//! - Tool calls, tool results and thinking stream as `reasoning_content`, never
//!   `content`. The client sends assistant `content` back as history on the
//!   next turn, so progress narration in `content` would be re-read by the
//!   brain as something it said.
//! - A session id is derived from the conversation's first user message (the
//!   OpenAI wire has no conversation id), so each client conversation lands in
//!   one `conduit-…` working-memory session. `X-Session-Id` overrides it.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive},
    response::{IntoResponse, Response, Sse},
};
use futures_util::stream::StreamExt;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::HttpTransportState;
use crate::clients::{ChatEvent, ChatHistoryMessage, ChatRequest};

/// The model id that means "the front door".
const DEFAULT_MODEL_ID: &str = "brain";
/// Prefix for a model id naming a specific context profile.
const PROFILE_MODEL_PREFIX: &str = "brain:";
/// Session-id prefix for turns arriving through this adapter.
const SESSION_PREFIX: &str = "conduit-";
/// Cap on a tool call's rendered arguments in the reasoning stream.
const MAX_ARGS_PREVIEW: usize = 200;

/// `GET /v1/models` — the front door plus every context profile.
pub(super) async fn handle_list_models(State(state): State<Arc<HttpTransportState>>) -> Response {
    let created = chrono::Utc::now().timestamp();
    let mut data = vec![json!({
        "id": DEFAULT_MODEL_ID,
        "object": "model",
        "created": created,
        "owned_by": "agent-brain",
    })];

    if let Some(ref handle) = state.context_builder
        && let Some(ref cb) = *handle.read().await
    {
        let mut profiles = cb.list_profiles().await;
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        for p in profiles {
            data.push(json!({
                "id": format!("{PROFILE_MODEL_PREFIX}{}", p.name),
                "object": "model",
                "created": created,
                "owned_by": "agent-brain",
                "description": p.description,
            }));
        }
    }

    Json(json!({ "object": "list", "data": data })).into_response()
}

/// `POST /v1/chat/completions` — one brain chat turn, streamed or not.
pub(super) async fn handle_chat_completions(
    State(state): State<Arc<HttpTransportState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(svc) = state.chat_service.clone() else {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Chat service not available",
        );
    };

    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL_ID)
        .to_string();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let header_session = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let request = match build_chat_request(&messages, &model, header_session) {
        Ok(r) => r,
        Err(msg) => return openai_error(StatusCode::BAD_REQUEST, &msg),
    };
    info!(
        model = %model,
        profile = ?request.context_profile,
        session = ?request.session_id,
        history = request.history.len(),
        stream,
        "openai-compat chat turn"
    );

    let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
    tokio::spawn(async move {
        svc.run(request, tx).await;
    });

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if stream {
        let events = tokio_stream::wrappers::ReceiverStream::new(rx);
        let s = async_stream::stream! {
            let mut translator = Translator::default();
            yield Ok::<_, std::convert::Infallible>(sse_chunk(&id, created, &model, json!({"role": "assistant"}), None));
            futures_util::pin_mut!(events);
            while let Some(event) = events.next().await {
                let done = matches!(event, ChatEvent::Done);
                for delta in translator.translate(event) {
                    yield Ok(sse_chunk(&id, created, &model, delta.into_json(), None));
                }
                if done {
                    break;
                }
            }
            yield Ok(sse_chunk(&id, created, &model, json!({}), Some("stop")));
            yield Ok(Event::default().data("[DONE]"));
        };
        return Sse::new(s).keep_alive(KeepAlive::default()).into_response();
    }

    // Non-streaming: drain the turn, then answer once.
    let mut rx = rx;
    let mut translator = Translator::default();
    let mut content = String::new();
    let mut reasoning = String::new();
    while let Some(event) = rx.recv().await {
        let done = matches!(event, ChatEvent::Done);
        for delta in translator.translate(event) {
            match delta {
                Delta::Content(c) => content.push_str(&c),
                Delta::Reasoning(r) => reasoning.push_str(&r),
            }
        }
        if done {
            break;
        }
    }
    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    Json(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
    }))
    .into_response()
}

/// Translate OpenAI `messages` into a brain `ChatRequest`.
fn build_chat_request(
    messages: &[Value],
    model: &str,
    header_session: Option<String>,
) -> Result<ChatRequest, String> {
    let mut turns: Vec<ChatHistoryMessage> = messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(Value::as_str)?;
            if role != "user" && role != "assistant" {
                return None;
            }
            let text = message_text(m.get("content")?);
            let text = if role == "assistant" {
                strip_think(&text)
            } else {
                text
            };
            (!text.trim().is_empty()).then(|| ChatHistoryMessage {
                role: role.to_string(),
                content: text,
            })
        })
        .collect();

    let Some(last_user) = turns.iter().rposition(|t| t.role == "user") else {
        return Err("messages must contain at least one user message with text".into());
    };
    // Anything after the last user message (a prefilled assistant turn) is
    // not something the brain can continue; drop it.
    turns.truncate(last_user + 1);
    let message = turns.pop().map(|t| t.content).unwrap_or_default();

    let first_user = turns
        .iter()
        .find(|t| t.role == "user")
        .map(|t| t.content.as_str())
        .unwrap_or(&message);
    let session_id = header_session.unwrap_or_else(|| derive_session_id(first_user, model));

    Ok(ChatRequest {
        message,
        history: turns,
        session_id: Some(session_id),
        tools: None,
        context_profile: profile_for_model(model),
        synthesis_provider: None,
        synthesis_model: None,
    })
}

/// `"brain"` / unknown → `None` (front door); `"brain:<name>"` → that profile.
fn profile_for_model(model: &str) -> Option<String> {
    model
        .strip_prefix(PROFILE_MODEL_PREFIX)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// OpenAI `content` is a string or an array of parts; keep the text parts.
fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Remove `<think>…</think>` blocks a client may have folded into content.
fn strip_think(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Stable per-conversation session id: FNV-1a over model + first user message.
/// FNV rather than `DefaultHasher` because the std hasher's output is not
/// guaranteed stable across Rust releases, and a rebuild must not split a
/// conversation into two sessions.
fn derive_session_id(first_user: &str, model: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in model.bytes().chain([0u8]).chain(first_user.trim().bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{SESSION_PREFIX}{h:016x}")
}

enum Delta {
    Content(String),
    Reasoning(String),
}

impl Delta {
    fn into_json(self) -> Value {
        match self {
            Delta::Content(c) => json!({ "content": c }),
            Delta::Reasoning(r) => json!({ "reasoning_content": r }),
        }
    }
}

/// Turns `ChatEvent`s into OpenAI deltas.
///
/// The one subtlety: a streaming loop sends `Token`s AND then a final `Message`
/// carrying the whole answer. Forwarding both would print the answer twice, so
/// a `Message` only contributes what the tokens have not already sent.
#[derive(Default)]
struct Translator {
    sent: String,
}

impl Translator {
    fn translate(&mut self, event: ChatEvent) -> Vec<Delta> {
        match event {
            ChatEvent::Token { content } => {
                self.sent.push_str(&content);
                vec![Delta::Content(content)]
            }
            ChatEvent::Message { content } => {
                let extra = if self.sent.is_empty() {
                    content
                } else if let Some(rest) = content.strip_prefix(self.sent.as_str()) {
                    rest.to_string()
                } else if content.trim() == self.sent.trim() {
                    String::new()
                } else {
                    format!("\n\n{content}")
                };
                if extra.is_empty() {
                    return vec![];
                }
                self.sent.push_str(&extra);
                vec![Delta::Content(extra)]
            }
            ChatEvent::Thinking { content } => vec![Delta::Reasoning(format!("{content}\n"))],
            ChatEvent::ToolCall { tool, args } => {
                let mut a = args.to_string();
                if a.len() > MAX_ARGS_PREVIEW {
                    let cut = (0..=MAX_ARGS_PREVIEW)
                        .rev()
                        .find(|i| a.is_char_boundary(*i))
                        .unwrap_or(0);
                    a.truncate(cut);
                    a.push('…');
                }
                vec![Delta::Reasoning(format!("→ {tool}({a})\n"))]
            }
            ChatEvent::ToolResult {
                tool,
                success,
                preview,
                ..
            } => {
                let mark = if success { "✓" } else { "✗" };
                vec![Delta::Reasoning(format!(
                    "{mark} {tool} ({} chars)\n",
                    preview.chars().count()
                ))]
            }
            ChatEvent::Error { message } => {
                warn!(error = %message, "openai-compat turn reported an error");
                let text = if self.sent.is_empty() {
                    format!("⚠ {message}")
                } else {
                    format!("\n\n⚠ {message}")
                };
                self.sent.push_str(&text);
                vec![Delta::Content(text)]
            }
            ChatEvent::Done => vec![],
        }
    }
}

fn sse_chunk(id: &str, created: i64, model: &str, delta: Value, finish: Option<&str>) -> Event {
    let chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    Event::default().data(chunk.to_string())
}

fn openai_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message, "type": "invalid_request_error" } })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: Value) -> Value {
        json!({ "role": role, "content": content })
    }

    #[test]
    fn last_user_message_is_the_turn_and_system_is_dropped() {
        let messages = vec![
            msg("system", json!("You are a helpful assistant")),
            msg("user", json!("hello")),
            msg("assistant", json!("<think>hmm</think>Hi there")),
            msg(
                "user",
                json!([{ "type": "text", "text": "what did we learn?" }]),
            ),
        ];
        let r = build_chat_request(&messages, "brain", None).unwrap();
        assert_eq!(r.message, "what did we learn?");
        assert_eq!(r.history.len(), 2);
        assert_eq!(r.history[1].content, "Hi there");
        assert_eq!(r.context_profile, None);
        assert!(r.session_id.unwrap().starts_with(SESSION_PREFIX));
    }

    #[test]
    fn session_id_is_stable_across_turns_of_one_conversation() {
        let first = build_chat_request(&[msg("user", json!("hello"))], "brain", None).unwrap();
        let second = build_chat_request(
            &[
                msg("user", json!("hello")),
                msg("assistant", json!("hi")),
                msg("user", json!("more")),
            ],
            "brain",
            None,
        )
        .unwrap();
        assert_eq!(first.session_id, second.session_id);
    }

    #[test]
    fn header_session_wins() {
        let r =
            build_chat_request(&[msg("user", json!("x"))], "brain", Some("mine".into())).unwrap();
        assert_eq!(r.session_id.as_deref(), Some("mine"));
    }

    #[test]
    fn profile_model_ids_select_a_profile() {
        assert_eq!(profile_for_model("brain"), None);
        assert_eq!(profile_for_model("gpt-4o"), None);
        assert_eq!(
            profile_for_model("brain:knowledge-worker").as_deref(),
            Some("knowledge-worker")
        );
    }

    #[test]
    fn no_user_message_is_rejected() {
        assert!(build_chat_request(&[msg("system", json!("x"))], "brain", None).is_err());
    }

    #[test]
    fn final_message_after_tokens_is_not_sent_twice() {
        let mut t = Translator::default();
        let mut out = String::new();
        for e in [
            ChatEvent::Token {
                content: "PO".into(),
            },
            ChatEvent::Token {
                content: "NG".into(),
            },
            ChatEvent::Message {
                content: "PONG".into(),
            },
            ChatEvent::Done,
        ] {
            for d in t.translate(e) {
                if let Delta::Content(c) = d {
                    out.push_str(&c);
                }
            }
        }
        assert_eq!(out, "PONG");
    }

    #[test]
    fn tool_activity_goes_to_reasoning_not_content() {
        let mut t = Translator::default();
        let deltas = t.translate(ChatEvent::ToolCall {
            tool: "recall_recent".into(),
            args: json!({}),
        });
        assert!(matches!(deltas.as_slice(), [Delta::Reasoning(_)]));
    }
}
