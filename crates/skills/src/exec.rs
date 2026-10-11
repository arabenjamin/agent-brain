//! Code Execution Skill — tool-integrated reasoning.
//!
//! Gives the brain a way to *compute* an answer rather than assert one. A model
//! asked for a shortage date or a capacity delta will produce a confident,
//! unchecked number; the same model asked to write six lines of Python and read
//! the output produces a number that either survives execution or fails loudly.
//! That difference is the entire point of the tool.
//!
//! Rust goes through the same tool with `language: "rust"`: a crate either
//! compiles and passes its tests or it hands back the compiler's error, which
//! is the signal a model needs to fix it.
//!
//! Execution happens in the `sandbox` (Python) and `rust-sandbox` compose
//! services, never in this process. See docker-compose.yml for the isolation
//! contract — an `internal: true` network with no egress, a read-only
//! filesystem, no credentials, and no bind mounts. This skill is only the
//! client.
//!
//! Registered only when `SANDBOX_URL` and/or `RUST_SANDBOX_URL` is set, and a
//! language is only offered when its sandbox is configured, so a deployment
//! without one simply lacks it instead of failing every call.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::time::Duration;
use tracing::{info, warn};

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Python ceilings, mirroring the sandbox's own caps.
const PYTHON_MAX_TIMEOUT_MS: u64 = 120_000;
const PYTHON_DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Rust ceilings: a cold build of a crate with dependencies takes far longer
/// than a Python script. Mirrors rust-sandbox/server.py.
const RUST_MAX_TIMEOUT_MS: u64 = 300_000;
const RUST_DEFAULT_TIMEOUT_MS: u64 = 120_000;

const RUST_COMMANDS: &[&str] = &["test", "check", "clippy", "fmt", "run", "build"];

/// Code Execution Skill.
pub struct ExecSkill {
    python_url: Option<String>,
    rust_url: Option<String>,
    client: reqwest::Client,
}

fn normalize_url(raw: Option<String>) -> Option<String> {
    let url = raw?.trim().trim_end_matches('/').to_string();
    (!url.is_empty()).then_some(url)
}

impl ExecSkill {
    /// Read the sandbox endpoints from the environment.
    ///
    /// `None` when neither `SANDBOX_URL` nor `RUST_SANDBOX_URL` is set — the
    /// caller skips registration entirely in that case.
    pub fn from_env() -> Option<Self> {
        let python = normalize_url(std::env::var("SANDBOX_URL").ok());
        let rust = normalize_url(std::env::var("RUST_SANDBOX_URL").ok());
        if python.is_none() && rust.is_none() {
            return None;
        }
        Some(Self::with_urls(python, rust))
    }

