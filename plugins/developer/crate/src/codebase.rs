//! Codebase Skill — read-only access to the agent's own local source code.
//!
//! Tools: read_codebase_file, search_codebase, get_git_log, analyze_own_structure,
//! write_codebase_doc, write_proposal (a Feed item, no file), and the workspace
//! pair when WORKSPACE_DIR is set.
//!
//! GitHub API access is intentionally NOT a native tool — use the generic
//! `http_request` tool with `context_name="github"`. The `github` ApiContext
//! is seeded at boot with base_url, auth header, and `GITHUB_TOKEN` auto-injection.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{info, warn};

use agent_brain_plugin_api::graph::query;
use agent_brain_plugin_api::protocol::Content;
use agent_brain_plugin_api::{
    Feed, FeedItem, Graph, Introspection, Knowledge, PluginContext, Provenance, Skill,
    ToolCallResult, ToolDefinition, parse_args,
};

/// Guard for `write_codebase_doc` overwrites of existing docs.
///
/// LLM-driven whole-file regeneration has previously replaced grounded docs with
/// generic hallucinated content, and each scheduled cycle then fed the
/// hallucination back in as ground truth (a one-way ratchet). This validates that
/// a rewrite is an *incremental update* of the existing document rather than a
/// from-scratch regeneration:
///
/// 1. **Heading retention** — ≥ 75% of the old markdown headings must still be
///    present (compared case-insensitively on letters only, so counts/dates in
///    headings may legitimately change).
/// 2. **Line retention** — ≥ 50% of the old non-empty lines must appear verbatim
///    in the new content.
/// 3. **Shrink cap** — the new content must be at least 40% of the old length.
///
/// Errors describe exactly which check failed so a calling LLM can correct
/// course (or a human can pass `force: true` for an intentional full rewrite).
fn validate_doc_rewrite(old: &str, new: &str) -> Result<(), String> {
    // Letters-only normalisation: stable against count/date churn in headings.
    fn normalize_heading(line: &str) -> String {
        line.chars()
            .filter(|c| c.is_alphabetic() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    let old_headings: Vec<String> = old
        .lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .map(normalize_heading)
        .filter(|h| !h.is_empty())
        .collect();
    if !old_headings.is_empty() {
        let new_headings: std::collections::HashSet<String> = new
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(normalize_heading)
            .collect();
        let kept = old_headings
            .iter()
            .filter(|h| new_headings.contains(*h))
            .count();
        let ratio = kept as f64 / old_headings.len() as f64;
        if ratio < 0.75 {
            let missing: Vec<&str> = old_headings
                .iter()
                .filter(|h| !new_headings.contains(*h))
                .map(|s| s.as_str())
                .take(5)
                .collect();
            return Err(format!(
                "rewrite drops {}/{} existing headings (e.g. {:?}). Updates must preserve \
                 the document structure — edit sections in place instead of regenerating \
                 the file from scratch.",
                old_headings.len() - kept,
                old_headings.len(),
                missing
            ));
        }
    }

    let old_lines: Vec<&str> = old
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !old_lines.is_empty() {
        let new_lines: std::collections::HashSet<&str> = new
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let kept = old_lines.iter().filter(|l| new_lines.contains(*l)).count();
        let ratio = kept as f64 / old_lines.len() as f64;
        if ratio < 0.5 {
            return Err(format!(
                "rewrite keeps only {:.0}% of the existing content lines (minimum 50%). \
                 This looks like a from-scratch regeneration, not an update — make \
                 targeted edits grounded in the existing document.",
                ratio * 100.0
            ));
        }
    }

    if new.len() < old.len() * 2 / 5 {
        return Err(format!(
            "new content is {} bytes vs {} existing ({}% — minimum 40%). Updates must \
             not discard most of the document.",
            new.len(),
            old.len(),
            new.len() * 100 / old.len().max(1)
        ));
    }

    Ok(())
}

/// Replace (or append) one markdown section of a document, leaving everything
/// else byte-for-byte untouched.
///
/// `section_heading` must be a full heading line (e.g. `## Recent Changes (auto)`).
/// If the heading exists, everything from it up to the next heading of the same
/// or higher level is replaced with `heading + content`. If it does not exist,
/// the section is appended at the end of the document.
///
/// This is the safe write path for scheduled LLM-generated doc updates: the
/// model's output is confined to its own section and can never destroy the
/// rest of the document, however bad it is.
fn upsert_doc_section(old: &str, section_heading: &str, content: &str) -> String {
    let heading = section_heading.trim();
    let level = heading.chars().take_while(|c| *c == '#').count().max(1);
    let block = format!("{}\n\n{}", heading, content.trim());

    let lines: Vec<&str> = old.lines().collect();
    let start = lines.iter().position(|l| l.trim() == heading);

    let new_text = match start {
        Some(i) => {
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim_start();
                if t.starts_with('#') && t.chars().take_while(|c| *c == '#').count() <= level {
                    break;
                }
                j += 1;
            }
            let mut parts: Vec<String> = lines[..i].iter().map(|s| s.to_string()).collect();
            parts.push(block);
            if j < lines.len() {
                // Blank line between the new section and the next heading.
                parts.push(String::new());
                parts.extend(lines[j..].iter().map(|s| s.to_string()));
            }
            parts.join("\n")
        }
        None => {
            let trimmed = old.trim_end();
            if trimmed.is_empty() {
                block
            } else {
                format!("{}\n\n{}", trimmed, block)
            }
        }
    };

    // Documents end with exactly one newline.
    format!("{}\n", new_text.trim_end())
}

/// Format `git log` output (lines of `hash|date|subject`) as a dated markdown
/// changelog digest, grouped by commit date:
///
/// ```markdown
/// ### 2026-06-12
///
/// - redesign doc-update chain around section-confined writes (`6ddd9ad`)
/// ```
///
/// Deterministic and 100% commit-grounded — used by the doc-update schedule so
/// no LLM sits between git history and the published changelog.
fn format_git_digest(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut current_date = "";
    for line in raw.lines() {
        let mut parts = line.splitn(3, '|');
        let (Some(hash), Some(date), Some(subject)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if date != current_date {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(format!("### {}", date));
            out.push(String::new());
            current_date = date;
        }
        out.push(format!("- {} (`{}`)", subject.trim(), hash));
    }
    out.join("\n")
}

/// Codebase Skill — read-only filesystem access to the agent's own source code,
/// plus workspace write tools and a write_proposal tool for staging fix proposals.
pub struct CodebaseSkill {
    /// Root directory of the codebase (from CODEBASE_DIR or auto-detected).
    codebase_dir: Option<PathBuf>,
    /// Writable workspace directory (from WORKSPACE_DIR) — separate from the read-only codebase.
    workspace_dir: Option<PathBuf>,
    /// Optional knowledge store for analyze_own_structure(store_as_note=true).
    knowledge: Option<Arc<dyn Knowledge>>,
    /// The graph, for the self-analysis inventory and the github ApiContext.
    neo4j: Option<Arc<dyn Graph>>,
    /// The owner's Feed: a proposal is an item there.
    feed: Option<Arc<dyn Feed>>,
    /// The host's view of the core (declared tools).
    intro: Option<Arc<dyn Introspection>>,
}

impl CodebaseSkill {
    pub fn new(ctx: &PluginContext) -> Self {
        let intro = ctx.introspection.clone();
        let codebase_dir = intro.as_ref().and_then(|i| i.codebase_dir());
        let workspace_dir = intro.as_ref().and_then(|i| i.workspace_dir());
        if let Some(ref dir) = codebase_dir {
            info!(path = %dir.display(), "CodebaseSkill initialized with codebase root");
        } else {
            warn!(
                "CodebaseSkill: no CODEBASE_DIR configured — filesystem tools will return errors"
            );
        }
        if let Some(ref dir) = workspace_dir {
            info!(path = %dir.display(), "CodebaseSkill: workspace directory configured");
        }
        Self {
            codebase_dir,
            workspace_dir,
            knowledge: ctx.knowledge.clone(),
            neo4j: ctx.graph.clone(),
            feed: ctx.feed.clone(),
            intro,
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn read_codebase_file_def() -> ToolDefinition {
        ToolDefinition {
            name: "read_codebase_file".to_string(),
            description: "Read a file from the agent's own codebase by path. Path is relative to the codebase root. Returns a WINDOW of lines, not necessarily the whole file: use start_line (1-indexed) with max_lines to page through a large file — the result states which lines you got and the exact start_line to continue from.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to codebase root (e.g. 'src/main.rs' or 'Cargo.toml')"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "1-indexed line to start reading from (default: 1). Use the value the previous result's continuation marker names to page through a file larger than max_lines."
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default: 500)"
                    },
                    "prepend_context": {
                        "type": "string",
                        "description": "Optional text to prepend to the output before the file content — useful in chains where {{_prev}} carries prior step output that should be passed forward alongside the file."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn write_codebase_doc_def() -> ToolDefinition {
        ToolDefinition {
            name: "write_codebase_doc".to_string(),
            description: "Write a Markdown (.md) file in the codebase. Restricted to .md files only — cannot modify source code. PREFERRED for automated updates: pass `section` (a heading line like '## Recent Changes') to replace or append just that section, leaving the rest of the document untouched. Whole-file overwrites of existing docs are guarded: the new content must preserve the document's headings and at least half of its existing lines (incremental update, not from-scratch regeneration); force=true bypasses for an intentional full rewrite.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to codebase root (e.g. 'project-docs/env.md'). Must end in .md."
                    },
                    "content": {
                        "type": "string",
                        "description": "File content, or the section body when `section` is set (max 8000 bytes in section mode)."
                    },
                    "section": {
                        "type": "string",
                        "description": "Optional markdown heading line (e.g. '## Recent Changes (auto)'). Replaces that section in place, or appends it if absent. The rest of the document is never touched."
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Whole-file mode only: bypass the incremental-update guard for an intentional full rewrite (default: false)."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn search_codebase_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_codebase".to_string(),
            description: "Search the codebase for a regex pattern, like grep. LITERAL match over file contents — NOT semantic search: pass an identifier, symbol, or exact phrase (`max_attempts`, `fn retry`), never a question or description, which will match nothing. Returns matching lines with file and line number context.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Regex or literal string to search for"
                    },
                    "file_pattern": {
                        "type": "string",
                        "description": "Filename suffix filter (e.g. '.rs', '.yaml')"
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Lines of context before/after each match (default: 0, max: 5)"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Case-sensitive search (default: false)"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum matches to return (default: 50)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn get_git_log_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_git_log".to_string(),
            description: "Get recent git commit history for the codebase. Pass format='digest' for a dated markdown changelog (### date headings + commit-subject bullets), ready to write into a doc section.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "description": "Number of commits to retrieve (default: 10, max: 50)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Limit to commits affecting this path (relative to codebase root)"
                    },
                    "format": {
                        "type": "string",
                        "description": "Optional output format: 'digest' = dated markdown changelog grouped by commit date (deterministic, commit-grounded)."
                    }
                }
            }),
        }
    }

    fn write_proposal_def() -> ToolDefinition {
        ToolDefinition {
            name: "write_proposal".to_string(),
            description: "Put a structured fix proposal in the user's Feed for review. Use this after diagnosing a bug or improvement — it never touches the source code; the user applies or dismisses it from the Feed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short human-readable title for the proposal"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "ID of the Task node that triggered this diagnosis"
                    },
                    "diagnosis": {
                        "type": "string",
                        "description": "Root cause analysis — what is broken and why"
                    },
                    "affected_file": {
                        "type": "string",
                        "description": "Relative path to the affected source file (or 'unknown')"
                    },
                    "proposed_fix": {
                        "type": "string",
                        "description": "Plain-English description of the fix"
                    },
                    "code_snippet": {
                        "type": "string",
                        "description": "Optional diff or replacement code snippet"
                    },
                    "severity": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Estimated impact severity"
                    }
                },
                "required": ["title", "task_id", "diagnosis", "proposed_fix", "severity"]
            }),
        }
    }

    fn write_workspace_file_def() -> ToolDefinition {
        ToolDefinition {
            name: "write_workspace_file".to_string(),
            description: "Write a file to the agent's writable workspace directory. Use this for generated code, scripts, experiments, or any output that should persist outside the read-only codebase. Path is relative to the workspace root.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root (e.g. 'scripts/fetch.py' or 'experiments/test.rs')"
                    },
                    "content": {
                        "type": "string",
                        "description": "File content to write"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "description": "Write mode: 'overwrite' (default) replaces the file, 'append' adds to end"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn list_workspace_files_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_workspace_files".to_string(),
            description: "List files in the agent's writable workspace directory.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "directory": {
                        "type": "string",
                        "description": "Subdirectory to list (relative to workspace root, default: root)"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Filename suffix or substring to filter (e.g. '.py', '.rs')"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum files to return (default: 100)"
                    }
                }
            }),
        }
    }

    fn analyze_own_structure_def() -> ToolDefinition {
        ToolDefinition {
            name: "analyze_own_structure".to_string(),
            description: "Generate a structured overview of the agent's own CODEBASE: a \
                          capability tally, directory tree, workspace layout, module list, and \
                          recent git history. Use it to orient in the source. It is long and \
                          gets truncated, so for the brain's INVENTORY — how many tools it has, \
                          what they are called, which agents exist, what a profile may use — \
                          call `list_capabilities` instead: that is the deterministic, \
                          drillable answer and this is a code overview that happens to lead \
                          with the counts. If store_as_note=true, persists the result to the \
                          knowledge graph as a semantic note."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "store_as_note": {
                        "type": "boolean",
                        "description": "Whether to store the analysis as a semantic note in the knowledge graph (default: false)"
                    }
                }
            }),
        }
    }

    // =========================================================================
    // Security helpers
    // =========================================================================

    /// Resolve a user-supplied relative path against codebase_dir, ensuring it
    /// stays within the root (no `../` traversal). Returns an absolute PathBuf.
    fn safe_path(&self, relative: &str) -> Result<PathBuf, ToolCallResult> {
        let root = match &self.codebase_dir {
            Some(d) => d,
            None => {
                return Err(ToolCallResult::error(
                    "CODEBASE_DIR not configured — set CODEBASE_DIR env var",
                ));
            }
        };

        let canonical_root = root
            .canonicalize()
            .map_err(|e| ToolCallResult::error(format!("Codebase dir not accessible: {e}")))?;

        // Build the target and normalize without requiring it to exist.
        let raw = canonical_root.join(relative.trim_start_matches('/'));
        let normalized = normalize_path(&raw);

        // Re-canonicalize if the path exists (resolves symlinks).
        let canonical_target = if normalized.exists() {
            normalized
                .canonicalize()
                .map_err(|e| ToolCallResult::error(format!("Path error: {e}")))?
        } else {
            normalized
        };

        if !canonical_target.starts_with(&canonical_root) {
            return Err(ToolCallResult::error(format!(
                "Path '{}' is outside the codebase root",
                relative
            )));
        }

        Ok(canonical_target)
    }

    fn root(&self) -> Result<PathBuf, ToolCallResult> {
        match &self.codebase_dir {
            Some(d) => d
                .canonicalize()
                .map_err(|e| ToolCallResult::error(format!("Codebase dir not accessible: {e}"))),
            None => Err(ToolCallResult::error("CODEBASE_DIR not configured")),
        }
    }

    // =========================================================================
    // Filesystem handlers
    // =========================================================================

    async fn handle_read_codebase_file(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            max_lines: Option<usize>,
            start_line: Option<usize>,
            prepend_context: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let max_lines = args.max_lines.unwrap_or(500).min(2000);
        // 1-indexed to match the `path:line` citations every other tool emits
        // (search_codebase results, compiler errors, the agent's own report).
        // Without an offset the window could only ever start at line 1, so a
        // 2790-line file like services/queue.rs had everything past line 2000
        // permanently unreachable — an agent asked about logic at line ~1572 hit
        // the cap and correctly reported it "not visible in the files examined".
        let start_line = args.start_line.unwrap_or(1).max(1);
        let start_idx = start_line - 1;
        let full_path = match self.safe_path(&args.path) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(format!("Cannot read '{}': {e}", args.path)),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if start_idx >= total && total > 0 {
            return ToolCallResult::success_text(format!(
                "// File: {}\n\n[start_line {} is past the end of the file ({} lines). \
                 Re-read with a start_line between 1 and {}.]",
                args.path, start_line, total, total
            ));
        }
        let end_idx = start_idx.saturating_add(max_lines).min(total);
        let truncated = end_idx < total;
        let shown = lines[start_idx.min(total)..end_idx].join("\n");

        let mut out = String::new();
        if let Some(ctx) = args.prepend_context
            && !ctx.trim().is_empty()
        {
            out.push_str(&ctx);
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!(
            "// File: {} (lines {}-{} of {})\n{}",
            args.path, start_line, end_idx, total, shown
        ));
        if truncated {
            // Name the exact next call. "use max_lines to read more" was a dead
            // end past the 2000 cap; a concrete start_line is not.
            out.push_str(&format!(
                "\n\n[... {} more line(s) not shown (total: {}). Continue with start_line: {} (max_lines caps at 2000 per call) ...]",
                total - end_idx,
                total,
                end_idx + 1
            ));
        }
        ToolCallResult::success_text(out)
    }

    async fn handle_write_codebase_doc(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
            #[serde(default)]
            section: Option<String>,
            #[serde(default)]
            force: bool,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        if !args.path.ends_with(".md") {
            return ToolCallResult::error(
                "write_codebase_doc only allows .md files — cannot modify source code",
            );
        }

        let full_path = match self.safe_path(&args.path) {
            Ok(p) => p,
            Err(e) => return e,
        };

        // Section mode: confine the write to one markdown section. The rest of
        // the document is preserved by construction, so the rewrite guard does
        // not apply — but the section content itself is size-capped.
        let final_content = if let Some(ref section) = args.section {
            if !section.trim_start().starts_with('#') {
                return ToolCallResult::error(
                    "`section` must be a markdown heading line (e.g. '## Recent Changes')",
                );
            }
            if args.content.len() > 8_000 {
                return ToolCallResult::error(format!(
                    "Section content is {} bytes (max 8000). Section updates are \
                     bounded digests, not whole documents.",
                    args.content.len()
                ));
            }
            let old = tokio::fs::read_to_string(&full_path)
                .await
                .unwrap_or_default();
            upsert_doc_section(&old, section, &args.content)
        } else {
            // Whole-file mode: guard overwrites of existing docs against
            // from-scratch LLM regeneration. Trivially small existing files
            // (< 200 bytes) are not worth guarding.
            if !args.force
                && let Ok(old) = tokio::fs::read_to_string(&full_path).await
                && old.len() >= 200
                && let Err(reason) = validate_doc_rewrite(&old, &args.content)
            {
                warn!(path = %args.path, %reason, "write_codebase_doc: rejected by doc guard");
                return ToolCallResult::error(format!(
                    "Refusing to overwrite '{}': {} (Pass force=true to override for an \
                     intentional full rewrite.)",
                    args.path, reason
                ));
            }
            args.content.clone()
        };

        if let Some(parent) = full_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolCallResult::error(format!("Cannot create parent directory: {e}"));
        }

        if let Err(e) = tokio::fs::write(&full_path, &final_content).await {
            return ToolCallResult::error(format!("Failed to write '{}': {e}", args.path));
        }

        let lines = final_content.lines().count();
        info!(path = %args.path, lines, section = ?args.section, "write_codebase_doc: wrote file");
        ToolCallResult::success_text(format!(
            "Wrote {} ({} lines{}): {}",
            args.path,
            lines,
            args.section
                .as_deref()
                .map(|s| format!(", section '{}'", s))
                .unwrap_or_default(),
            full_path.display()
        ))
    }

    async fn handle_search_codebase(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            file_pattern: Option<String>,
            context_lines: Option<usize>,
            case_sensitive: Option<bool>,
            max_results: Option<usize>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let root = match self.root() {
            Ok(r) => r,
            Err(e) => return e,
        };
        let max = args.max_results.unwrap_or(50).min(200);
        let ctx = args.context_lines.unwrap_or(0).min(5);
        let case_sensitive = args.case_sensitive.unwrap_or(false);

        let re = match if case_sensitive {
            regex::Regex::new(&args.query)
        } else {
            regex::Regex::new(&format!("(?i){}", args.query))
        } {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Invalid regex: {e}")),
        };

        let mut results: Vec<String> = Vec::new();
        search_in_dir(
            &root,
            &root,
            &re,
            &args.file_pattern,
            ctx,
            &mut results,
            max,
        );

        if results.is_empty() {
            // A zero result must carry the way out of it. `search_codebase` is a
            // literal/regex grep, so a natural-language question matches nothing
            // and reads exactly like "this code does not exist" — on 2026-09-11 a
            // delegated code-analyst asked a prose question about the queue's
            // retry handling, got this empty result, and correctly-but-wrongly
            // reported the logic absent — on a codebase where the relevant
            // identifier has 50 hits. The
            // model was not at fault; the empty answer named nothing it could try
            // next. So when a multi-word query misses, re-probe each significant
            // term and report which ones DO match (same doctrine as
            // `recall_recent`'s unfiltered re-run and `list_endpoints` pointing at
            // `list_apis`).
            const PROBE_CAP: usize = 20;
            const NOISE: &[&str] = &[
                "the", "and", "for", "with", "from", "that", "this", "how", "does", "what", "why",
                "who", "are", "was", "its", "our", "all", "any", "can", "does", "did", "you",
                "use", "get", "into", "about", "when", "where", "which",
            ];
            let terms: Vec<&str> = args
                .query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|t| t.len() >= 3 && !NOISE.contains(&t.to_ascii_lowercase().as_str()))
                .take(8)
                .collect();

            let mut hits: Vec<String> = Vec::new();
            if terms.len() > 1 {
                for term in &terms {
                    let Ok(tre) = regex::Regex::new(&format!("(?i){}", regex::escape(term))) else {
                        continue;
                    };
                    let mut sub: Vec<String> = Vec::new();
                    search_in_dir(
                        &root,
                        &root,
                        &tre,
                        &args.file_pattern,
                        0,
                        &mut sub,
                        PROBE_CAP,
                    );
                    if !sub.is_empty() {
                        // The probe is capped, so an exact count would be a lie at
                        // the ceiling; mark it as a floor instead.
                        let plus = if sub.len() >= PROBE_CAP { "+" } else { "" };
                        hits.push(format!("  '{}' - {}{} match(es)", term, sub.len(), plus));
                    }
                }
            }

            let guidance = if hits.is_empty() {
                concat!(
                    "This tool is a LITERAL regex/grep over file contents - it is NOT ",
                    "semantic search, so a question or description will never match. Search ",
                    "for an identifier, symbol, or exact phrase that appears verbatim in the ",
                    "source (e.g. `max_attempts`, `fn retry`, `impl QueueService`). To find ",
                    "code by concept, use `analyze_own_structure` (it includes the tree) to ",
                    "locate the file, then `read_codebase_file`."
                )
                .to_string()
            } else {
                format!(
                    concat!(
                        "This tool is a LITERAL regex/grep, not semantic search - the full ",
                        "phrase matched nothing, but these terms from it DO occur:\n{}\n\n",
                        "Re-run with one of those, or with a symbol you expect verbatim in ",
                        "the source. Absence of the phrase is NOT absence of the code."
                    ),
                    hits.join("\n")
                )
            };

            ToolCallResult::success_text(format!(
                "No matches found for '{}'.\n\n{}",
                args.query, guidance
            ))
        } else {
            ToolCallResult::success_text(format!(
                "{} match(es) for '{}':\n\n{}",
                results.len(),
                args.query,
                results.join("\n---\n")
            ))
        }
    }

    async fn handle_get_git_log(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            n: Option<u32>,
            path: Option<String>,
            #[serde(default)]
            format: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let root = match self.root() {
            Ok(r) => r,
            Err(e) => return e,
        };
        let n = args.n.unwrap_or(10).min(50);
        let digest = args.format.as_deref() == Some("digest");

        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&root);
        cmd.arg("log");
        cmd.arg(format!("-{n}"));
        if digest {
            cmd.arg("--format=%h|%ad|%s");
        } else {
            cmd.arg("--format=%h %ad %an: %s");
        }
        cmd.arg("--date=short");
        if let Some(ref p) = args.path {
            cmd.arg("--").arg(p);
        }

        match cmd.output().await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                if text.trim().is_empty() {
                    return ToolCallResult::success_text("No commits found".to_string());
                }
                ToolCallResult::success_text(if digest {
                    format_git_digest(text.trim())
                } else {
                    format!("Recent commits:\n{}", text.trim())
                })
            }
            Ok(out) => ToolCallResult::error(String::from_utf8_lossy(&out.stderr).to_string()),
            Err(e) => ToolCallResult::error(format!("git command failed: {e}")),
        }
    }

    // =========================================================================
    // Proposal reader / manager
    // =========================================================================

    // =========================================================================
    // Proposal writer
    // =========================================================================

    async fn handle_write_proposal(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            title: String,
            task_id: String,
            diagnosis: String,
            affected_file: Option<String>,
            proposed_fix: String,
            code_snippet: Option<String>,
            severity: String,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let Some(feed) = &self.feed else {
            return ToolCallResult::error("write_proposal needs Neo4j: a proposal is a Feed item");
        };
        let affected = args.affected_file.as_deref().unwrap_or("unknown");
        let mut body = format!(
            "- **Task ID:** `{task_id}`\n\
             - **Severity:** {severity}\n\
             - **Affected file:** `{affected}`\n\n\
             ## Diagnosis\n\n{diagnosis}\n\n\
             ## Proposed Fix\n\n{proposed_fix}\n",
            task_id = args.task_id,
            severity = args.severity,
            diagnosis = args.diagnosis,
            proposed_fix = args.proposed_fix,
        );
        if let Some(ref snippet) = args.code_snippet {
            body.push_str(&format!("\n## Code\n\n```\n{}\n```\n", snippet));
        }
        // Reviewing a proposal is work for the user: it lives in their Feed,
        // whole — there is no proposal file (CORE_AND_PLUGINS_PLAN.md §4).
        match proposal_item(
            feed.as_ref(),
            &args.task_id,
            &args.title,
            &args.severity,
            body,
        )
        .await
        {
            Ok(id) => {
                info!(item = %id, severity = %args.severity, "write_proposal: item created");
                ToolCallResult::success_text(format!(
                    "Proposal is in the user's Feed as item {id}. Human review required before applying."
                ))
            }
            Err(e) => ToolCallResult::error(format!("Failed to create the proposal item: {e}")),
        }
    }

    // =========================================================================
    // Workspace write helpers
    // =========================================================================

    fn workspace_root(&self) -> Result<PathBuf, ToolCallResult> {
        match &self.workspace_dir {
            Some(d) => Ok(d.clone()),
            None => Err(ToolCallResult::error(
                "WORKSPACE_DIR not configured — set WORKSPACE_DIR env var to enable workspace tools",
            )),
        }
    }

    fn safe_workspace_path(&self, relative: &str) -> Result<PathBuf, ToolCallResult> {
        let root = self.workspace_root()?;
        let raw = root.join(relative.trim_start_matches('/'));
        let normalized = normalize_path(&raw);
        // Enforce no traversal outside workspace root.
        if !normalized.starts_with(&root) {
            return Err(ToolCallResult::error(format!(
                "Path '{}' is outside the workspace root",
                relative
            )));
        }
        Ok(normalized)
    }

    async fn handle_write_workspace_file(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
            mode: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let full_path = match self.safe_workspace_path(&args.path) {
            Ok(p) => p,
            Err(e) => return e,
        };

        if let Some(parent) = full_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolCallResult::error(format!("Cannot create directory: {e}"));
        }

        let append = args.mode.as_deref() == Some("append");
        let result = if append {
            use tokio::io::AsyncWriteExt;
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&full_path)
                .await
            {
                Ok(f) => f,
                Err(e) => return ToolCallResult::error(format!("Cannot open file: {e}")),
            };
            file.write_all(args.content.as_bytes()).await
        } else {
            tokio::fs::write(&full_path, &args.content).await
        };

        match result {
            Ok(()) => {
                info!(path = %full_path.display(), "write_workspace_file: wrote file");
                ToolCallResult::success_text(format!(
                    "Written: {}\nAbsolute path: {}",
                    args.path,
                    full_path.display()
                ))
            }
            Err(e) => ToolCallResult::error(format!("Failed to write '{}': {e}", args.path)),
        }
    }

    async fn handle_list_workspace_files(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            directory: Option<String>,
            pattern: Option<String>,
            max_results: Option<usize>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let max = args.max_results.unwrap_or(100).min(500);
        let root = match self.workspace_root() {
            Ok(p) => p,
            Err(e) => return e,
        };
        let start_dir = match &args.directory {
            Some(d) => match self.safe_workspace_path(d) {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => root.clone(),
        };

        if !start_dir.exists() {
            return ToolCallResult::success_text(format!(
                "Workspace at '{}' is empty or does not exist yet.",
                root.display()
            ));
        }

        let mut files: Vec<String> = Vec::new();
        collect_files(&root, &start_dir, &args.pattern, &mut files, max);
        files.sort();

        if files.is_empty() {
            ToolCallResult::success_text(format!("Workspace at '{}' is empty.", root.display()))
        } else {
            ToolCallResult::success_text(format!(
                "Workspace ({}): {} file(s):\n{}",
                root.display(),
                files.len(),
                files.join("\n")
            ))
        }
    }

    // =========================================================================
    // Self-analysis handler
    // =========================================================================

    async fn handle_analyze_own_structure(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Args {
            store_as_note: Option<bool>,
        }
        let args: Args = parse_args(arguments).unwrap_or_default();
        let store = args.store_as_note.unwrap_or(false);

        let mut sections: Vec<String> = Vec::new();

        // Section 0: the actual capability inventory, FIRST.
        //
        // This tool's name promises self-knowledge, so it is what a model
        // reaches for when asked "how many tools do you have" — and until now it
        // answered with a directory tree, 40 lines of Cargo.toml, and the 25
        // `pub mod` lines of skills/mod.rs, which name modules, not tools, and
        // carry no count at all. Worse, MAX_TOOL_RESULT_CHARS (6000) clipped the
        // response long before even that module list. Observed 2026-09-11: a
        // `toolsmith` sub-agent called this twice, correctly, and reported "10
        // distinct tools" against a real registry of 90.
        //
        // The counts come from the meta-graph, re-synced from the live tool
        // registry at the end of every build_skills(), and lead the output
        // because a truncated result must not be able to drop them — the same
        // position-zero rule as the truncation and search-source markers.
        if let Some(neo4j) = &self.neo4j {
            let mut lines: Vec<String> = Vec::new();

            if let Ok(rows) = neo4j
                .execute(query(
                    "MATCH (d:ToolDef) RETURN count(d) AS tools, count(DISTINCT d.skill) AS skills",
                ))
                .await
                && let Some(row) = rows.first()
            {
                let tools = row.get::<i64>("tools").unwrap_or(0);
                let skills = row.get::<i64>("skills").unwrap_or(0);
                lines.push(format!(
                    "**{tools} tools registered across {skills} skills.** This is the count — \
                     do not infer a different one from the module list or directory tree below."
                ));
            }

            if let Ok(rows) = neo4j
                .execute(query(
                    "MATCH (d:ToolDef) RETURN coalesce(d.skill, '(none)') AS skill, \
                     count(d) AS c ORDER BY c DESC, skill",
                ))
                .await
                && !rows.is_empty()
            {
                lines.push("\n### Tools per skill".into());
                for row in &rows {
                    lines.push(format!(
                        "- {}: {}",
                        row.get::<String>("skill").unwrap_or_default(),
                        row.get::<i64>("c").unwrap_or(0)
                    ));
                }
            }

            if let Ok(rows) = neo4j
                .execute(query(
                    "MATCH (c:ContextProfile) RETURN count(c) AS profiles, \
                     count(CASE WHEN c.spawnable THEN 1 END) AS agents",
                ))
                .await
                && let Some(row) = rows.first()
            {
                lines.push(format!(
                    "\n**{} context profiles, of which {} are spawnable agents.**",
                    row.get::<i64>("profiles").unwrap_or(0),
                    row.get::<i64>("agents").unwrap_or(0)
                ));
            }

            if let Ok(rows) = neo4j
                .execute(query(
                    "MATCH (c:ContextProfile) WHERE c.spawnable \
                     RETURN c.name AS name, coalesce(c.specialty, c.description) AS specialty \
                     ORDER BY c.name",
                ))
                .await
                && !rows.is_empty()
            {
                lines.push("\n### Spawnable agents (the delegate/run_agent roster)".into());
                for row in &rows {
                    lines.push(format!(
                        "- {}: {}",
                        row.get::<String>("name").unwrap_or_default(),
                        row.get::<String>("specialty").unwrap_or_default()
                    ));
                }
            }

            if !lines.is_empty() {
                lines.push(
                    "\nFor tool NAMES, one skill's tools, or one profile's allowlist, call \
                     `list_capabilities` — this section is a tally, not the rows."
                        .into(),
                );
                sections.push(format!("## Capability Inventory\n{}", lines.join("\n")));
            }
        }

        // Section 1: Directory tree (depth 3)
        let tree = match self.root() {
            Ok(root) => {
                let mut out = format!(
                    "{}/\n",
                    root.file_name().and_then(|n| n.to_str()).unwrap_or(".")
                );
                build_tree(&root, "", 3, 0, &mut out);
                out
            }
            Err(e) => extract_text(&e),
        };
        sections.push(format!("## Directory Structure\n```\n{tree}\n```"));

        // Section 2: Workspace Cargo.toml
        if let Ok(p) = self.safe_path("Cargo.toml")
            && let Ok(content) = tokio::fs::read_to_string(&p).await
        {
            let preview = content.lines().take(40).collect::<Vec<_>>().join("\n");
            sections.push(format!(
                "## Cargo.toml (workspace root)\n```toml\n{}\n```",
                preview
            ));
        }

        // Section 3: skills/mod.rs (skill registry)
        if let Ok(p) = self.safe_path("crates/app/src/skills/mod.rs")
            && let Ok(content) = tokio::fs::read_to_string(&p).await
        {
            sections.push(format!(
                "## Skill Registry (skills/mod.rs)\n```rust\n{}\n```",
                content.trim()
            ));
        }

        // Section 4: Recent git log
        let log = self.handle_get_git_log(Some(json!({"n": 10}))).await;
        sections.push(format!(
            "## Recent Git History\n```\n{}\n```",
            extract_text(&log)
        ));

        // Section 5: declarative capabilities (workflows, declared tools)
        if let Some(neo4j) = &self.neo4j {
            let mut dyn_lines: Vec<String> = Vec::new();

            // Workflow nodes — the brain's configured job chains
            let workflows = neo4j
                .execute(query(
                    "MATCH (w:Workflow) RETURN w.name AS name, w.interval_seconds AS every, \
                     coalesce(w.description, '') AS description ORDER BY w.name",
                ))
                .await
                .unwrap_or_default();
            if !workflows.is_empty() {
                dyn_lines.push("### Workflows (graph-stored job chains)".into());
                for w in &workflows {
                    let when = w
                        .get::<i64>("every")
                        .map(|s| format!("every {}", format_every(s)))
                        .unwrap_or_else(|_| "by name".into());
                    let name = w.get::<String>("name").unwrap_or_default();
                    let desc = w.get::<String>("description").unwrap_or_default();
                    dyn_lines.push(format!("- {name} ({when}): {desc}"));
                }
            }

            // Declared tools — a bundle's tools/*.yaml (skills/bundle_tools.rs)
            let declared: Vec<(String, String)> = self
                .intro
                .as_ref()
                .map(|i| i.declared_tools())
                .unwrap_or_default();
            if !declared.is_empty() {
                dyn_lines
                    .push("\n### Declared tools (bundle tools/*.yaml, read-only Cypher)".into());
                for (name, plugin) in &declared {
                    dyn_lines.push(format!("- {name} (plugin {plugin})"));
                }
            }

            if !dyn_lines.is_empty() {
                sections.push(format!(
                    "## Graph-Stored Capabilities\n\n{}\n\n> NOTE: These capabilities already exist in the knowledge graph. \
                     Do NOT recommend creating them when analysing gaps.",
                    dyn_lines.join("\n")
                ));
            }
        }

        let content = format!(
            "# Agent Brain — Codebase Self-Analysis\n\nGenerated: {}\n\n{}",
            chrono::Utc::now().to_rfc3339(),
            sections.join("\n\n")
        );

        info!(
            chars = content.len(),
            store_as_note = store,
            "analyze_own_structure complete"
        );

        if store && let Some(knowledge) = &self.knowledge {
            match knowledge
                .store_note_as(
                    &content,
                    "semantic",
                    Some("codebase_self_analysis"),
                    Provenance::SynthesisInference,
                )
                .await
            {
                Ok(id) => {
                    info!(note_id = %id, "Stored codebase self-analysis note");
                }
                Err(e) => {
                    warn!(error = %e, "Failed to store self-analysis note (non-fatal)");
                }
            }
        }

        ToolCallResult::success_text(content)
    }
}

