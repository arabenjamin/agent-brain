//! `scaffold_plugin`: the brain drafts a new plugin bundle from the same
//! template the owner stamps with (`templates/plugin`, CODEBASE_SPLIT_PLAN
//! Phase 2 step 5), and offers it for review. It never writes the repo.
//!
//! - **Rendered in the offline Python sandbox.** cookiecutter evaluates every
//!   answer as a Jinja template, and the answers come from a model, so a
//!   description holding `{{ … }}` would run Python wherever it is rendered.
//!   The template is read from the codebase and sent to the sandbox (no
//!   network, no credentials) with the answers; the rendered files come back.
//!   Answers holding `{`, `}` or `%` are refused before that anyway.
//! - **Staged in the workspace, proposed in the Feed.** The preview and its
//!   `answers.json` go to `scaffolds/<name>/` in the workspace, and one Feed
//!   item asks the owner. Applying it is the owner's
//!   `scripts/plugin_template.sh new --answers …`, which re-renders from the
//!   same committed template, registers a Rust plugin with the host and runs
//!   the checks.
//! - **Refused early when it could not apply.** A bundle of that name, a tool
//!   name the brain already has (dispatch is first match), or an answer the
//!   template's own hook would reject.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use agent_brain_plugin_api::graph::query;
use agent_brain_plugin_api::protocol::tool_schema;
use agent_brain_plugin_api::{
    Feed, FeedItem, Graph, PluginContext, Skill, ToolCallResult, ToolDefinition, parse_args,
};

/// The sandbox's own wall clock for one render; cookiecutter takes about a
/// second.
const RENDER_TIMEOUT_MS: u64 = 60_000;
/// What the sandbox accepts as `code` (server.py's MAX_CODE_BYTES).
const MAX_PROGRAM_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
    kind: String,
    description: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    graph_labels: Vec<String>,
    #[serde(default)]
    needs_env: Vec<String>,
    why: String,
}

pub struct ScaffoldSkill {
    codebase_dir: PathBuf,
    workspace_dir: PathBuf,
    sandbox_url: String,
    graph: Arc<dyn Graph>,
    feed: Arc<dyn Feed>,
    client: reqwest::Client,
}

