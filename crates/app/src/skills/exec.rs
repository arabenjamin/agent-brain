//! Code Execution Skill — tool-integrated reasoning.
//!
//! Gives the brain a way to *compute* an answer rather than assert one. A model
//! asked for a shortage date or a capacity delta will produce a confident,
//! unchecked number; the same model asked to write six lines of Python and read
//! the output produces a number that either survives execution or fails loudly.
//! That difference is the entire point of the tool.
//!
//! Execution happens in the `sandbox` compose service, never in this process.
//! See docker-compose.yml for the isolation contract — an `internal: true`
//! network with no egress, a read-only filesystem, no credentials, and no bind
//! mounts. This skill is only the client.
//!
//! Registered only when `SANDBOX_URL` is set, so a deployment without the
//! sandbox service simply lacks the tool instead of failing every call.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{info, warn};

use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Ceiling for a single run, mirroring the sandbox's own cap.
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Code Execution Skill.
pub struct ExecSkill {
    base_url: String,
    client: reqwest::Client,
}

impl ExecSkill {
    /// Read the sandbox endpoint from the environment.
    ///
    /// `None` when `SANDBOX_URL` is unset or blank — the caller skips
    /// registration entirely in that case.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("SANDBOX_URL").ok()?;
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return None;
        }
        Some(Self::new(url))
    }

    /// Create a skill pointed at an explicit sandbox base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            // Outlives the sandbox's own wall clock so a timed-out *run* comes
            // back as a structured result rather than a transport error — the
            // model can act on "your code timed out", not on "the tool broke".
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(MAX_TIMEOUT_MS + 30_000))
                .build()
                .unwrap_or_default(),
        }
    }

    fn execute_code_def() -> ToolDefinition {
        ToolDefinition {
            name: "execute_code".to_string(),
            description:
                "Run Python in an isolated sandbox and return what it printed. Use this for \
                 ANY quantitative work — arithmetic, projections, unit conversion, date math, \
                 aggregation, statistics, symbolic algebra — instead of computing in prose, \
                 where numbers drift silently. Print the results you need; nothing else is \
                 captured. numpy, sympy and pandas are available. The sandbox has NO network \
                 access and NO access to the brain's files or credentials, so code cannot \
                 fetch data, call APIs, or read the repository: pass every input inline in \
                 the code. Each run starts from an empty scratch directory and keeps nothing, \
                 so state does not carry between calls."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python 3 source to execute. Use print() for every value you want back."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wall-clock limit in milliseconds (default 30000, max 120000)."
                    }
                },
                "required": ["code"]
            }),
        }
    }

    async fn handle_execute_code(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExecuteCodeInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        if input.code.trim().is_empty() {
            return ToolCallResult::error("`code` must be a non-empty string");
        }

        let timeout_ms = input
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1_000, MAX_TIMEOUT_MS);

        let url = format!("{}/exec", self.base_url);
        info!(
            code_len = input.code.len(),
            timeout_ms, "Executing code in sandbox"
        );

        let response = self
            .client
            .post(&url)
            .json(&json!({ "code": input.code, "timeout_ms": timeout_ms }))
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, url = %url, "Sandbox request failed");
                return ToolCallResult::error(format!(
                    "Sandbox unreachable at {url}: {e}. Is the `sandbox` service running?"
                ));
            }
        };

        let status = response.status();
        let body: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolCallResult::error(format!("Sandbox returned unreadable response: {e}"));
            }
        };

        if !status.is_success() {
            let msg = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return ToolCallResult::error(format!("Sandbox rejected the run ({status}): {msg}"));
        }

        let stdout = body.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        let stderr = body.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
        let exit_code = body.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
        let timed_out = body
            .get("timed_out")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // A non-zero exit is reported as a successful tool call carrying a
        // failed run. The distinction is load-bearing: a tool error would
        // burn a retry and can dead-letter the job (and, via chain-death
        // attribution, its Task), whereas a traceback handed back to the model
        // is exactly the signal it needs to fix the code and call again.
        let summary = if timed_out {
            format!("Execution timed out after {timeout_ms}ms.")
        } else if exit_code == 0 {
            "Execution succeeded.".to_string()
        } else {
            format!("Execution failed with exit code {exit_code}.")
        };

        ToolCallResult::success_json(json!({
            "success": exit_code == 0 && !timed_out,
            "summary": summary,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "truncated": body.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false),
            "duration_ms": body.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0),
        }))
    }
}

#[async_trait]
impl Skill for ExecSkill {
    fn name(&self) -> &str {
        "Code Execution"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::execute_code_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "execute_code" => Some(self.handle_execute_code(arguments).await),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExecuteCodeInput {
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_rejects_blank_url() {
        // Safety: single-threaded test, no other thread reads the environment.
        unsafe { std::env::set_var("SANDBOX_URL", "   ") };
        assert!(ExecSkill::from_env().is_none());
        unsafe { std::env::remove_var("SANDBOX_URL") };
        assert!(ExecSkill::from_env().is_none());
    }

    #[test]
    fn base_url_loses_trailing_slash() {
        unsafe { std::env::set_var("SANDBOX_URL", "http://sandbox:8000/") };
        let skill = ExecSkill::from_env().expect("skill should build");
        assert_eq!(skill.base_url, "http://sandbox:8000");
        unsafe { std::env::remove_var("SANDBOX_URL") };
    }

    #[test]
    fn exposes_execute_code() {
        let skill = ExecSkill::new("http://sandbox:8000");
        let tools = skill.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "execute_code");
    }
}