// =========================================================================
// Skill trait implementation
// =========================================================================

#[async_trait]
impl Skill for CodebaseSkill {
    fn name(&self) -> &str {
        "Codebase Inspector"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![
            Self::read_codebase_file_def(),
            Self::search_codebase_def(),
            Self::get_git_log_def(),
            Self::write_proposal_def(),
            Self::write_codebase_doc_def(),
            Self::analyze_own_structure_def(),
        ];
        if self.workspace_dir.is_some() {
            tools.push(Self::write_workspace_file_def());
            tools.push(Self::list_workspace_files_def());
        }
        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "read_codebase_file" => Some(self.handle_read_codebase_file(arguments).await),
            "search_codebase" => Some(self.handle_search_codebase(arguments).await),
            "get_git_log" => Some(self.handle_get_git_log(arguments).await),
            "write_proposal" => Some(self.handle_write_proposal(arguments).await),
            "write_codebase_doc" => Some(self.handle_write_codebase_doc(arguments).await),
            "write_workspace_file" => Some(self.handle_write_workspace_file(arguments).await),
            "list_workspace_files" => Some(self.handle_list_workspace_files(arguments).await),
            "analyze_own_structure" => Some(self.handle_analyze_own_structure(arguments).await),
            _ => None,
        }
    }
}

