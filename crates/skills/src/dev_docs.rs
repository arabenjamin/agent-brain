//! `dev_docs`: the brain's engineering history, subsystem rules and plan
//! decisions, queried from `(:DevNote)` (`services/dev_notes.rs`). For coding
//! agents and the self-analyst; not in the general profile.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use agent_brain_knowledge::dev_notes::{DevNote, DevNotes, KINDS};
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 30;

pub struct DevDocsSkill {
    notes: Arc<DevNotes>,
}

impl DevDocsSkill {
    pub fn new(notes: Arc<DevNotes>) -> Self {
        Self { notes }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "dev_docs".to_string(),
            description: "The brain's own developer documentation, kept in the graph: \
                subsystem RULES (what must hold when changing code), HISTORY (incidents and \
                measurements behind each rule), DECISIONS (finished plans) and PLANS (designed, \
                not built). Read before changing a subsystem. action=search: hybrid search by \
                meaning and keyword, optionally narrowed by `kind`, `subsystem` (substring; a former \
                doc name such as `subsystem-rules` or `HBI_REDESIGN_PLAN` also works) or \
                `path` (a file the note names, e.g. `services/queue.rs`); returns ids, titles and \
                snippets. action=get: one note in full by `id`. action=write: add or replace a \
                note (`kind`, `subsystem`, `title`, `content`, optional `paths`, `id`) — record \
                the WHY of a change here, not in a markdown file."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["search", "get", "write"] },
                    "query": { "type": "string", "description": "search: what you want to know." },
                    "kind": { "type": "string", "enum": KINDS },
                    "subsystem": { "type": "string",
                                   "description": "search: substring filter; write: the subsystem." },
                    "path": { "type": "string", "description": "search: a file path the note names." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT },
                    "id": { "type": "string", "description": "get: the note; write: replace it (optional)." },
                    "title": { "type": "string" },
                    "content": { "type": "string" },
                    "paths": { "type": "array", "items": { "type": "string" } }
                }
            }),
        }
    }

    async fn run(&self, a: &Value) -> Result<Value, String> {
        let s = |k: &str| a[k].as_str().map(str::trim).filter(|v| !v.is_empty());
        match s("action").unwrap_or("") {
            "search" => {
                let query = s("query").ok_or("search needs `query`")?;
                let limit = a["limit"]
                    .as_u64()
                    .map(|l| l as usize)
                    .unwrap_or(DEFAULT_LIMIT)
                    .clamp(1, MAX_LIMIT);
                let (hits, matched) = self
                    .notes
                    .search(query, s("kind"), s("subsystem"), s("path"), limit)
                    .await
                    .map_err(|e| format!("dev_docs search failed: {e}"))?;
                // Ranked, not counted: every query has candidates (the top 60 by meaning
                // plus the top 60 by keyword), so `candidates` is that pool after the
                // filters, best first, not a number of relevant notes.
                let mut out =
                    json!({ "candidates": matched, "returned": hits.len(), "notes": hits });
                if matched > limit {
                    out["hint"] = json!(
                        "Best-ranked first; more candidates rank below. Narrow by \
                         kind/subsystem/path, or raise limit."
                    );
                }
                if matched == 0 {
                    let total = self.notes.count().await.unwrap_or(0);
                    out["hint"] = json!(format!(
                        "No dev note matched these filters; {total} notes exist. Drop a filter \
                         (subsystem and path are substrings) or rephrase."
                    ));
                }
                Ok(out)
            }
            "get" => {
                let id = s("id").ok_or("get needs `id`")?;
                match self.notes.get(id).await.map_err(|e| e.to_string())? {
                    Some(n) => Ok(json!(n)),
                    None => Err(format!("No dev note with id '{id}'. Search to find ids.")),
                }
            }
            "write" => {
                let n = DevNote {
                    id: s("id").unwrap_or("").to_string(),
                    kind: s("kind").ok_or("write needs `kind`")?.to_string(),
                    subsystem: s("subsystem").unwrap_or("").to_string(),
                    title: s("title").ok_or("write needs `title`")?.to_string(),
                    content: a["content"].as_str().unwrap_or("").to_string(),
                    paths: a["paths"]
                        .as_array()
                        .map(|p| {
                            p.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    source: "dev_docs".into(),
                    date: String::new(),
                };
                let id = self.notes.write(n).await.map_err(|e| format!("{e:#}"))?;
                Ok(json!({ "id": id, "exported": agent_brain_knowledge::dev_notes::seed_file() }))
            }
            other => Err(format!(
                "Unknown action '{other}': use search, get or write."
            )),
        }
    }
}

#[async_trait]
impl Skill for DevDocsSkill {
    fn name(&self) -> &str {
        "Dev Docs"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "dev_docs" {
            return None;
        }
        let a = arguments.unwrap_or_default();
        Some(match self.run(&a).await {
            Ok(v) => ToolCallResult::success_json(v),
            Err(e) => ToolCallResult::error(e),
        })
    }
}
