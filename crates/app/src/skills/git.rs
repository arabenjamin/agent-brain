//! Git Skill — write files to the codebase and manage the git + GitHub PR workflow.
//!
//! Provides 6 tools:
//! - `git_status`        — current branch, staged/unstaged changes
//! - `git_create_branch` — create and checkout a new branch
//! - `write_codebase_file` — write (or overwrite) a file inside CODEBASE_DIR
//! - `git_commit`        — stage files and create a commit
//! - `git_push`          — push current branch to origin
//! - `git_create_pr`     — open a GitHub pull request via the API

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;
use tokio::process::Command;
use tracing::info;

use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

pub struct GitSkill {
    codebase_dir: Option<PathBuf>,
    github_token: Option<String>,
}

impl GitSkill {
    pub fn new(codebase_dir: Option<PathBuf>) -> Self {
        let github_token = std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty());
        Self {
            codebase_dir,
            github_token,
        }
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    fn codebase_dir(&self) -> Result<&PathBuf, ToolCallResult> {
        self.codebase_dir.as_ref().ok_or_else(|| {
            ToolCallResult::error("CODEBASE_DIR is not configured — GitSkill unavailable")
        })
    }

    /// Resolve a relative path inside codebase_dir, rejecting path-traversal attempts.
    fn resolve_safe(&self, rel: &str) -> Result<PathBuf, ToolCallResult> {
        let root = self.codebase_dir()?;
        let joined = root.join(rel);
        let canonical_root = root.canonicalize().map_err(|e| {
            ToolCallResult::error(format!("Cannot canonicalize codebase root: {e}"))
        })?;
        // Don't require the file to exist yet — just check no component escapes root.
        let mut resolved = canonical_root.clone();
        for component in Path::new(rel).components() {
            match component {
                Component::ParentDir => {
                    return Err(ToolCallResult::error(
                        "Path traversal ('..') is not allowed in codebase paths",
                    ));
                }
                Component::Normal(c) => resolved.push(c),
                _ => {}
            }
        }
        // Paranoia check: must still be inside root after any symlinks in prefix.
        if !resolved.starts_with(&canonical_root) && !joined.starts_with(root) {
            return Err(ToolCallResult::error("Path escapes codebase root"));
        }
        Ok(resolved)
    }

    async fn run_git(&self, args: &[&str]) -> Result<String, String> {
        let root = self
            .codebase_dir
            .as_ref()
            .ok_or_else(|| "CODEBASE_DIR not configured".to_string())?;
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .await
            .map_err(|e| format!("git spawn failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(if stderr.is_empty() { stdout } else { stderr })
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn git_status_def() -> ToolDefinition {
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Show the current git branch and a summary of staged/unstaged changes in the codebase.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn git_create_branch_def() -> ToolDefinition {
        ToolDefinition {
            name: "git_create_branch".to_string(),
            description: "Create and checkout a new git branch in the codebase. Optionally specify a base ref (commit, branch, or tag) to branch from.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Branch name (e.g. 'feature/config-modal')"
                    },
                    "base": {
                        "type": "string",
                        "description": "Optional base ref to branch from (default: current HEAD)"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn write_codebase_file_def() -> ToolDefinition {
        ToolDefinition {
            name: "write_codebase_file".to_string(),
            description: "Write (create or overwrite) a file inside the codebase. Path is relative to the codebase root. Parent directories are created automatically. Use this before git_commit to stage changes.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to codebase root (e.g. 'hbi-frontend/src/components/ConfigModal.tsx')"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file content to write"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn git_commit_def() -> ToolDefinition {
        ToolDefinition {
            name: "git_commit".to_string(),
            description: "Stage files and create a git commit in the codebase. By default stages all modified/new tracked files. Specify 'paths' to stage only specific files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Commit message"
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Specific file paths to stage (relative to codebase root). If omitted, stages all changes ('git add -A')."
                    }
                },
                "required": ["message"]
            }),
        }
    }

    fn git_push_def() -> ToolDefinition {
        ToolDefinition {
            name: "git_push".to_string(),
            description: "Push the current branch to origin. Sets upstream tracking on first push."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "force": {
                        "type": "boolean",
                        "description": "Use --force-with-lease (safe force push). Default false."
                    }
                }
            }),
        }
    }

