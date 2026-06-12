//! Codebase Skill — read-only access to the agent's own local source code.
//!
//! Provides 7 tools:
//! - **Filesystem (6)**: read_codebase_file, list_codebase_files, search_codebase,
//!   get_file_tree, get_git_log, get_git_diff
//! - **Self-analysis (1)**: analyze_own_structure
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

use crate::repository::Neo4jClient;
use crate::services::KnowledgeStore;
use crate::skills::Skill;
use agent_brain_models::ProvenanceFlag;
use agent_brain_protocol::{Content, ToolCallResult, ToolDefinition, parse_args};

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
    /// Directory where fix proposals are written (from PROPOSALS_DIR, default ./proposals).
    proposals_dir: Option<PathBuf>,
    /// Optional knowledge store for analyze_own_structure(store_as_note=true).
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    /// Optional Neo4j client for querying dynamic capabilities (chains, tools, procedures).
    neo4j: Option<Neo4jClient>,
}

impl CodebaseSkill {
    pub fn new(
        codebase_dir: Option<PathBuf>,
        workspace_dir: Option<PathBuf>,
        proposals_dir: Option<PathBuf>,
        knowledge: Option<Arc<dyn KnowledgeStore>>,
        neo4j: Option<Neo4jClient>,
    ) -> Self {
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
        if let Some(ref dir) = proposals_dir {
            info!(path = %dir.display(), "CodebaseSkill: proposals directory configured");
        }
        Self {
            codebase_dir,
            workspace_dir,
            proposals_dir,
            knowledge,
            neo4j,
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn read_codebase_file_def() -> ToolDefinition {
        ToolDefinition {
            name: "read_codebase_file".to_string(),
            description: "Read a file from the agent's own codebase by path. Path is relative to the codebase root.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to codebase root (e.g. 'src/main.rs' or 'Cargo.toml')"
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
                        "description": "File path relative to codebase root (e.g. 'project-docs/TODO.md'). Must end in .md."
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

    fn list_codebase_files_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_codebase_files".to_string(),
            description:
                "List files in the codebase, optionally filtered by directory and filename pattern."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "directory": {
                        "type": "string",
                        "description": "Directory to list (relative to codebase root, default: root)"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Filename suffix or substring to filter (e.g. '.rs', 'mod.rs')"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum files to return (default: 100)"
                    }
                }
            }),
        }
    }

    fn search_codebase_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_codebase".to_string(),
            description: "Search the codebase for a regex pattern, like grep. Returns matching lines with file and line number context.".to_string(),
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

    fn get_file_tree_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_file_tree".to_string(),
            description:
                "Get a tree view of the codebase directory structure, skipping build artifacts."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "directory": {
                        "type": "string",
                        "description": "Subdirectory to tree (relative to codebase root, default: root)"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Maximum depth to traverse (default: 4, max: 8)"
                    }
                }
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

    fn get_git_diff_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_git_diff".to_string(),
            description: "Get the git diff between two refs (commits, branches, tags).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from_ref": {
                        "type": "string",
                        "description": "Starting git ref (e.g. 'HEAD~5', 'main', a commit hash)"
                    },
                    "to_ref": {
                        "type": "string",
                        "description": "Ending ref (default: 'HEAD')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Limit diff to this path (relative to codebase root)"
                    }
                },
                "required": ["from_ref"]
            }),
        }
    }

    fn list_proposals_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_proposals".to_string(),
            description: "List all pending fix proposals in the proposals directory. Returns a JSON array sorted newest-first.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "include_applied": {
                        "type": "boolean",
                        "description": "Also include applied/dismissed proposals (default: false)"
                    }
                }
            }),
        }
    }

    fn read_proposal_def() -> ToolDefinition {
        ToolDefinition {
            name: "read_proposal".to_string(),
            description: "Read the full markdown content of a specific proposal by filename."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Proposal filename as returned by list_proposals"
                    }
                },
                "required": ["filename"]
            }),
        }
    }

    fn dismiss_proposal_def() -> ToolDefinition {
        ToolDefinition {
            name: "dismiss_proposal".to_string(),
            description:
                "Mark a proposal as applied or dismissed, moving it to proposals/applied/."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Proposal filename to dismiss"
                    },
                    "reason": {
                        "type": "string",
                        "enum": ["applied", "rejected", "obsolete"],
                        "description": "Why this proposal is being dismissed"
                    }
                },
                "required": ["filename", "reason"]
            }),
        }
    }

    fn write_proposal_def() -> ToolDefinition {
        ToolDefinition {
            name: "write_proposal".to_string(),
            description: "Write a structured fix proposal to the proposals directory for human review. Use this after diagnosing a bug or improvement — it stages the proposal as a markdown file without touching the source code.".to_string(),
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
            description: "Generate a structured overview of the agent's own codebase: directory tree, workspace layout, skill registry, and recent git history. If store_as_note=true, persists the result to the knowledge graph as a semantic note.".to_string(),
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
            prepend_context: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let max_lines = args.max_lines.unwrap_or(500).min(2000);
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
        let truncated = total > max_lines;
        let shown = lines[..max_lines.min(total)].join("\n");

        let mut out = String::new();
        if let Some(ctx) = args.prepend_context
            && !ctx.trim().is_empty()
        {
            out.push_str(&ctx);
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!("// File: {}\n{}", args.path, shown));
        if truncated {
            out.push_str(&format!(
                "\n\n[... {} more lines truncated (total: {}) — use max_lines to read more ...]",
                total - max_lines,
                total
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

    async fn handle_list_codebase_files(&self, arguments: Option<Value>) -> ToolCallResult {
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
        let start_dir = match &args.directory {
            Some(d) => match self.safe_path(d) {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => match self.root() {
                Ok(p) => p,
                Err(e) => return e,
            },
        };

        let mut files: Vec<String> = Vec::new();
        collect_files(&start_dir, &start_dir, &args.pattern, &mut files, max);
        files.sort();

        ToolCallResult::success_text(format!(
            "Found {} file(s):\n{}",
            files.len(),
            files.join("\n")
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
            regex::Regex::new(&format!("(?i){}", &args.query))
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
            ToolCallResult::success_text(format!("No matches found for '{}'", args.query))
        } else {
            ToolCallResult::success_text(format!(
                "{} match(es) for '{}':\n\n{}",
                results.len(),
                args.query,
                results.join("\n---\n")
            ))
        }
    }

    async fn handle_get_file_tree(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            directory: Option<String>,
            max_depth: Option<usize>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let max_depth = args.max_depth.unwrap_or(4).min(8);
        let start = match &args.directory {
            Some(d) => match self.safe_path(d) {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => match self.root() {
                Ok(p) => p,
                Err(e) => return e,
            },
        };

        let root_name = start.file_name().and_then(|n| n.to_str()).unwrap_or(".");
        let mut out = format!("{}/\n", root_name);
        build_tree(&start, "", max_depth, 0, &mut out);

        ToolCallResult::success_text(out)
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

    async fn handle_get_git_diff(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            from_ref: String,
            to_ref: Option<String>,
            path: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let root = match self.root() {
            Ok(r) => r,
            Err(e) => return e,
        };
        let to = args.to_ref.as_deref().unwrap_or("HEAD");

        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&root);
        cmd.arg("diff");
        cmd.arg(format!("{}..{}", args.from_ref, to));
        cmd.arg("--stat");
        if let Some(ref p) = args.path {
            cmd.arg("--").arg(p);
        }

        match cmd.output().await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                ToolCallResult::success_text(if text.trim().is_empty() {
                    format!("No differences between {} and {}", args.from_ref, to)
                } else {
                    text
                })
            }
            Ok(out) => ToolCallResult::error(String::from_utf8_lossy(&out.stderr).to_string()),
            Err(e) => ToolCallResult::error(format!("git command failed: {e}")),
        }
    }

    // =========================================================================
    // Proposal reader / manager
    // =========================================================================

    async fn handle_list_proposals(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Args {
            include_applied: Option<bool>,
        }
        let args: Args = parse_args(arguments).unwrap_or_default();

        let proposals_dir = match &self.proposals_dir {
            Some(d) => d.clone(),
            None => return ToolCallResult::error("PROPOSALS_DIR not configured"),
        };

        let mut entries: Vec<serde_json::Value> = Vec::new();

        let dirs_to_scan: Vec<(PathBuf, bool)> = if args.include_applied.unwrap_or(false) {
            vec![
                (proposals_dir.clone(), false),
                (proposals_dir.join("applied"), true),
            ]
        } else {
            vec![(proposals_dir.clone(), false)]
        };

        for (dir, is_applied) in dirs_to_scan {
            let mut read_dir = match tokio::fs::read_dir(&dir).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".md") {
                    continue;
                }
                let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();

                // Parse metadata from the markdown header.
                let title = content
                    .lines()
                    .find(|l| l.starts_with("# Proposal:"))
                    .map(|l| l.trim_start_matches("# Proposal:").trim().to_string())
                    .unwrap_or_else(|| name.clone());
                let severity = content
                    .lines()
                    .find(|l| l.contains("**Severity:**"))
                    .and_then(|l| l.split("**Severity:**").nth(1))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let task_id = content
                    .lines()
                    .find(|l| l.contains("**Task ID:**"))
                    .and_then(|l| l.split('`').nth(1))
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let created = content
                    .lines()
                    .find(|l| l.contains("**Created:**"))
                    .and_then(|l| l.split("**Created:**").nth(1))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();

                entries.push(serde_json::json!({
                    "filename": name,
                    "title": title,
                    "severity": severity,
                    "task_id": task_id,
                    "created": created,
                    "applied": is_applied,
                }));
            }
        }

        // Sort newest-first by filename (timestamp prefix ensures lexicographic == chronological).
        entries.sort_by(|a, b| {
            b["filename"]
                .as_str()
                .unwrap_or("")
                .cmp(a["filename"].as_str().unwrap_or(""))
        });

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&serde_json::json!({ "proposals": entries }))
                .unwrap_or_default(),
        )
    }

    async fn handle_read_proposal(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            filename: String,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let proposals_dir = match &self.proposals_dir {
            Some(d) => d.clone(),
            None => return ToolCallResult::error("PROPOSALS_DIR not configured"),
        };

        // Accept filenames from both pending and applied subdirs.
        let candidates = [
            proposals_dir.join(&args.filename),
            proposals_dir.join("applied").join(&args.filename),
        ];
        for path in &candidates {
            if path.exists() {
                return match tokio::fs::read_to_string(path).await {
                    Ok(c) => ToolCallResult::success_text(c),
                    Err(e) => ToolCallResult::error(format!("Cannot read proposal: {e}")),
                };
            }
        }
        ToolCallResult::error(format!("Proposal '{}' not found", args.filename))
    }

    async fn handle_dismiss_proposal(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            filename: String,
            reason: String,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let proposals_dir = match &self.proposals_dir {
            Some(d) => d.clone(),
            None => return ToolCallResult::error("PROPOSALS_DIR not configured"),
        };

        let src = proposals_dir.join(&args.filename);
        if !src.exists() {
            return ToolCallResult::error(format!("Proposal '{}' not found", args.filename));
        }

        let applied_dir = proposals_dir.join("applied");
        if let Err(e) = tokio::fs::create_dir_all(&applied_dir).await {
            return ToolCallResult::error(format!("Cannot create applied dir: {e}"));
        }

        let dst = applied_dir.join(&args.filename);
        if let Err(e) = tokio::fs::rename(&src, &dst).await {
            return ToolCallResult::error(format!("Failed to move proposal: {e}"));
        }

        info!(filename = %args.filename, reason = %args.reason, "proposal dismissed");
        ToolCallResult::success_text(format!(
            "Proposal '{}' marked as {} and moved to applied/.",
            args.filename, args.reason
        ))
    }

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

        let proposals_dir = match &self.proposals_dir {
            Some(d) => d.clone(),
            None => {
                return ToolCallResult::error(
                    "PROPOSALS_DIR not configured — set PROPOSALS_DIR env var",
                );
            }
        };

        if let Err(e) = tokio::fs::create_dir_all(&proposals_dir).await {
            return ToolCallResult::error(format!("Cannot create proposals dir: {e}"));
        }

        let now = chrono::Utc::now();
        let timestamp = now.format("%Y%m%dT%H%M%SZ");
        // Slugify the title for the filename.
        let slug: String = args
            .title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let filename = format!(
            "{}-{}-{}.md",
            timestamp,
            &args.task_id[..8.min(args.task_id.len())],
            slug
        );
        let path = proposals_dir.join(&filename);

        let affected = args.affected_file.as_deref().unwrap_or("unknown");
        let mut content = format!(
            "# Proposal: {title}\n\n\
             - **Created:** {ts}\n\
             - **Task ID:** `{task_id}`\n\
             - **Severity:** {severity}\n\
             - **Affected file:** `{affected}`\n\n\
             ## Diagnosis\n\n{diagnosis}\n\n\
             ## Proposed Fix\n\n{proposed_fix}\n",
            title = args.title,
            ts = now.to_rfc3339(),
            task_id = args.task_id,
            severity = args.severity,
            affected = affected,
            diagnosis = args.diagnosis,
            proposed_fix = args.proposed_fix,
        );
        if let Some(ref snippet) = args.code_snippet {
            content.push_str(&format!("\n## Code\n\n```\n{}\n```\n", snippet));
        }
        content.push_str(
            "\n---\n*Auto-generated by agent-brain. Human review required before applying.*\n",
        );

        if let Err(e) = tokio::fs::write(&path, &content).await {
            return ToolCallResult::error(format!("Failed to write proposal: {e}"));
        }

        info!(file = %filename, severity = %args.severity, "write_proposal: proposal staged");
        ToolCallResult::success_text(format!(
            "Proposal written: {filename}\nPath: {}\nReview and apply manually when ready.",
            path.display()
        ))
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

        // Section 1: Directory tree (depth 3)
        let tree = self
            .handle_get_file_tree(Some(json!({"max_depth": 3})))
            .await;
        sections.push(format!(
            "## Directory Structure\n```\n{}\n```",
            extract_text(&tree)
        ));

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

        // Section 5: Graph-stored dynamic capabilities (SchedulerChains, DynamicTools, Procedures)
        if let Some(neo4j) = &self.neo4j {
            use neo4rs::query;

            let mut dyn_lines: Vec<String> = Vec::new();

            // SchedulerChain nodes — the brain's learned/configured task automation
            match neo4j
                .execute(query(
                    "MATCH (c:SchedulerChain) RETURN c.pattern AS pattern, c.description AS description, c.priority AS priority ORDER BY c.priority DESC, c.pattern",
                ))
                .await
            {
                Ok(rows) if !rows.is_empty() => {
                    dyn_lines.push("### SchedulerChains (graph-stored task automation)".into());
                    for row in &rows {
                        let pattern = row.get::<String>("pattern").unwrap_or_default();
                        let desc = row.get::<String>("description").unwrap_or_default();
                        let prio = row.get::<i64>("priority").unwrap_or(1);
                        dyn_lines.push(format!("- [{prio}] {pattern}: {desc}"));
                    }
                }
                _ => {}
            }

            // DynamicTool nodes — runtime-defined MCP tools
            match neo4j
                .execute(query(
                    "MATCH (t:DynamicTool) RETURN t.name AS name, t.description AS description ORDER BY t.name",
                ))
                .await
            {
                Ok(rows) if !rows.is_empty() => {
                    dyn_lines.push("\n### DynamicTools (runtime-defined MCP tools)".into());
                    for row in &rows {
                        let name = row.get::<String>("name").unwrap_or_default();
                        let desc = row.get::<String>("description").unwrap_or_default();
                        dyn_lines.push(format!("- {name}: {desc}"));
                    }
                }
                _ => {}
            }

            // Procedure nodes — named multi-step workflows
            match neo4j
                .execute(query(
                    "MATCH (p:Procedure) RETURN p.name AS name, p.description AS description ORDER BY p.name",
                ))
                .await
            {
                Ok(rows) if !rows.is_empty() => {
                    dyn_lines.push("\n### Procedures (named multi-step workflows)".into());
                    for row in &rows {
                        let name = row.get::<String>("name").unwrap_or_default();
                        let desc = row.get::<String>("description").unwrap_or_default();
                        dyn_lines.push(format!("- {name}: {desc}"));
                    }
                }
                _ => {}
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
                .store_note(
                    &content,
                    Some("semantic"),
                    Some("codebase_self_analysis"),
                    None,
                    Some(ProvenanceFlag::SynthesisInference),
                )
                .await
            {
                Ok((id, chunks)) => {
                    info!(note_id = %id, chunks = chunks, "Stored codebase self-analysis note");
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
            Self::list_codebase_files_def(),
            Self::search_codebase_def(),
            Self::get_file_tree_def(),
            Self::get_git_log_def(),
            Self::get_git_diff_def(),
            Self::list_proposals_def(),
            Self::read_proposal_def(),
            Self::dismiss_proposal_def(),
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
            "list_codebase_files" => Some(self.handle_list_codebase_files(arguments).await),
            "search_codebase" => Some(self.handle_search_codebase(arguments).await),
            "get_file_tree" => Some(self.handle_get_file_tree(arguments).await),
            "get_git_log" => Some(self.handle_get_git_log(arguments).await),
            "get_git_diff" => Some(self.handle_get_git_diff(arguments).await),
            "list_proposals" => Some(self.handle_list_proposals(arguments).await),
            "read_proposal" => Some(self.handle_read_proposal(arguments).await),
            "dismiss_proposal" => Some(self.handle_dismiss_proposal(arguments).await),
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

/// Build an ASCII directory tree into `out`.
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

/// Walk up from `current_dir()` to find the repo root (directory containing `Cargo.toml`).
pub fn detect_repo_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
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