    /// Create a Python-only skill pointed at an explicit sandbox base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_urls(normalize_url(Some(base_url.into())), None)
    }

    /// Create a skill with explicit (optional) Python and Rust sandbox URLs.
    pub fn with_urls(python_url: Option<String>, rust_url: Option<String>) -> Self {
        Self {
            python_url,
            rust_url,
            // Outlives the longest sandbox wall clock so a timed-out *run* comes
            // back as a structured result rather than a transport error — the
            // model can act on "your code timed out", not on "the tool broke".
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(RUST_MAX_TIMEOUT_MS + 30_000))
                .build()
                .unwrap_or_default(),
        }
    }

    fn languages(&self) -> Vec<&'static str> {
        let mut langs = Vec::new();
        if self.python_url.is_some() {
            langs.push("python");
        }
        if self.rust_url.is_some() {
            langs.push("rust");
        }
        langs
    }

    fn execute_code_def(&self) -> ToolDefinition {
        let langs = self.languages();
        let mut description = String::from(
            "Run code in an isolated sandbox and return its output. The sandbox has NO network \
             access and NO access to the brain's files or credentials: pass every input inline. \
             Each run starts from an empty scratch directory and keeps nothing between calls.",
        );
        if self.python_url.is_some() {
            description.push_str(
                " PYTHON (language=\"python\", the default): use this for ANY quantitative work — \
                 arithmetic, projections, unit conversion, date math, aggregation, statistics, \
                 symbolic algebra — instead of computing in prose, where numbers drift silently. \
                 Print the results you need; nothing else is captured. numpy, sympy and pandas \
                 are available.",
            );
        }
        if self.rust_url.is_some() {
            description.push_str(
                " RUST (language=\"rust\"): compile, lint and test Rust. Pass `code` for a single \
                 main.rs (runs with cargo run), or `files` for a whole crate as a path->content \
                 map (e.g. Cargo.toml, src/lib.rs, tests/foo.rs; defaults to cargo test). \
                 Crates come from an OFFLINE cache (anyhow, itertools, pretty_assertions, rand, \
                 regex, serde, serde_json, thiserror, time); anything else fails to resolve. \
                 A compile error or failing test is returned as a result to act on — read the \
                 compiler output, fix the code, and call again.",
            );
        }

        let mut properties = Map::new();
        properties.insert(
            "language".into(),
            json!({
                "type": "string",
                "enum": langs,
                "description": format!("Language to run (default \"{}\").", langs[0]),
            }),
        );
        properties.insert(
            "code".into(),
            json!({
                "type": "string",
                "description": "Source to execute. Python: use print() for every value you want back. Rust: the contents of src/main.rs."
            }),
        );
        if self.rust_url.is_some() {
            properties.insert(
                "files".into(),
                json!({
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Rust only: a whole crate as relative path -> file content. Include Cargo.toml when tests import the crate by name. Mutually exclusive with `code`."
                }),
            );
            properties.insert(
                "command".into(),
                json!({
                    "type": "string",
                    "enum": RUST_COMMANDS,
                    "description": "Rust only: cargo subcommand (default `test` for `files`, `run` for `code`)."
                }),
            );
            properties.insert(
                "include_ignored".into(),
                json!({
                    "type": "boolean",
                    "description": "Rust only: with command=test, also run #[ignore] tests."
                }),
            );
        }
        properties.insert(
            "timeout_ms".into(),
            json!({
                "type": "integer",
                "description": "Wall-clock limit in milliseconds (Python default 30000, max 120000; Rust default 120000, max 300000)."
            }),
        );

        ToolDefinition {
            name: "execute_code".to_string(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": properties,
            }),
        }
    }

    /// Resolve an input to the sandbox URL and request body, or a caller-facing
    /// error. Pure, so the routing and validation are testable without HTTP.
    fn build_request(&self, input: &ExecuteCodeInput) -> Result<(String, Value, u64), String> {
        let default_lang = self.languages().first().copied().unwrap_or("python");
        let language = input.language.as_deref().unwrap_or(default_lang);

        match language {
            "python" => {
                let base = self
                    .python_url
                    .as_ref()
                    .ok_or("language \"python\" is not available: SANDBOX_URL is not configured")?;
                if input.files.is_some() || input.command.is_some() {
                    return Err("`files` and `command` are Rust-only; Python takes `code`".into());
                }
                let code = input.code.as_deref().unwrap_or("");
                if code.trim().is_empty() {
                    return Err("`code` must be a non-empty string".into());
                }
                let timeout_ms = input
                    .timeout_ms
                    .unwrap_or(PYTHON_DEFAULT_TIMEOUT_MS)
                    .clamp(1_000, PYTHON_MAX_TIMEOUT_MS);
                Ok((
                    format!("{base}/exec"),
                    json!({ "code": code, "timeout_ms": timeout_ms }),
                    timeout_ms,
                ))
            }
            "rust" => {
                let base = self.rust_url.as_ref().ok_or(
                    "language \"rust\" is not available: RUST_SANDBOX_URL is not configured",
                )?;
                let timeout_ms = input
                    .timeout_ms
                    .unwrap_or(RUST_DEFAULT_TIMEOUT_MS)
                    .clamp(1_000, RUST_MAX_TIMEOUT_MS);
                let mut body = Map::new();
                match (&input.code, &input.files) {
                    (Some(_), Some(_)) => {
                        return Err("pass either `code` or `files`, not both".into());
                    }
                    (Some(code), None) if !code.trim().is_empty() => {
                        body.insert("code".into(), json!(code));
                    }
                    (None, Some(files)) if !files.is_empty() => {
                        body.insert("files".into(), json!(files));
                    }
                    _ => {
                        return Err(
                            "Rust needs a non-empty `code` (a main.rs) or `files` (a crate)".into(),
                        );
                    }
                }
                if let Some(command) = &input.command {
                    if !RUST_COMMANDS.contains(&command.as_str()) {
                        return Err(format!("`command` must be one of {RUST_COMMANDS:?}"));
                    }
                    body.insert("command".into(), json!(command));
                }
                if let Some(include_ignored) = input.include_ignored {
                    body.insert("include_ignored".into(), json!(include_ignored));
                }
                body.insert("timeout_ms".into(), json!(timeout_ms));
                Ok((format!("{base}/exec"), Value::Object(body), timeout_ms))
            }
            other => Err(format!(
                "unsupported language {other:?}; available: {:?}",
                self.languages()
            )),
        }
    }

    async fn handle_execute_code(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExecuteCodeInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let (url, body, timeout_ms) = match self.build_request(&input) {
            Ok(request) => request,
            Err(msg) => return ToolCallResult::error(msg),
        };

        info!(
            url = %url,
            language = input.language.as_deref().unwrap_or("default"),
            timeout_ms,
            "Executing code in sandbox"
        );

        let response = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, url = %url, "Sandbox request failed");
                return ToolCallResult::error(format!(
                    "Sandbox unreachable at {url}: {e}. Is the sandbox service running?"
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

        ToolCallResult::success_json(summarize_run(&body, timeout_ms))
    }
}