    fn git_create_pr_def() -> ToolDefinition {
        ToolDefinition {
            name: "git_create_pr".to_string(),
            description: "Create a GitHub pull request for the current branch. Requires GITHUB_TOKEN to be set. Auto-detects the repo from 'git remote get-url origin'.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "PR title (keep under 72 characters)"
                    },
                    "body": {
                        "type": "string",
                        "description": "PR description (markdown supported)"
                    },
                    "base": {
                        "type": "string",
                        "description": "Base branch to merge into (default: 'dev')"
                    },
                    "draft": {
                        "type": "boolean",
                        "description": "Open as a draft PR (default: false)"
                    }
                },
                "required": ["title", "body"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_git_status(&self) -> ToolCallResult {
        let branch = self
            .run_git(&["branch", "--show-current"])
            .await
            .unwrap_or_else(|_| "(detached HEAD)".to_string());
        let status = match self.run_git(&["status", "--short"]).await {
            Ok(s) if s.is_empty() => "nothing to commit, working tree clean".to_string(),
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("git status failed: {e}")),
        };
        ToolCallResult::success_text(format!("Branch: {branch}\n\n{status}"))
    }

    async fn handle_git_create_branch(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            base: Option<String>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let git_args: Vec<&str> = if let Some(ref base) = args.base {
            vec!["checkout", "-b", &args.name, base]
        } else {
            vec!["checkout", "-b", &args.name]
        };
        match self.run_git(&git_args).await {
            Ok(_) => {
                info!(branch = %args.name, "git_create_branch: created and checked out");
                ToolCallResult::success_text(format!("Switched to new branch '{}'", args.name))
            }
            Err(e) => ToolCallResult::error(format!("git checkout -b failed: {e}")),
        }
    }

    async fn handle_write_codebase_file(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let full_path = match self.resolve_safe(&args.path) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(parent) = full_path.parent()
            && let Err(e) = fs::create_dir_all(parent).await
        {
            return ToolCallResult::error(format!("Failed to create directories: {e}"));
        }
        if let Err(e) = fs::write(&full_path, &args.content).await {
            return ToolCallResult::error(format!("Failed to write file: {e}"));
        }
        let bytes = args.content.len();
        info!(path = %args.path, bytes = bytes, "write_codebase_file: wrote file");
        ToolCallResult::success_text(format!("Wrote {} bytes to '{}'", bytes, args.path))
    }

    async fn handle_git_commit(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            message: String,
            paths: Option<Vec<String>>,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        // Stage files.
        let stage_result = if let Some(ref paths) = args.paths {
            let mut git_args = vec!["add", "--"];
            let path_strs: Vec<&str> = paths.iter().map(String::as_str).collect();
            git_args.extend_from_slice(&path_strs);
            self.run_git(&git_args).await
        } else {
            self.run_git(&["add", "-A"]).await
        };
        if let Err(e) = stage_result {
            return ToolCallResult::error(format!("git add failed: {e}"));
        }

        // Commit.
        match self.run_git(&["commit", "-m", &args.message]).await {
            Ok(out) => {
                info!(message = %args.message, "git_commit: committed");
                ToolCallResult::success_text(out)
            }
            Err(e) => ToolCallResult::error(format!("git commit failed: {e}")),
        }
    }

    async fn handle_git_push(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            force: bool,
        }
        let args: Args = parse_args(arguments).unwrap_or(Args { force: false });

        // Get current branch name.
        let branch = match self.run_git(&["branch", "--show-current"]).await {
            Ok(b) if !b.is_empty() => b,
            _ => return ToolCallResult::error("Could not determine current branch"),
        };

