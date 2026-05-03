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

use crate::services::KnowledgeStore;
use crate::skills::Skill;
use agent_brain_models::ProvenanceFlag;
use agent_brain_protocol::{Content, ToolCallResult, ToolDefinition, parse_args};

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
}

impl CodebaseSkill {
    pub fn new(
        codebase_dir: Option<PathBuf>,
        workspace_dir: Option<PathBuf>,
        proposals_dir: Option<PathBuf>,
        knowledge: Option<Arc<dyn KnowledgeStore>>,
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
                    }
                },
                "required": ["path"]
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
            description: "Get recent git commit history for the codebase.".to_string(),
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

        let mut out = format!("// File: {}\n{}", args.path, shown);
        if truncated {
            out.push_str(&format!(
                "\n\n[... {} more lines truncated (total: {}) — use max_lines to read more ...]",
                total - max_lines,
                total
            ));
        }
        ToolCallResult::success_text(out)
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

        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&root);
        cmd.arg("log");
        cmd.arg(format!("-{n}"));
        cmd.arg("--format=%h %ad %an: %s");
        cmd.arg("--date=short");
        if let Some(ref p) = args.path {
            cmd.arg("--").arg(p);
        }

        match cmd.output().await {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                ToolCallResult::success_text(if text.trim().is_empty() {
                    "No commits found".to_string()
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