impl ScaffoldSkill {
    /// Offered only when it can work: a codebase to read the template from, a
    /// workspace to stage in, the Python sandbox, the graph and the Feed.
    pub fn new(ctx: &PluginContext) -> Option<Self> {
        let intro = ctx.introspection.clone()?;
        let sandbox_url = ctx
            .env("SANDBOX_URL")?
            .trim()
            .trim_end_matches('/')
            .to_string();
        if sandbox_url.is_empty() {
            return None;
        }
        Some(Self {
            codebase_dir: intro.codebase_dir()?,
            workspace_dir: intro.workspace_dir()?,
            sandbox_url,
            graph: ctx.graph.clone()?,
            feed: ctx.feed.clone()?,
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(RENDER_TIMEOUT_MS + 15_000))
                .build()
                .ok()?,
        })
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "scaffold_plugin".to_string(),
            description: "Draft a new plugin bundle from the brain's plugin template, for the \
                owner to review: renders it in the offline sandbox, stages the files under \
                scaffolds/<name>/ in the workspace, and files one Feed item with the command \
                that applies it. Changes nothing in the codebase. Use it when a proposal needs \
                a new bundle (a new API's tools, a new set of workflows); a data plugin holds \
                only seeds, a rust plugin also tools. Tools answer 'not built yet' until \
                written."
                .to_string(),
            input_schema: tool_schema!(plugin "scaffold_plugin"),
        }
    }

    async fn run(&self, args: Args) -> Result<String, String> {
        let answers = Answers::from_args(&args)?;
        if self
            .codebase_dir
            .join("plugins")
            .join(&answers.name)
            .exists()
        {
            return Err(format!("a bundle named '{}' already exists", answers.name));
        }
        let taken = self.taken_tools(&args.tools).await?;
        if !taken.is_empty() {
            return Err(format!(
                "tool name(s) already in the brain: {} (a tool name belongs to one skill)",
                taken.join(", ")
            ));
        }
        let template = read_template(&self.codebase_dir.join("templates/plugin"))?;
        let program = render_program(&template, &answers.context());
        if program.len() > MAX_PROGRAM_BYTES {
            return Err(format!(
                "the template is {} bytes as a program, over the sandbox's {MAX_PROGRAM_BYTES}",
                program.len()
            ));
        }
        let files = self.render(&program).await?;
        let dir = self.stage(&answers, &files)?;
        let item = self.propose(&answers, &args.why, &files, &dir).await?;
        info!(plugin = %answers.name, files = files.len(), %item, "scaffold_plugin: staged");
        Ok(format!(
            "Drafted plugin '{}' ({} files) in the workspace at {}, and asked the owner in \
             Feed item {item}. Nothing in the codebase changed; the owner applies it with:\n{}",
            answers.name,
            files.len(),
            dir.display(),
            answers.apply_command()
        ))
    }

    async fn taken_tools(&self, tools: &[String]) -> Result<Vec<String>, String> {
        if tools.is_empty() {
            return Ok(vec![]);
        }
        let rows = self
            .graph
            .execute(
                query("MATCH (d:ToolDef) WHERE d.name IN $names RETURN d.name AS name")
                    .param("names", tools),
            )
            .await
            .map_err(|e| format!("could not read the brain's tool names: {e}"))?;
        let mut taken: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get::<String>("name").ok())
            .collect();
        taken.sort();
        Ok(taken)
    }

    async fn render(&self, program: &str) -> Result<BTreeMap<String, String>, String> {
        let resp = self
            .client
            .post(format!("{}/exec", self.sandbox_url))
            .json(&json!({ "code": program, "timeout_ms": RENDER_TIMEOUT_MS }))
            .send()
            .await
            .map_err(|e| format!("the Python sandbox did not answer: {e}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("the sandbox's reply was not JSON ({status}): {e}"))?;
        parse_render(&body)
    }

    /// Write the preview and its answers under `scaffolds/<name>/`, replacing
    /// an earlier draft of the same name.
    fn stage(
        &self,
        answers: &Answers,
        files: &BTreeMap<String, String>,
    ) -> Result<PathBuf, String> {
        let dir = self.workspace_dir.join("scaffolds").join(&answers.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
        }
        let plugin = dir.join("plugin");
        for (rel, text) in files {
            let path = plugin.join(safe_relative(rel)?);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        }
        let answers_json =
            serde_json::to_string_pretty(&answers.context()).unwrap_or_default() + "\n";
        std::fs::write(dir.join("answers.json"), answers_json)
            .map_err(|e| format!("cannot write answers.json: {e}"))?;
        Ok(dir)
    }

    async fn propose(
        &self,
        answers: &Answers,
        why: &str,
        files: &BTreeMap<String, String>,
        dir: &Path,
    ) -> Result<String, String> {
        let listing: String = files
            .iter()
            .map(|(rel, text)| format!("- `{rel}` ({} lines)\n", text.lines().count()))
            .collect();
        let body = format!(
            "{why}\n\n\
             - **Plugin:** `{name}` ({kind})\n\
             - **Description:** {description}\n\
             - **Tools:** {tools}\n\
             - **Graph labels:** {labels}\n\
             - **Needs env:** {env}\n\n\
             ## Files\n\n{listing}\n\
             The preview is in the workspace at `{dir}`. Nothing in the codebase changed. \
             To apply it, from the repo root:\n\n```\n{command}\n```\n\n\
             That re-renders it from the committed template, registers a Rust plugin with the \
             host and the Dockerfile, and runs its tests and the repo checks; then enable it in \
             `PLUGINS_ENABLED`.",
            name = answers.name,
            kind = answers.kind,
            description = answers.description,
            tools = or_none(&answers.tools),
            labels = or_none(&answers.graph_labels),
            env = or_none(&answers.needs_env),
            dir = dir.display(),
            command = answers.apply_command(),
        );
        self.feed
            .create_item(FeedItem {
                title: format!("Review new plugin: {}", answers.name),
                body,
                priority: 2,
                origin_kind: "proposal".into(),
                origin_ref: format!("scaffold:{}", answers.name),
                ask: Some(
                    "Review the drafted plugin; apply it with the command in the item, or \
                     dismiss it. Nothing in the codebase changes until you run it."
                        .into(),
                ),
                source_task: None,
                source: None,
            })
            .await
            .map_err(|e| format!("the draft is staged, but the Feed item failed: {e}"))
    }
}

fn or_none(v: &[String]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

/// The checked answers, as the template takes them.
#[derive(Debug)]
struct Answers {
    name: String,
    kind: String,
    description: String,
    tools: Vec<String>,
    graph_labels: Vec<String>,
    needs_env: Vec<String>,
}

impl Answers {
    /// The template's pre-generation rules (`hooks/pre_gen_project.py`), and
    /// no `{`, `}` or `%` anywhere: cookiecutter renders each answer as Jinja.
    fn from_args(a: &Args) -> Result<Self, String> {
        let trim = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let answers = Self {
            name: a.name.trim().to_string(),
            kind: a.kind.trim().to_string(),
            description: a.description.trim().to_string(),
            tools: trim(&a.tools),
            graph_labels: trim(&a.graph_labels),
            needs_env: trim(&a.needs_env),
        };
        let mut problems = vec![];
        let re = |p: &str| Regex::new(p).expect("a valid pattern");
        if !re(r"^[a-z][a-z0-9]*(-[a-z0-9]+)*$").is_match(&answers.name) || answers.name.len() > 40
        {
            problems.push(format!(
                "name '{}': lowercase words joined by '-', at most 40 characters",
                answers.name
            ));
        }
        if answers.name == "core" {
            problems.push("name 'core' is the core bundle".into());
        }
        if answers.kind != "rust" && answers.kind != "data" {
            problems.push("kind: 'rust' or 'data'".into());
        }
        if answers.description.is_empty()
            || answers.description.len() > 300
            || answers.description.contains(['"', '\n'])
        {
            problems.push("description: one line, at most 300 characters, no double quotes".into());
        }
        let snake = re(r"^[a-z][a-z0-9]*(_[a-z0-9]+)*$");
        for t in &answers.tools {
            if !snake.is_match(t) {
                problems.push(format!("tool '{t}': snake_case"));
            }
        }
        let mut unique = answers.tools.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != answers.tools.len() {
            problems.push("tools: a name is given twice".into());
        }
        let pascal = re(r"^[A-Z][A-Za-z0-9]*$");
        for l in &answers.graph_labels {
            if !pascal.is_match(l) {
                problems.push(format!("graph label '{l}': PascalCase"));
            }
        }
        let upper = re(r"^[A-Z][A-Z0-9_]*$");
        for e in &answers.needs_env {
            if !upper.is_match(e) {
                problems.push(format!("env var '{e}': UPPER_SNAKE_CASE"));
            }
        }
        if answers.kind == "data" && (!answers.tools.is_empty() || !answers.graph_labels.is_empty())
        {
            problems
                .push("a data plugin has no crate: tools and graph labels need kind 'rust'".into());
        }
        let all = std::iter::once(&answers.name)
            .chain([&answers.kind, &answers.description])
            .chain(&answers.tools)
            .chain(&answers.graph_labels)
            .chain(&answers.needs_env);
        if all.into_iter().any(|s| s.contains(['{', '}', '%'])) {
            problems
                .push("no answer may hold '{', '}' or '%' (the template renders answers)".into());
        }
        if problems.is_empty() {
            Ok(answers)
        } else {
            Err(problems.join("; "))
        }
    }

    /// The template's context: lists as comma-separated strings.
    fn context(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "kind": self.kind,
            "tools": self.tools.join(","),
            "graph_labels": self.graph_labels.join(","),
            "needs_env": self.needs_env.join(","),
        })
    }

    fn apply_command(&self) -> String {
        format!(
            "scripts/plugin_template.sh new --answers workspace/scaffolds/{}/answers.json",
            self.name
        )
    }
}