        let mut git_args = vec!["push", "-u", "origin", &branch];
        if args.force {
            git_args.push("--force-with-lease");
        }
        match self.run_git(&git_args).await {
            Ok(out) => {
                info!(branch = %branch, "git_push: pushed");
                ToolCallResult::success_text(if out.is_empty() {
                    format!("Pushed branch '{}' to origin", branch)
                } else {
                    out
                })
            }
            Err(e) => ToolCallResult::error(format!("git push failed: {e}")),
        }
    }

    async fn handle_git_create_pr(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Args {
            title: String,
            body: String,
            base: Option<String>,
            #[serde(default)]
            draft: bool,
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let token = match &self.github_token {
            Some(t) => t.clone(),
            None => return ToolCallResult::error("GITHUB_TOKEN is not set — cannot create PR"),
        };

        // Detect owner/repo from remote URL.
        let remote_url = match self.run_git(&["remote", "get-url", "origin"]).await {
            Ok(u) => u,
            Err(e) => return ToolCallResult::error(format!("Cannot read git remote: {e}")),
        };
        let (owner, repo) = match parse_github_owner_repo(&remote_url) {
            Some(pair) => pair,
            None => {
                return ToolCallResult::error(format!(
                    "Cannot parse owner/repo from remote URL: {remote_url}"
                ));
            }
        };

        // Current branch becomes the PR head.
        let head = match self.run_git(&["branch", "--show-current"]).await {
            Ok(b) if !b.is_empty() => b,
            _ => return ToolCallResult::error("Could not determine current branch for PR head"),
        };

        let base = args.base.unwrap_or_else(|| "dev".to_string());
        let api_url = format!("https://api.github.com/repos/{owner}/{repo}/pulls");

        let client = reqwest::Client::new();
        let resp = client
            .post(&api_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "agent-brain/0.1")
            .json(&json!({
                "title": args.title,
                "body": args.body,
                "head": head,
                "base": base,
                "draft": args.draft
            }))
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                let body: Value = r.json().await.unwrap_or(json!({}));
                if status.is_success() {
                    let pr_url = body
                        .get("html_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown URL)");
                    let pr_number = body.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                    info!(pr = pr_number, url = pr_url, "git_create_pr: PR opened");
                    ToolCallResult::success_text(format!("PR #{pr_number} created: {pr_url}"))
                } else {
                    let msg = body
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    ToolCallResult::error(format!("GitHub API error {status}: {msg}"))
                }
            }
            Err(e) => ToolCallResult::error(format!("HTTP request to GitHub failed: {e}")),
        }
    }
}

/// Parse `owner` and `repo` from SSH or HTTPS GitHub remote URLs.
fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    // SSH: git@github.com:owner/repo.git
    // HTTPS: https://github.com/owner/repo.git  or  https://github.com/owner/repo
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        rest
    } else {
        return None;
    };
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

#[async_trait]
impl Skill for GitSkill {
    fn name(&self) -> &str {
        "git"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::git_status_def(),
            Self::git_create_branch_def(),
            Self::write_codebase_file_def(),
            Self::git_commit_def(),
            Self::git_push_def(),
            Self::git_create_pr_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "git_status" => Some(self.handle_git_status().await),
            "git_create_branch" => Some(self.handle_git_create_branch(arguments).await),
            "write_codebase_file" => Some(self.handle_write_codebase_file(arguments).await),
            "git_commit" => Some(self.handle_git_commit(arguments).await),
            "git_push" => Some(self.handle_git_push(arguments).await),
            "git_create_pr" => Some(self.handle_git_create_pr(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_url() {
        let (o, r) = parse_github_owner_repo("git@github.com:arabenjamin/agent-brain.git").unwrap();
        assert_eq!(o, "arabenjamin");
        assert_eq!(r, "agent-brain");
    }

    #[test]
    fn parse_https_url() {
        let (o, r) =
            parse_github_owner_repo("https://github.com/arabenjamin/agent-brain.git").unwrap();
        assert_eq!(o, "arabenjamin");
        assert_eq!(r, "agent-brain");
    }

    #[test]
    fn parse_https_no_dot_git() {
        let (o, r) = parse_github_owner_repo("https://github.com/arabenjamin/agent-brain").unwrap();
        assert_eq!(o, "arabenjamin");
        assert_eq!(r, "agent-brain");
    }

    #[test]
    fn parse_unknown_url_returns_none() {
        assert!(parse_github_owner_repo("https://gitlab.com/foo/bar.git").is_none());
    }

    #[test]
    fn tools_list_has_six_tools() {
        let skill = GitSkill::new(None);
        assert_eq!(skill.tools().len(), 6);
    }
}