/// Shape a sandbox response into the tool result.
///
/// A non-zero exit is reported as a successful tool call carrying a failed
/// run. The distinction is load-bearing: a tool error would burn a retry and
/// can dead-letter the job (and, via chain-death attribution, its Task),
/// whereas a traceback or compiler error handed back to the model is exactly
/// the signal it needs to fix the code and call again.
fn summarize_run(body: &Value, timeout_ms: u64) -> Value {
    let exit_code = body.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let timed_out = body
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let compile_failed = body.get("compile_failed").and_then(|v| v.as_bool());
    let tests_passed = body.get("tests_passed").and_then(|v| v.as_i64());
    let tests_failed = body.get("tests_failed").and_then(|v| v.as_i64());
    let command = body.get("command").and_then(|v| v.as_str());
    let lint_findings = body.get("lint_findings").and_then(|v| v.as_i64());

    let summary = if timed_out {
        format!("Execution timed out after {timeout_ms}ms.")
    } else if compile_failed == Some(true) {
        "Compilation failed — see stderr for the compiler errors.".to_string()
    } else if command == Some("clippy") && exit_code == 0 {
        match lint_findings {
            Some(0) | None => "Clippy is clean: no findings.".to_string(),
            Some(n) => {
                format!("Clippy reported {n} findings — see stderr; fix them and run clippy again.")
            }
        }
    } else if command == Some("fmt") {
        if exit_code == 0 {
            "Formatting is clean: rustfmt would change nothing.".to_string()
        } else {
            "rustfmt would change the code — stdout holds the diff; apply it and run fmt again."
                .to_string()
        }
    } else if exit_code == 0 {
        match tests_passed {
            Some(passed) => format!("Build succeeded; {passed} tests passed."),
            None => "Execution succeeded.".to_string(),
        }
    } else {
        match (tests_passed, tests_failed) {
            (Some(passed), Some(failed)) => {
                format!("Tests failed: {failed} failed, {passed} passed (exit code {exit_code}).")
            }
            _ => format!("Execution failed with exit code {exit_code}."),
        }
    };

    let mut result = json!({
        "success": exit_code == 0 && !timed_out,
        "summary": summary,
        "stdout": body.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
        "stderr": body.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "truncated": body.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false),
        "duration_ms": body.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0),
    });
    // Rust-only fields: present only when the sandbox reported them, so a
    // Python result is unchanged.
    for key in [
        "command",
        "compile_failed",
        "tests_passed",
        "tests_failed",
        "lint_findings",
    ] {
        if let Some(value) = body.get(key).filter(|v| !v.is_null()) {
            result[key] = value.clone();
        }
    }
    result
}