/// Every file of the template, by path relative to it.
fn read_template(root: &Path) -> Result<BTreeMap<String, String>, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == "__pycache__") {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out)?;
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned();
                let text =
                    std::fs::read_to_string(&p).map_err(|e| format!("cannot read {rel}: {e}"))?;
                out.insert(rel, text);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    if !out.contains_key("cookiecutter.json") {
        return Err(format!("no plugin template at {}", root.display()));
    }
    Ok(out)
}

/// The program the sandbox runs: write the template, render it with the
/// answers, print the files as one JSON line. A JSON string is a valid Python
/// string literal, so both are embedded with `json.loads`.
fn render_program(template: &BTreeMap<String, String>, context: &Value) -> String {
    let files = serde_json::to_string(&serde_json::to_string(template).unwrap_or_default())
        .unwrap_or_default();
    let answers = serde_json::to_string(&serde_json::to_string(context).unwrap_or_default())
        .unwrap_or_default();
    format!(
        r#"import json, os, runpy, sys
from cookiecutter import hooks as cookiecutter_hooks
from cookiecutter.main import cookiecutter
from cookiecutter.exceptions import CookiecutterException, FailedHookException


# Run each rendered hook in this process, not as a subprocess, so the render
# never depends on the sandbox's process cap (RLIMIT_NPROC; before 2026-11-06
# the sandbox shared the host user's uid and could start no child at all).
# cookiecutter 2.7.1 calls run_script(path, cwd) for each hook.
def run_script_here(script_path, cwd="."):
    back = os.getcwd()
    os.chdir(cwd)
    try:
        runpy.run_path(script_path, run_name="__main__")
    except SystemExit as e:
        if e.code not in (0, None):
            raise FailedHookException("Hook script failed (exit status: %s)" % e.code)
    finally:
        os.chdir(back)


cookiecutter_hooks.run_script = run_script_here

FILES = json.loads({files})
ANSWERS = json.loads({answers})
here = os.getcwd()
tpl = os.path.join(here, "template")
for rel, text in FILES.items():
    path = os.path.join(tpl, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
try:
    made = cookiecutter(tpl, no_input=True, extra_context=ANSWERS, output_dir=os.path.join(here, "out"))
except CookiecutterException as e:
    print(json.dumps({{"ok": False, "error": type(e).__name__ + ": " + str(e)}}))
    sys.exit(0)
files = {{}}
for d, _, names in os.walk(made):
    for n in names:
        p = os.path.join(d, n)
        with open(p, encoding="utf-8") as f:
            files[os.path.relpath(p, made)] = f.read()
print(json.dumps({{"ok": True, "files": files}}))
"#
    )
}

/// The rendered files from the sandbox's reply, or what went wrong, said.
fn parse_render(body: &Value) -> Result<BTreeMap<String, String>, String> {
    if let Some(e) = body.get("error").and_then(Value::as_str) {
        return Err(format!("the sandbox refused the render: {e}"));
    }
    let stderr = body
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if body.get("timed_out").and_then(Value::as_bool) == Some(true) {
        return Err("the render timed out in the sandbox".into());
    }
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        return Err("the rendered plugin is larger than the sandbox returns (64 KB)".into());
    }
    let stdout = body.get("stdout").and_then(Value::as_str).unwrap_or("");
    let Some(line) = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
    else {
        return Err(format!(
            "the render printed no result (exit {}): {}",
            body.get("exit_code").cloned().unwrap_or(Value::Null),
            if stderr.is_empty() {
                "no error output"
            } else {
                stderr
            }
        ));
    };
    let out: Value =
        serde_json::from_str(line).map_err(|e| format!("the render's result was not JSON: {e}"))?;
    if out.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = out
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        // The template's hook says why on its own line; the traceback after it
        // is cookiecutter's.
        let why = stderr
            .lines()
            .find(|l| l.starts_with("templates/plugin:"))
            .or_else(|| stderr.lines().last())
            .unwrap_or("");
        return Err(if why.is_empty() {
            format!("the template refused the answers: {error}")
        } else {
            format!("the template refused the answers: {error}; {why}")
        });
    }
    let files: BTreeMap<String, String> = out
        .get("files")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return Err("the render produced no files".into());
    }
    Ok(files)
}