// =========================================================================
// Helper functions
// =========================================================================

/// Extract text content from a ToolCallResult (used internally for composing analyze_own_structure).
fn extract_text(result: &ToolCallResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize a path without canonicalizing (for paths that may not exist yet).
fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

const SKIP_DIRS: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    ".cargo",
    "dist",
    "build",
    "__pycache__",
];

/// Recursively collect files matching an optional suffix/substring filter.
fn collect_files(
    root: &Path,
    dir: &Path,
    pattern: &Option<String>,
    results: &mut Vec<String>,
    max: usize,
) {
    if results.len() >= max {
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if results.len() >= max {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            collect_files(root, &path, pattern, results, max);
        } else if path.is_file() {
            if let Some(pat) = pattern
                && !name_str.ends_with(pat.as_str())
                && !name_str.contains(pat.as_str())
            {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                results.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

/// Search files for a regex pattern, collecting formatted match strings.
fn search_in_dir(
    root: &Path,
    dir: &Path,
    re: &regex::Regex,
    file_pattern: &Option<String>,
    context_lines: usize,
    results: &mut Vec<String>,
    max: usize,
) {
    if results.len() >= max {
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if results.len() >= max {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            search_in_dir(root, &path, re, file_pattern, context_lines, results, max);
        } else if path.is_file() {
            // Skip binary-looking file types.
            if matches!(
                name_str.split('.').next_back().unwrap_or(""),
                "db" | "gz" | "png" | "jpg" | "gif" | "ico" | "woff" | "ttf" | "bin" | "lock"
            ) {
                continue;
            }
            if let Some(pat) = file_pattern
                && !name_str.ends_with(pat.as_str())
                && !name_str.contains(pat.as_str())
            {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            let lines: Vec<&str> = content.lines().collect();

            for (i, line) in lines.iter().enumerate() {
                if results.len() >= max {
                    break;
                }
                if re.is_match(line) {
                    let start = i.saturating_sub(context_lines);
                    let end = (i + context_lines + 1).min(lines.len());
                    let snippet = lines[start..end]
                        .iter()
                        .enumerate()
                        .map(|(j, l)| format!("{:>4}: {}", start + j + 1, l))
                        .collect::<Vec<_>>()
                        .join("\n");
                    results.push(format!("{}:{}\n{}", rel, i + 1, snippet));
                }
            }
        }
    }
}

fn build_tree(dir: &Path, prefix: &str, max_depth: usize, depth: usize, out: &mut String) {
    if depth >= max_depth {
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(e) => e
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                !SKIP_DIRS.contains(&name.to_string_lossy().as_ref())
            })
            .collect(),
        Err(_) => return,
    };
    // Directories first, then alphabetical.
    entries.sort_by_key(|e| (!e.path().is_dir(), e.file_name()));

    let count = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == count - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };
        let name = entry.file_name();
        let is_dir = entry.path().is_dir();
        let display = if is_dir {
            format!("{}/", name.to_string_lossy())
        } else {
            name.to_string_lossy().to_string()
        };
        out.push_str(&format!("{}{}{}\n", prefix, connector, display));
        if is_dir {
            build_tree(
                &entry.path(),
                &format!("{}{}", prefix, child_prefix),
                max_depth,
                depth + 1,
                out,
            );
        }
    }
}

/// A schedule period as written in workflow files (`1w`, `6h`, `30m`).
fn format_every(secs: i64) -> String {
    for (unit, size) in [("w", 604_800), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs % size == 0 {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// The item that asks the user to review a proposal: the proposal itself.
async fn proposal_item(
    feed: &dyn Feed,
    task_id: &str,
    title: &str,
    severity: &str,
    body: String,
) -> anyhow::Result<String> {
    let priority = match severity.to_lowercase().as_str() {
        "critical" | "high" => 1,
        "low" => 3,
        _ => 2,
    };
    feed.create_item(FeedItem {
        title: format!("Review proposal: {title}"),
        body,
        priority,
        origin_kind: "proposal".into(),
        origin_ref: task_id.to_string(),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{format_git_digest, upsert_doc_section, validate_doc_rewrite};

    #[test]
    fn git_digest_groups_by_date() {
        let raw = "abc1234|2026-06-12|redesign doc-update chain\n\
def5678|2026-06-12|guard write_codebase_doc\n\
0123abc|2026-06-11|graph hygiene sweep";
        let out = format_git_digest(raw);
        assert_eq!(
            out,
            "### 2026-06-12\n\n\
- redesign doc-update chain (`abc1234`)\n\
- guard write_codebase_doc (`def5678`)\n\n\
### 2026-06-11\n\n\
- graph hygiene sweep (`0123abc`)"
        );
    }

    #[test]
    fn git_digest_skips_malformed_lines() {
        let out = format_git_digest("not a digest line\nabc1234|2026-06-12|real commit");
        assert_eq!(out, "### 2026-06-12\n\n- real commit (`abc1234`)");
    }

    const DOC: &str = "\
# Brain Status

**Build:** passing
**Tool count:** 64 static registered across 16 skills

## Architecture Overview

| Layer | Technology | Status |
|-------|-----------|--------|
| Protocol | MCP via stdio + HTTP/SSE | Live |
| Graph DB | Neo4j via `neo4rs` | Live |

## Skill Registry (64 tools static + N runtime)

| Skill | Tools |
|-------|-------|
| KnowledgeSkill | 7 |
| TaskSkill | 7 |

## Known Issues / Backlog

- SSE push for job results on stdio transport
- Rhai scripting in procedure steps
";

    #[test]
    fn incremental_update_passes() {
        // Change a count in a heading + a table cell, add a new backlog line.
        let updated = DOC
            .replace(
                "(64 tools static + N runtime)",
                "(65 tools static + N runtime)",
            )
            .replace("| KnowledgeSkill | 7 |", "| KnowledgeSkill | 8 |")
            + "- New backlog item from recent commits\n";
        assert!(validate_doc_rewrite(DOC, &updated).is_ok());
    }

    #[test]
    fn from_scratch_regeneration_rejected() {
        let hallucinated = "\
# Project Status Report

## Key Objectives Status

| Objective | Status |
|-----------|--------|
| Authentication & Authorization | On Track |
| Data Pipeline Reliability | At Risk |

## Issues & Risks Log

Rate limiting on the external data source is causing batch failures.
";
        let err = validate_doc_rewrite(DOC, hallucinated).unwrap_err();
        assert!(err.contains("headings"), "unexpected error: {err}");
    }

    #[test]
    fn heavy_shrink_rejected() {
        let stub = "# Brain Status\n\n## Architecture Overview\n\n## Skill Registry\n\n## Known Issues / Backlog\n";
        assert!(validate_doc_rewrite(DOC, stub).is_err());
    }

    #[test]
    fn identical_content_passes() {
        assert!(validate_doc_rewrite(DOC, DOC).is_ok());
    }

    #[test]
    fn new_file_has_no_old_content_to_check() {
        // Guard is only invoked for existing files, but empty old must not panic.
        assert!(validate_doc_rewrite("", "# Anything\nnew content\n").is_ok());
    }

    const SECTIONED: &str = "\
# Brain Status

Intro line.

## Architecture Overview

Arch content stays.

## Recent Changes (auto)

### 2026-06-10

- old digest entry

## Known Issues / Backlog

- backlog item
";

    #[test]
    fn section_replace_preserves_rest() {
        let out = upsert_doc_section(
            SECTIONED,
            "## Recent Changes (auto)",
            "### 2026-06-12\n\n- new digest entry",
        );
        assert!(out.contains("### 2026-06-12"), "new content missing: {out}");
        assert!(
            !out.contains("2026-06-10"),
            "old section not replaced: {out}"
        );
        assert!(out.contains("Arch content stays."));
        assert!(out.contains("## Known Issues / Backlog"));
        assert!(out.contains("- backlog item"));
        assert!(out.contains("Intro line."));
    }

    #[test]
    fn section_appended_when_missing() {
        let out = upsert_doc_section(
            "# Doc\n\nBody text.\n",
            "## Recent Changes (auto)",
            "- first entry",
        );
        assert!(
            out.starts_with("# Doc\n\nBody text.\n\n## Recent Changes (auto)\n\n- first entry")
        );
        assert!(out.ends_with("\n"));
    }

    #[test]
    fn section_into_empty_file() {
        let out = upsert_doc_section("", "## Recent Changes (auto)", "- entry");
        assert_eq!(out, "## Recent Changes (auto)\n\n- entry\n");
    }

    #[test]
    fn section_at_end_of_file_replaced() {
        let doc = "# Doc\n\n## Recent Changes (auto)\n\n- old entry\n";
        let out = upsert_doc_section(doc, "## Recent Changes (auto)", "- new entry");
        assert!(out.contains("- new entry"));
        assert!(!out.contains("- old entry"));
        assert!(out.starts_with("# Doc\n"));
    }

    #[test]
    fn section_subheadings_belong_to_section() {
        // The ### sub-heading inside the section must be replaced along with it.
        let out = upsert_doc_section(SECTIONED, "## Recent Changes (auto)", "- flat entry");
        assert!(!out.contains("### 2026-06-10"));
        assert!(out.contains("## Known Issues / Backlog"));
    }
}
