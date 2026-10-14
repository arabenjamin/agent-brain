//! Delivery of a turn's events to the client, and reporting of dropped or abandoned turns.

use super::*;

/// Relay every event from a provider loop to the caller, capture the final
/// assistant text, and guarantee the turn ends with exactly one `Done`.
///
/// This owns the only detector for a **dropped turn** — a turn where the user's
/// message was banked, the provider loop returned, and nothing was ever written
/// back. Observed 2026-08-24: a user asked the brain to create a scheduled task,
/// the message was persisted to working memory at 18:49:30, and then nothing —
/// no reply, no tool call, no error event, and zero ERROR-level log lines all
/// day. The empty string was dropped by an `!is_empty()` guard on the persist
/// path that had no else branch, so the failure had no representation anywhere
/// and the user was left believing the work was underway.
///
/// `tx` is moved in here and dropped when this returns, so this is the last
/// point at which anything can still be said to the client — code after the
/// provider loops in `run()` cannot reach it.
/// Records a chat turn that ended without ever finishing — the one turn-ending
/// path `forward_chat_events`'s own tail cannot report.
///
/// If the SSE client goes away mid-turn, the handler future is dropped, the
/// forwarder's tail never runs, and the turn leaves NO trace: not the
/// dropped-turn warning, not an error event, nothing. On 2026-09-11 a chat turn
/// asked "summarize the YouTube videos you watched today", ran `search_notes`,
/// spent two gpt-oss calls (~31k prompt tokens), and then vanished — the only
/// evidence it had ever existed was an orphaned user row in working memory and
/// two ledger rows. Reading the logs afterwards, that is indistinguishable from
/// a request that never arrived, which is the worst possible state for a
/// subsystem whose failures are already hard to see.
///
/// `Drop` runs on the cancellation path, so the abandonment gets recorded.
pub(super) struct TurnAbandonGuard {
    pub(super) session_id: String,
    pub(super) user_snippet: String,
    pub(super) completed: bool,
}

impl TurnAbandonGuard {
    pub(super) fn new(session_id: Option<&str>, user_snippet: &str) -> Self {
        Self {
            session_id: session_id.unwrap_or("-").to_string(),
            user_snippet: user_snippet.to_string(),
            completed: false,
        }
    }

    /// The turn's event stream drained normally, so whatever happened is already
    /// represented (a message, an error, or the dropped-turn warning below).
    pub(super) fn mark_completed(&mut self) {
        self.completed = true;
    }

    pub(super) fn should_warn(&self) -> bool {
        !self.completed
    }
}

impl Drop for TurnAbandonGuard {
    fn drop(&mut self) {
        if self.should_warn() {
            warn!(
                session_id = %self.session_id,
                user_message = %self.user_snippet,
                "Chat turn abandoned before producing a reply — the client disconnected or the \
                 task was cancelled mid-turn. Any model calls already made are still billed, and \
                 nothing was saved for this turn."
            );
        }
    }
}

/// The tool calls a turn made, as the forwarder saw them stream past, for the
/// turn's `turn:` telemetry row (route mining, GRAPH_CONTEXT_PLAN Phase B).
pub(super) type TurnTrace = Arc<std::sync::Mutex<Vec<Value>>>;

/// One call, small: the tool and the arguments that say what it was for.
fn trace_call(tool: &str, args: &Value) -> Value {
    let mut brief = serde_json::Map::new();
    if let Some(o) = args.as_object() {
        for (k, v) in o {
            if matches!(
                k.as_str(),
                "agent"
                    | "view"
                    | "topic"
                    | "window"
                    | "query"
                    | "entity_name"
                    | "action"
                    | "kind"
                    | "api"
                    | "name"
            ) {
                let v = match v.as_str() {
                    Some(s) => Value::String(s.chars().take(120).collect()),
                    None => v.clone(),
                };
                brief.insert(k.clone(), v);
            }
        }
    }
    serde_json::json!({"tool": tool, "args": brief})
}

pub(super) async fn forward_chat_events(
    mut inner_rx: mpsc::Receiver<ChatEvent>,
    tx: mpsc::Sender<ChatEvent>,
    result_tx: mpsc::Sender<String>,
    session_id: Option<String>,
    user_snippet: String,
    citations: Option<Arc<dyn CitationChecker>>,
    trace: Option<TurnTrace>,
) {
    // Armed for the whole turn; disarmed once the stream drains normally. See
    // TurnAbandonGuard — a cancelled turn runs none of the code below.
    let mut abandon_guard = TurnAbandonGuard::new(session_id.as_deref(), &user_snippet);
    let mut final_text = String::new();
    let mut saw_error = false;
    while let Some(mut event) = inner_rx.recv().await {
        if let ChatEvent::Message { content } = event {
            event = ChatEvent::Message {
                content: check_citations(content, citations.as_deref()).await,
            };
        }
        if let (ChatEvent::ToolCall { tool, args }, Some(t)) = (&event, &trace)
            && let Ok(mut calls) = t.lock()
        {
            calls.push(trace_call(tool, args));
        }
        match &event {
            ChatEvent::Message { content } => final_text = content.clone(),
            ChatEvent::Error { .. } => saw_error = true,
            // Held back and re-emitted below. The client closes its reader on
            // `done`, so an error reported after it would never be displayed —
            // the turn has to be marked failed *before* it is marked finished.
            ChatEvent::Done => continue,
            _ => {}
        }
        let _ = tx.send(event).await;
    }

    // The stream drained, so the turn reached an end it can describe itself.
    abandon_guard.mark_completed();

    // `saw_error` keeps this from double-reporting a turn that already failed
    // loudly (e.g. no provider configured): that turn has an explanation, and a
    // second, vaguer error would only obscure it.
    if final_text.trim().is_empty() && !saw_error {
        warn!(
            session_id = session_id.as_deref().unwrap_or("-"),
            user_message = %user_snippet,
            "Chat turn produced no assistant message and no error — dropped turn"
        );
        let _ = tx
            .send(ChatEvent::Error {
                message: "The assistant produced no response for this turn, and reported no \
                          error explaining why. Nothing was saved. Please retry — and do not \
                          assume any action you asked for was carried out."
                    .into(),
            })
            .await;
    }

    // Always terminate the stream, including when a provider loop returned
    // without sending one.
    let _ = tx.send(ChatEvent::Done).await;
    let _ = result_tx.send(final_text).await;
}
