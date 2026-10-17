//! Task-locals a running job sets so the LLM stack below it can route and
//! attribute its calls (the queue sets them at dispatch, `SharedLlm` reads them).

tokio::task_local! {
    /// Set to `true` inside a background job task when `provider_hint == "ollama"`.
    ///
    /// `SharedLlm` reads this flag to route generation calls to the local Ollama
    /// endpoint instead of the active (possibly cloud) model, preventing background
    /// maintenance jobs from consuming cloud quota.
    pub static USE_LOCAL_LLM: bool;

    /// Per-step model override resolved from `ChainStep.required_capabilities`
    /// by the model router (Phase 1 of the Agent Constructor plan).
    /// Takes precedence over `USE_LOCAL_LLM` in `SharedLlm` — a step that
    /// explicitly requires capabilities has earned its routing.
    pub static SELECTED_LLM: Option<crate::LlmConfig>;

    /// Name of the tool whose execution this task is running, so `SharedLlm`
    /// can attribute its usage rows to it.
    ///
    /// Without this every background LLM call lands in the ledger with
    /// `tool_name IS NULL`. That was 55% of 30 days of cloud spend — the
    /// majority of it unattributable by the one query you would ask after
    /// exhausting a quota, which is exactly when you need it. The tool name is
    /// known right here at dispatch; it just was not being carried down.
    pub static CURRENT_TOOL: Option<String>;

    /// The Task that owns the running job (`__owner_task_id`): for a
    /// schedule's run, its run-record task. Every note written under it is
    /// stamped `produced_by_task`, so what a run produced stays traceable
    /// after its completed jobs are pruned (about two days), including notes
    /// an agent writes inside a `run_agent` step.
    pub static CURRENT_OWNER_TASK: Option<String>;

    /// The agent profile whose loop this runs under (`AgentRunner` scopes it
    /// around a sub-agent run), so a tool call's telemetry (`arg_checks`)
    /// says which agent made it (PROGRAMS_PLAN Phase 4).
    pub static CURRENT_AGENT: Option<String>;
}

/// The agent whose loop this code runs under, if any.
pub fn current_agent() -> Option<String> {
    CURRENT_AGENT.try_with(|v| v.clone()).unwrap_or(None)
}

/// The owning task of the job this code runs under, if any.
pub fn current_owner_task() -> Option<String> {
    CURRENT_OWNER_TASK.try_with(|v| v.clone()).unwrap_or(None)
}
