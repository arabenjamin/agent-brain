//! `manage_program` (PROGRAMS_PLAN.md Phase 1): the brain's programs —
//! departments, projects, investigations, cases and shared services — as a
//! tool. The rules are `services/programs.rs`, shared with REST.

use async_trait::async_trait;
use serde_json::{Value, json};

use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;
use agent_brain_work::programs::{self, PROGRAM_KINDS, PROGRAM_STATUSES};

pub struct ProgramSkill {
    neo4j: Neo4jClient,
}

impl ProgramSkill {
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_program".to_string(),
            description: "Programs organize the brain's work: a department (standing work), \
                project (a deliverable), investigation or case (a question that closes with a \
                finding), or services (on-demand workflows every program uses). Programs nest \
                (`parent`); each owns workflows (one program per workflow), names the topics \
                it covers and the agents and APIs it relies on, and has a charter and success \
                criteria. USE THIS to answer how a line of work is organized or doing. \
                action=list: the tree, with each program's issues and the enabled workflows in \
                no program. action=get: one program in full (`name`): workflows' state, \
                children, open todos. action=upsert: create or edit `name`; keys left out keep \
                their values, null resets one. A paused or closed program pauses its workflows \
                (except `keep_running`) and active again resumes exactly those. Closing an \
                investigation or case needs a `finding`; a closed program is a cold case the \
                owner reopens. Editing a seeded program makes it runtime-owned. \
                action=delete: remove `name` (refused while programs sit under it). \
                action=reset: put a seeded program's file back. action=audit: only what is \
                wrong (issues, unassigned workflows)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "upsert", "delete", "reset", "audit"] },
                    "name": { "type": "string", "description": "The program's slug." },
                    "label": { "type": "string" },
                    "kind": { "type": "string", "enum": PROGRAM_KINDS },
                    "parent": { "type": ["string", "null"], "description": "The program above it; null = top level." },
                    "status": { "type": "string", "enum": PROGRAM_STATUSES },
                    "charter": { "type": "string", "description": "What it is for, in a few sentences." },
                    "success": { "type": "string", "description": "What good output looks like." },
                    "topics": { "type": "array", "items": { "type": "string" } },
                    "workflows": { "type": "array", "items": { "type": "string" }, "description": "The whole list (it replaces the stored one)." },
                    "keep_running": { "type": "array", "items": { "type": "string" }, "description": "Its workflows a pause leaves on." },
                    "agents": { "type": "array", "items": { "type": "string" } },
                    "apis": { "type": "array", "items": { "type": "string" } },
                    "review": { "type": ["object", "null"], "properties": { "every": { "type": "string" } } },
                    "budget": { "type": ["object", "null"], "properties": { "monthly_usd": { "type": "number" } } },
                    "finding": { "type": ["string", "null"], "description": "What a closed investigation or case found." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn handle(&self, args: Value) -> ToolCallResult {
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let need_name = || ToolCallResult::error("`name` is required for this action".to_string());
        let saved = |r: Result<programs::Saved, programs::SaveError>| match r {
            Ok(s) => ToolCallResult::success_json(json!(s)),
            Err(e) => ToolCallResult::error(e.to_string()),
        };
        match args["action"].as_str().unwrap_or("") {
            "list" => match programs::tree(&self.neo4j).await {
                Ok(t) => ToolCallResult::success_json(t),
                Err(e) => ToolCallResult::error(format!("Failed to list programs: {e}")),
            },
            "audit" => match programs::tree(&self.neo4j).await {
                Ok(mut t) => {
                    let rows: Vec<Value> = t["programs"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| r["issues"].as_array().is_some_and(|i| !i.is_empty()))
                        .map(|r| json!({"name": r["name"], "issues": r["issues"]}))
                        .collect();
                    t["programs"] = json!(rows);
                    t["count"] = json!(rows.len());
                    ToolCallResult::success_json(t)
                }
                Err(e) => ToolCallResult::error(format!("Failed to audit programs: {e}")),
            },
            "get" => {
                let Some(name) = name else { return need_name() };
                match programs::detail(&self.neo4j, name).await {
                    Ok(Some(p)) => ToolCallResult::success_json(p),
                    Ok(None) => {
                        let names: Vec<String> = programs::load_all(&self.neo4j)
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|s| s.def.name)
                            .collect();
                        ToolCallResult::error(format!(
                            "No program '{name}'. Programs: {}",
                            names.join(", ")
                        ))
                    }
                    Err(e) => ToolCallResult::error(e.to_string()),
                }
            }
            "upsert" => {
                if name.is_none() {
                    return need_name();
                }
                saved(programs::save(&self.neo4j, &args, false).await)
            }
            "delete" => {
                let Some(name) = name else { return need_name() };
                match programs::delete(&self.neo4j, name).await {
                    Ok(changes) => {
                        ToolCallResult::success_json(json!({"deleted": name, "changes": changes}))
                    }
                    Err(e) => ToolCallResult::error(e.to_string()),
                }
            }
            "reset" => {
                let Some(name) = name else { return need_name() };
                let plugins = agent_brain_graph::bundles::Plugins::global();
                saved(programs::reset(&self.neo4j, &plugins, name).await)
            }
            other => ToolCallResult::error(format!(
                "Unknown action '{other}': use list, get, upsert, delete, reset or audit."
            )),
        }
    }
}

#[async_trait]
impl Skill for ProgramSkill {
    fn name(&self) -> &str {
        "program"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "manage_program" => Some(self.handle(arguments.unwrap_or_default()).await),
            _ => None,
        }
    }
}