#[async_trait]
impl Skill for ExecSkill {
    fn name(&self) -> &str {
        "Code Execution"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![self.execute_code_def()]
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
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    files: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    include_ignored: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn clippy_and_fmt_results_say_what_to_do() {
        let warned = summarize_run(
            &serde_json::json!({"exit_code": 0, "command": "clippy", "lint_findings": 2, "compile_failed": false}),
            1000,
        );
        assert_eq!(warned["lint_findings"], 2);
        assert!(warned["summary"].as_str().unwrap().contains("2 findings"));
        let clean = summarize_run(
            &serde_json::json!({"exit_code": 0, "command": "clippy", "lint_findings": 0}),
            1000,
        );
        assert!(clean["summary"].as_str().unwrap().contains("clean"));
        let fmt = summarize_run(&serde_json::json!({"exit_code": 1, "command": "fmt"}), 1000);
        assert!(fmt["summary"].as_str().unwrap().contains("diff"));
    }

    use super::*;

    /// `SANDBOX_URL` is process-global and cargo runs these in parallel threads,
    /// so the tests that mutate it raced: each would clear the var while the
    /// other was mid-assertion, and `base_url_loses_trailing_slash` failed
    /// intermittently on an unrelated change. The comment claiming "single-
    /// threaded test, no other thread reads the environment" was simply untrue.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn input(value: Value) -> ExecuteCodeInput {
        serde_json::from_value(value).expect("valid input")
    }

    fn both() -> ExecSkill {
        ExecSkill::with_urls(
            Some("http://sandbox:8000".into()),
            Some("http://rust-sandbox:8000".into()),
        )
    }

