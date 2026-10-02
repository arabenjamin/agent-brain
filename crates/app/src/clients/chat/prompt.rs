//! System prompt assembly.

pub(super) const DEFAULT_SYSTEM_PROMPT_TEMPLATE: &str = "\
You are agent-brain, an autonomous AI assistant backed by a persistent Neo4j \
knowledge graph. Always think step-by-step before acting and use the available \
tools to give the most accurate, grounded answer possible.\n\
TIME: right now it is {NOW}. That is local wall-clock time, and it is read \
fresh at the start of every turn — treat it as authoritative over anything you \
remember about the date. Resolve \"today\", \"tonight\", \"this week\", and \
\"recent\" against it. When searching for recent content, include the current \
date in your queries (e.g. \"daily news brief {DATE}\"). Timestamps stored in \
the graph are UTC, which is {UTC_NOW} right now — convert before quoting one to \
a person, and note that a UTC timestamp can carry a different calendar date \
than the local one above.\n\
{PATHS_SECTION}\
CRITICAL — interactive chat rules:\n\
1. Always deliver the actual result. Never describe what you are about to do \
   or what you have queued — the user is waiting for the answer RIGHT NOW.\n\
2. Never use enqueue_jobs or manage_scheduled_task in chat. Background jobs run \
   asynchronously and their output will NEVER appear here. Do the work inline: \
   use search_web to fetch data, reason to synthesize it, store_note to save it, \
   then present the result to the user directly.\n\
3. If asked for a news brief that is not in the graph, search_web for current \
   headlines, synthesize a brief with reason, store_note it, then show it here.\n\
Key tools: `search_web` (fetch current info), `search_notes` (knowledge graph), \
`store_note` (save), `reason` (synthesize), `create_task` / `list_tasks` (tasks). \
Only call tools that exist — do not invent tool names. \
Never output XML tags like <invoke> — use only the provided function-call tools.";

/// Compose the effective system prompt for one chat turn.
///
/// Layered so the most specific guidance sits closest to the conversation:
/// the shared base rules, then the active context profile's `system_prompt`,
/// then whatever its `pre_load_query` returned. The profile layers are dropped
/// when no profile applies (explicit tool allowlists, or no context builder).
pub(crate) fn build_system_prompt(profile_prompt: Option<&str>, pre_loaded: &[String]) -> String {
    let mut prompt = build_base_system_prompt();

    if let Some(p) = profile_prompt.map(str::trim).filter(|p| !p.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(p);
    }

    if !pre_loaded.is_empty() {
        prompt.push_str(
            "\n\n# LIVE SELF-STATE — read from your own graph just now. \
             This is authoritative: prefer it over any recollection, and do not \
             contradict it.\n\n",
        );
        prompt.push_str(&pre_loaded.join("\n\n"));
    }

    prompt
}

/// Compose the shared base prompt for one turn.
///
/// Called per turn (from `run`), not cached, so the clock in the prompt stays
/// correct across a long session and across a local midnight.
pub(super) fn build_base_system_prompt() -> String {
    let date = crate::services::clock::today();
    let now = crate::services::clock::now_stamp();
    let utc_now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let codebase_dir = std::env::var("CODEBASE_DIR").unwrap_or_default();
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_default();
    let paths_section = match (codebase_dir.is_empty(), workspace_dir.is_empty()) {
        (false, false) => format!(
            "Your codebase root is `{codebase_dir}` (read-only via codebase tools). \
             Your writable workspace is `{workspace_dir}` — use write_workspace_file to create files there.\n"
        ),
        (false, true) => {
            format!("Your codebase root is `{codebase_dir}` (read-only via codebase tools).\n")
        }
        (true, false) => format!(
            "Your writable workspace is `{workspace_dir}` — use write_workspace_file to create files there.\n"
        ),
        (true, true) => String::new(),
    };
    DEFAULT_SYSTEM_PROMPT_TEMPLATE
        .replace("{DATE}", &date)
        .replace("{NOW}", &now)
        .replace("{UTC_NOW}", &utc_now)
        .replace("{PATHS_SECTION}", &paths_section)
}
