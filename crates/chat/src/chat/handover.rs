//! Handing a failed attempt's read-only work over to the next ladder rung.

use super::*;

/// One tool call a failed attempt made, carried to the next ladder rung.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PriorCall {
    pub(super) tool: String,
    pub(super) args: Value,
    pub(super) success: Option<bool>,
    /// What the failed model read back, capped at [`HANDOVER_RESULT_CHARS`].
    pub(super) result: String,
}

/// Per-call cap on a handed-over tool result, and the cap on the whole block.
pub(super) const HANDOVER_RESULT_CHARS: usize = 4000;

pub(super) const HANDOVER_TOTAL_CHARS: usize = 16000;

/// Tools a probe runs for real: each only reads (the graph, the telemetry, the
/// web, the codebase) or runs code in the no-network sandbox. Everything else
/// is stubbed, so a tool added later is stubbed until it is listed here: a
/// probe that shows a stub where a read was is a worse probe, never a write.
const PROBE_READ_TOOLS: &[&str] = &[
    "search_notes",
    "recall_recent",
    "search_web",
    "get_search_usage",
    "read_page",
    "neo4j_query",
    "describe_graph",
    "recent_tasks",
    "list_apis",
    "list_endpoints",
    "list_capabilities",
    "duckdb_query",
    "get_llm_budget",
    "mcp_list_tools",
    "list_tasks",
    "select_agent",
    "lookup",
    "wiki_background",
    "read_codebase_file",
    "search_codebase",
    "get_git_log",
    "analyze_own_structure",
    "list_workspace_files",
    "execute_code",
    "area_traffic",
    "geocode",
];

/// Tools with an `action` whose reading actions a probe may run.
const PROBE_READ_ACTIONS: &[(&str, &[&str])] = &[
    ("manage_workflow", &["list", "get"]),
    ("manage_program", &["list", "get", "review_context"]),
    ("manage_todo", &["list", "get"]),
    ("manage_plugin", &["list", "get"]),
    ("manage_media_source", &["list"]),
    ("dev_docs", &["search", "get"]),
    ("dead_letter", &["list"]),
];

/// Whether a probe run may execute this call (HARNESS2_PLAN Phase 1). Reads
/// run, so the probe sees what the agent would see; a call that could act
/// outside the run is stubbed. Stricter than [`call_is_read_only`], which
/// trusts any name that does not look like a write: `reason` stores
/// inferences, `dev_docs` writes notes, `deep_research` is paid.
pub(super) fn probe_may_run(tool: &str, args: &Value) -> bool {
    if matches!(tool, "http_request" | "mcp_call") {
        return call_is_read_only(tool, args);
    }
    if PROBE_READ_TOOLS.contains(&tool) {
        return true;
    }
    let action = args["action"].as_str().unwrap_or("");
    PROBE_READ_ACTIONS
        .iter()
        .any(|(t, actions)| *t == tool && actions.contains(&action))
}

/// Whether a tool call has no effect outside the turn, so another model may
/// safely continue from it (and even repeat it). Conservative: an unknown
/// mutating shape counts as a write.
pub(super) fn call_is_read_only(tool: &str, args: &Value) -> bool {
    if WRITE_TOOLS.contains(&tool) || tool == "run_agent" {
        return false;
    }
    const MUTATING_PREFIXES: [&str; 12] = [
        "manage_",
        "ingest_",
        "store_",
        "create_",
        "update_",
        "delete_",
        "write_",
        "push_",
        "set_",
        "register_",
        "define_",
        "scheduler_",
    ];
    if MUTATING_PREFIXES.iter().any(|p| tool.starts_with(p)) {
        return false;
    }
    match tool {
        "http_request" => {
            let method = args["method"].as_str().unwrap_or("GET");
            method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
        }
        "mcp_call" => {
            // The remote tool is opaque; only names that read are trusted.
            let inner = args["tool"].as_str().unwrap_or("").to_ascii_lowercase();
            [
                "search", "fetch", "get", "list", "read", "extract", "map", "crawl",
            ]
            .iter()
            .any(|verb| inner.contains(verb))
        }
        // Chat sub-agents run with allow_writes: false.
        _ => true,
    }
}

/// The system-prompt block that hands a failed model's work to the next rung.
pub(super) fn handover_block(calls: &[PriorCall], error: &str) -> String {
    let error: String = error.chars().take(200).collect();
    let mut out = format!(
        "WORK ALREADY DONE THIS TURN. Another model started answering this message, ran \
         the tools below, and then failed ({error}). These results are real and current. \
         Do NOT call these tools again with the same arguments — continue from them, call \
         anything still missing, and answer the user.\n"
    );
    let mut used = out.len();
    for (i, c) in calls.iter().enumerate() {
        let mark = match c.success {
            Some(true) => "✓",
            Some(false) => "✗ failed",
            None => "no result",
        };
        let entry = format!(
            "\n{}. {}({}) {mark}\n{}\n",
            i + 1,
            c.tool,
            serde_json::to_string(&c.args).unwrap_or_default(),
            c.result
        );
        if used + entry.len() > HANDOVER_TOTAL_CHARS {
            out.push_str(&format!(
                "\n[{} further tool result(s) omitted for length]\n",
                calls.len() - i
            ));
            break;
        }
        used += entry.len();
        out.push_str(&entry);
    }
    out
}
