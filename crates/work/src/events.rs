//! The internal event bus's events. The queue emits the job events; the
//! brain core owns the channel (`BrainCore::subscribe`).

/// Events emitted by the brain core and broadcast to all subscribers.
///
/// Transport adapters (HTTP, REST) subscribe via `BrainCore::subscribe` and
/// route events to their own notification channels (e.g. SSE push to client).
#[derive(Debug, Clone)]
pub enum BrainEvent {
    /// A background job completed successfully.
    JobCompleted {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        result_preview: Option<String>,
    },
    /// A background job failed but may be retried.
    JobFailed {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        error: String,
    },
    /// A background job exhausted all retries and is permanently dead.
    JobDead {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        error: String,
    },
    /// The brain created a proactive notification for the user.
    AgentChatInitiated {
        notification_id: String,
        message: String,
        /// Optional session the brain wants to continue (for context).
        related_session_id: Option<String>,
    },
}