/// A rendered path, kept inside the staging directory.
fn safe_relative(rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("the render produced an unsafe path: {rel:?}"));
    }
    Ok(p.to_path_buf())
}

#[async_trait]
impl Skill for ScaffoldSkill {
    fn name(&self) -> &str {
        "Plugin Scaffold"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "scaffold_plugin" {
            return None;
        }
        let args: Args = match parse_args(arguments) {
            Ok(a) => a,
            Err(e) => return Some(e),
        };
        Some(match self.run(args).await {
            Ok(text) => ToolCallResult::success_text(text),
            Err(e) => ToolCallResult::error(format!("scaffold_plugin: {e}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(name: &str, kind: &str, description: &str, tools: &[&str]) -> Args {
        Args {
            name: name.into(),
            kind: kind.into(),
            description: description.into(),
            tools: tools.iter().map(ToString::to_string).collect(),
            graph_labels: vec![],
            needs_env: vec![],
            why: "test".into(),
        }
    }

    #[test]
    fn answers_follow_the_template_rules() {
        assert!(
            Answers::from_args(&args("rail-watch", "rust", "Trains.", &["rail_status"])).is_ok()
        );
        let bad = Answers::from_args(&args("Rail_Watch", "rust", "Trains.", &["RailStatus"]))
            .unwrap_err();
        assert!(bad.contains("name") && bad.contains("snake_case"), "{bad}");
        assert!(
            Answers::from_args(&args("x", "data", "d", &["a_tool"]))
                .unwrap_err()
                .contains("data plugin")
        );
        assert!(Answers::from_args(&args("core", "data", "d", &[])).is_err());
        assert!(
            Answers::from_args(&args("x", "rust", "d", &["a", "a"]))
                .unwrap_err()
                .contains("twice")
        );
    }

    #[test]
    fn answers_that_jinja_would_evaluate_are_refused() {
        for d in ["{{ ''.__class__ }}", "50% off", "a {% if x %}"] {
            let e = Answers::from_args(&args("x", "data", d, &[])).unwrap_err();
            assert!(e.contains("'{', '}' or '%'"), "{d}: {e}");
        }
    }

    #[test]
    fn the_shipped_template_fits_the_sandbox_as_a_program() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../templates/plugin");
        let template = read_template(&root).unwrap();
        assert!(template.contains_key("hooks/pre_gen_project.py"));
        assert!(template.keys().any(|k| k.ends_with("crate/src/lib.rs")));
        let a =
            Answers::from_args(&args("rail-watch", "rust", "Trains.", &["rail_status"])).unwrap();
        let program = render_program(&template, &a.context());
        assert!(program.len() < MAX_PROGRAM_BYTES, "{} bytes", program.len());
        assert!(program.contains("from cookiecutter.main import cookiecutter"));
    }

    #[test]
    fn the_sandbox_reply_is_read_or_its_failure_said() {
        let ok = json!({"stdout": "{\"ok\": true, \"files\": {\"plugin.yaml\": \"name: x\\n\"}}\n", "stderr": "", "exit_code": 0});
        assert_eq!(parse_render(&ok).unwrap()["plugin.yaml"], "name: x\n");
        let refused = json!({"stdout": "{\"ok\": false, \"error\": \"FailedHookException: Hook script failed\"}\n",
            "stderr": "templates/plugin: name 'X': lowercase\nTraceback (most recent call last):\n  boom", "exit_code": 0});
        let e = parse_render(&refused).unwrap_err();
        assert!(
            e.contains("FailedHookException") && e.contains("lowercase"),
            "{e}"
        );
        assert!(!e.contains("Traceback"), "{e}");
        let crashed = json!({"stdout": "", "stderr": "ModuleNotFoundError: No module named 'cookiecutter'", "exit_code": 1});
        assert!(
            parse_render(&crashed)
                .unwrap_err()
                .contains("ModuleNotFoundError")
        );
        assert!(parse_render(&json!({"error": "too large"})).is_err());
        assert!(
            parse_render(&json!({"stdout": "x", "truncated": true}))
                .unwrap_err()
                .contains("64 KB")
        );
    }

    #[test]
    fn rendered_paths_stay_inside_the_staging_directory() {
        assert!(safe_relative("crate/src/lib.rs").is_ok());
        for bad in ["../x", "/etc/passwd", "a/../../b", ""] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
    }
}
