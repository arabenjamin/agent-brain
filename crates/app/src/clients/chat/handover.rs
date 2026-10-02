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

/// Whether a tool call has no effect outside the turn, so another model may
/// safely continue from it (and even repeat it). Conservative: an unknown
/// mutating shape counts as a write.
pub(super) fn call_is_read_only(tool: &str, args: &Value) -> bool {
    if WRITE_TOOLS.contains(&tool) || tool == "run_agent" || tool == "use_model" {
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