    #[test]
    fn from_env_rejects_blank_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: serialised against the other env-mutating tests by ENV_LOCK.
        unsafe {
            std::env::set_var("SANDBOX_URL", "   ");
            std::env::remove_var("RUST_SANDBOX_URL");
        }
        assert!(ExecSkill::from_env().is_none());
        unsafe { std::env::remove_var("SANDBOX_URL") };
        assert!(ExecSkill::from_env().is_none());
    }

    #[test]
    fn base_url_loses_trailing_slash() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: serialised against the other env-mutating tests by ENV_LOCK.
        unsafe {
            std::env::set_var("SANDBOX_URL", "http://sandbox:8000/");
            std::env::remove_var("RUST_SANDBOX_URL");
        }
        let skill = ExecSkill::from_env().expect("skill should build");
        assert_eq!(skill.python_url.as_deref(), Some("http://sandbox:8000"));
        unsafe { std::env::remove_var("SANDBOX_URL") };
    }

    #[test]
    fn rust_sandbox_alone_registers_the_tool() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: serialised against the other env-mutating tests by ENV_LOCK.
        unsafe {
            std::env::remove_var("SANDBOX_URL");
            std::env::set_var("RUST_SANDBOX_URL", "http://rust-sandbox:8000/");
        }
        let skill = ExecSkill::from_env().expect("rust-only skill should build");
        assert_eq!(skill.languages(), vec!["rust"]);
        unsafe { std::env::remove_var("RUST_SANDBOX_URL") };
    }

    #[test]
    fn exposes_execute_code() {
        let skill = ExecSkill::new("http://sandbox:8000");
        let tools = skill.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "execute_code");
    }

    #[test]
    fn python_only_schema_offers_no_rust_fields() {
        let def = ExecSkill::new("http://sandbox:8000").execute_code_def();
        let props = &def.input_schema["properties"];
        assert_eq!(props["language"]["enum"], json!(["python"]));
        assert!(props.get("files").is_none());
        assert!(props.get("command").is_none());
        assert!(!def.description.contains("RUST"));
    }

    #[test]
    fn schema_offers_rust_only_when_configured() {
        let def = both().execute_code_def();
        let props = &def.input_schema["properties"];
        assert_eq!(props["language"]["enum"], json!(["python", "rust"]));
        assert!(props.get("files").is_some());
        assert!(def.description.contains("OFFLINE"));
    }

    #[test]
    fn language_defaults_to_python_for_backward_compat() {
        let (url, body, _) = both()
            .build_request(&input(json!({ "code": "print(1)" })))
            .unwrap();
        assert_eq!(url, "http://sandbox:8000/exec");
        assert_eq!(body["code"], "print(1)");
    }

    #[test]
    fn python_rejects_rust_only_fields() {
        let err = both()
            .build_request(&input(json!({ "code": "print(1)", "command": "test" })))
            .unwrap_err();
        assert!(err.contains("Rust-only"));
    }

    #[test]
    fn rust_files_route_to_rust_sandbox() {
        let (url, body, timeout) = both()
            .build_request(&input(json!({
                "language": "rust",
                "files": { "src/lib.rs": "pub fn f() {}" },
                "command": "test",
                "include_ignored": true
            })))
            .unwrap();
        assert_eq!(url, "http://rust-sandbox:8000/exec");
        assert_eq!(body["files"]["src/lib.rs"], "pub fn f() {}");
        assert_eq!(body["command"], "test");
        assert_eq!(body["include_ignored"], true);
        assert_eq!(timeout, RUST_DEFAULT_TIMEOUT_MS);
        assert!(body.get("code").is_none());
    }

    #[test]
    fn rust_rejects_code_and_files_together_and_unknown_commands() {
        let skill = both();
        let err = skill
            .build_request(&input(json!({
                "language": "rust", "code": "fn main(){}", "files": { "src/main.rs": "" }
            })))
            .unwrap_err();
        assert!(err.contains("not both"));
        let err = skill
            .build_request(&input(json!({
                "language": "rust", "code": "fn main(){}", "command": "publish"
            })))
            .unwrap_err();
        assert!(err.contains("command"));
    }

    #[test]
    fn unconfigured_language_names_the_missing_env_var() {
        let err = ExecSkill::new("http://sandbox:8000")
            .build_request(&input(json!({ "language": "rust", "code": "fn main(){}" })))
            .unwrap_err();
        assert!(err.contains("RUST_SANDBOX_URL"));
    }

    #[test]
    fn rust_timeout_is_clamped_to_its_own_ceiling() {
        let (_, body, timeout) = both()
            .build_request(&input(json!({
                "language": "rust", "code": "fn main(){}", "timeout_ms": 10_000_000
            })))
            .unwrap();
        assert_eq!(timeout, RUST_MAX_TIMEOUT_MS);
        assert_eq!(body["timeout_ms"], RUST_MAX_TIMEOUT_MS);
    }

    #[test]
    fn failing_tests_are_a_successful_call_with_counts() {
        let result = summarize_run(
            &json!({
                "exit_code": 101, "timed_out": false, "stdout": "", "stderr": "",
                "command": "test", "tests_passed": 3, "tests_failed": 2, "compile_failed": false
            }),
            1000,
        );
        assert_eq!(result["success"], false);
        assert_eq!(result["tests_failed"], 2);
        assert!(result["summary"].as_str().unwrap().contains("2 failed"));
    }

    #[test]
    fn compile_failure_is_named_in_the_summary() {
        let result = summarize_run(
            &json!({ "exit_code": 101, "compile_failed": true, "tests_passed": null }),
            1000,
        );
        assert!(
            result["summary"]
                .as_str()
                .unwrap()
                .contains("Compilation failed")
        );
        assert!(result.get("tests_passed").is_none());
    }

    #[test]
    fn python_result_shape_is_unchanged() {
        let result = summarize_run(&json!({ "exit_code": 0, "stdout": "1\n" }), 1000);
        assert_eq!(result["summary"], "Execution succeeded.");
        assert!(result.get("command").is_none());
        assert!(result.get("compile_failed").is_none());
    }
}
