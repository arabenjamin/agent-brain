//! `SseNotifier` trait — abstract SSE push-notification interface.
//!
//! The concrete `SessionManager` in `mcp/session.rs` implements this trait.
//! `QueueService` stores `Option<Arc<dyn SseNotifier>>` so it does not depend
//! on the concrete MCP session type.

use async_trait::async_trait;
use serde_json::Value;

/// Sends server-sent-event notifications to a connected HTTP client session.
#[async_trait]
pub trait SseNotifier: Send + Sync {
    /// Push a notification to the given session.
    ///
    /// - `session_id` — target session identifier
    /// - `event`      — SSE event name (e.g. `"agent_job"`)
    /// - `data`       — JSON payload
    async fn notify(&self, session_id: &str, event: &str, data: Value);
}
