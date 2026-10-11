//! Running items assigned to agents (HBI plan Phase 13; `TODO_DELAYED_PROMPT_PLAN.md`
//! Phases 2–3).
//!
//! **An agent-assigned item is a Task with a delay.** When it is due, it gets
//! a Task (linked `(:Todo)-[:SPAWNED]->(:Task)`, so Work's lineage shows it)
//! owning a three-step chain: `run_agent` on the item's prompt →
//! `manage_todo deliver`, which stores the result as a note, posts it into the
//! item's thread and sets `awaiting_review` → `update_task completed`.
//!
//! Guardrails, all from the delayed-prompt plan:
//! - **asks first:** an agent that can act outside the graph
//!   (`items::side_effects`) waits in `awaiting_approval` until the user
//!   approves. An approval is single-use: it names the run number it allows
//!   (`approved_dispatch`), so it can never start a second run;
//! - **capped:** at most [`MAX_RUNS`] runs per item (sending back counts), and
//!   at most [`MAX_PER_POLL`] started per poll;
//! - **idempotent:** a run starts only through a conditional status change
//!   that also checks the run count, so two polls can't start the same run;
//! - **no recurrence:** an item runs when due and is then reviewed. Something
//!   that should recur becomes a schedule ("make it a schedule").
//!
//! A run that dies (its Task failed) goes back to `pending` while runs remain,
//! or to `awaiting_review` with the failure once they don't. Either way the
//! reason is posted in the thread.

use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::items::{self, SideEffects};
use agent_brain_repository::{Neo4jClient, Todo, TodoItemPatch};
use agent_brain_work::WorkingMemoryStore;
use agent_brain_work::context_builder::ContextBuilderService;
use agent_brain_work::queue::{ChainStep, QueueService};

/// Runs one item may have, sending back included.
pub const MAX_RUNS: i64 = 3;
/// Runs one poll may start; a cloud agent's cost is a multiplication.
pub const MAX_PER_POLL: usize = 3;
/// Characters of a result posted into the thread; the note holds all of it.
const THREAD_RESULT_CHARS: usize = 6_000;

/// What happened when an item was offered for a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Started {
        task_id: String,
        agent: String,
        run: i64,
    },
    AwaitingApproval {
        reason: String,
    },
    NotDue,
    NotReady(String),
}

impl Outcome {
    pub fn to_json(&self) -> Value {
        match self {
            Outcome::Started {
                task_id,
                agent,
                run,
            } => {
                json!({ "outcome": "started", "task_id": task_id, "agent": agent, "run": run })
            }
            Outcome::AwaitingApproval { reason } => {
                json!({ "outcome": "awaiting_approval", "reason": reason })
            }
            Outcome::NotDue => json!({ "outcome": "not_due" }),
            Outcome::NotReady(why) => json!({ "outcome": "not_ready", "reason": why }),
        }
    }
}

/// The agent an item is assigned to (`agent:<name>` → `<name>`).
pub fn assigned_agent(t: &Todo) -> Option<&str> {
    t.assignee
        .as_deref()?
        .strip_prefix("agent:")
        .filter(|a| !a.is_empty())
}

/// Whether an item is due: no due date, or its date is today or earlier
/// (local calendar date, as the Feed reads it).
pub fn is_due(t: &Todo, today: &str) -> bool {
    t.due_at
        .as_deref()
        .and_then(|d| d.get(..10))
        .is_none_or(|d| d <= today)
}

/// The agent's task. The agent sees nothing else, so it carries the item,
/// and on a rerun the previous answer and the user's comment on it.
pub fn run_prompt(t: &Todo, previous: Option<&str>) -> String {
    let mut out = match t.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => match t
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(d) => format!("{}\n\n{d}", t.title.trim()),
            None => t.title.trim().to_string(),
        },
    };
    if let Some(comment) = t
        .review_comment
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        if let Some(prev) = previous.map(str::trim).filter(|p| !p.is_empty()) {
            out.push_str("\n\n--- Your previous answer ---\n");
            out.push_str(prev);
        }
        out.push_str("\n\n--- The user sent it back with this comment ---\n");
        out.push_str(comment);
        out.push_str("\n\nAnswer again, taking the comment into account.");
    }
    out
}

/// The agent's own text, without the relay header `run_agent` adds for an
/// orchestrating model (the tool receipt and sources stay).
fn strip_relay_header(answer: &str) -> &str {
    let a = answer.trim_start();
    if a.starts_with("[FINDINGS FROM ")
        && let Some(end) = a.find("]\n")
    {
        return a[end + 2..].trim_start();
    }
    a
}

/// Post a line into an item's thread, giving it one (`todo-<id>`) if it has none.
pub async fn post_to_thread(neo4j: &Neo4jClient, t: &Todo, role: &str, text: &str) -> String {
    let session = match t.session_id.clone() {
        Some(s) => s,
        None => {
            let s = format!("todo-{}", t.id);
            let patch = TodoItemPatch {
                session_id: Some(Some(s.clone())),
                ..Default::default()
            };
            let _ = neo4j.patch_todo_item(&t.id, &patch).await;
            s
        }
    };
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    if let Err(e) = neo4j.push_entry(&id, &session, text, role, &ts).await {
        warn!(item = %t.id, error = %e, "Could not post to the item's thread");
    }
    session
}

/// Store an agent's result for an item and hand it to the user for review.
/// Only an item with a run in progress takes a delivery.
pub async fn deliver(
    neo4j: &Neo4jClient,
    knowledge: &dyn agent_brain_knowledge::KnowledgeStore,
    id: &str,
    result: &str,
) -> Result<Value, String> {
    let t = neo4j
        .get_todo(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No item with id '{id}'"))?;
    if t.status != "in_progress" {
        return Err(format!(
            "item '{}' is {}, not in progress: nothing is waiting for a result",
            t.title, t.status
        ));
    }
    let body = strip_relay_header(result);
    if body.trim().is_empty() {
        return Err("the result is empty".to_string());
    }
    // An answer the run's own checks flagged is stored as unsourced, never
    // as knowledge (the same rule `store_note` applies to run_agent output).
    let note_type = if body.starts_with("UNVERIFIED") {
        "unsourced_synthesis"
    } else {
        "semantic"
    };
    let content = format!("## {}\n\n{body}", t.title.trim());
    let (note_id, _) = knowledge
        .store_note(
            &content,
            Some(note_type),
            Some(&format!("item:{}", t.id)),
            None,
            Some(agent_brain_models::ProvenanceFlag::SynthesisInference),
        )
        .await
        .map_err(|e| format!("the result could not be stored: {e}"))?;
    neo4j
        .run(
            neo4rs::query(
                "MATCH (t:Todo {id: $id}) WHERE t.status = 'in_progress' \
                 SET t.status = 'awaiting_review', t.result_note_id = $note, \
                     t.review_comment = null, t.updated_at = datetime()",
            )
            .param("id", id)
            .param("note", note_id.as_str()),
        )
        .await
        .map_err(|e| e.to_string())?;
    let shown: String = body.chars().take(THREAD_RESULT_CHARS).collect();
    let more = if body.chars().count() > THREAD_RESULT_CHARS {
        format!("\n\n[… the full result is note {note_id}]")
    } else {
        String::new()
    };
    let session = post_to_thread(neo4j, &t, "assistant", &format!("{shown}{more}")).await;
    post_to_thread(
        neo4j,
        &t,
        "observation",
        "The result is waiting for your review: approve it in the Feed, or send it back with a comment.",
    )
    .await;
    info!(item = %id, note = %note_id, "Item result delivered for review");
    Ok(
        json!({ "delivered": true, "note_id": note_id, "session_id": session, "status": "awaiting_review" }),
    )
}

pub struct Dispatcher {
    neo4j: Neo4jClient,
    queue: Arc<QueueService>,
    context_builder: Option<Arc<ContextBuilderService>>,
}

impl Dispatcher {
    pub fn new(
        neo4j: Neo4jClient,
        queue: Arc<QueueService>,
        context_builder: Option<Arc<ContextBuilderService>>,
    ) -> Self {
        Self {
            neo4j,
            queue,
            context_builder,
        }
    }

    /// Offer one item for a run. `now` skips the due date (the user just
    /// handed it over or approved it).
    pub async fn dispatch(&self, t: &Todo, now: bool) -> Result<Outcome, String> {
        let Some(agent) = assigned_agent(t) else {
            return Ok(Outcome::NotReady("it is not assigned to an agent".into()));
        };
        if !matches!(t.status.as_str(), "pending" | "awaiting_approval") {
            return Ok(Outcome::NotReady(format!("it is {}", t.status)));
        }
        if t.dispatch_count >= MAX_RUNS {
            return Ok(Outcome::NotReady(format!(
                "it has run {} times, the most an item may",
                t.dispatch_count
            )));
        }
        if !now && !is_due(t, &agent_brain_graph::clock::today()) {
            return Ok(Outcome::NotDue);
        }
        let profile = match (&self.context_builder, agent) {
            (_, "auto") => None,
            (Some(cb), name) => match cb.get_profile(name).await {
                Some(p) if p.spawnable => Some(p),
                _ => {
                    return Ok(Outcome::NotReady(format!(
                        "'{name}' is not a spawnable agent"
                    )));
                }
            },
            (None, _) => return Ok(Outcome::NotReady("no agent roster is loaded".into())),
        };
        let run = t.dispatch_count + 1;
        let approved = t.approved_dispatch == Some(run);
        if let SideEffects::AsksFirst(reason) = items::side_effects(profile.as_ref())
            && !approved
        {
            if t.status == "pending" {
                self.neo4j
                    .run(
                        neo4rs::query(
                            "MATCH (t:Todo {id: $id}) WHERE t.status = 'pending' \
                             SET t.status = 'awaiting_approval', t.updated_at = datetime()",
                        )
                        .param("id", t.id.as_str()),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                post_to_thread(
                    &self.neo4j,
                    t,
                    "observation",
                    &format!(
                        "**{agent}** is assigned this and asks before it runs: {reason}. \
                         Approve it in the Feed to start run {run}. It will be told:\n\n{}",
                        run_prompt(t, None)
                    ),
                )
                .await;
            }
            return Ok(Outcome::AwaitingApproval { reason });
        }

        // Claim the run: only one caller can move it from this count.
        let claimed = self
            .neo4j
            .execute(
                neo4rs::query(
                    "MATCH (t:Todo {id: $id}) \
                     WHERE t.status IN ['pending', 'awaiting_approval'] \
                       AND coalesce(t.dispatch_count, 0) = $count \
                     SET t.status = 'in_progress', t.dispatched_at = datetime(), \
                         t.dispatch_count = $count + 1, t.updated_at = datetime() \
                     RETURN t.id AS id",
                )
                .param("id", t.id.as_str())
                .param("count", t.dispatch_count),
            )
            .await
            .map_err(|e| e.to_string())?;
        if claimed.is_empty() {
            return Ok(Outcome::NotReady(
                "another dispatch already started it".into(),
            ));
        }

        let previous = match &t.result_note_id {
            Some(nid) if t.review_comment.is_some() => self
                .neo4j
                .execute(
                    neo4rs::query("MATCH (n:Note {id: $id}) RETURN n.content AS c")
                        .param("id", nid.as_str()),
                )
                .await
                .ok()
                .and_then(|r| r.first().and_then(|r| r.get::<String>("c").ok())),
            _ => None,
        };
        let prompt = run_prompt(t, previous.as_deref());

        let started = self.start_run(t, agent, run, &prompt).await;
        match started {
            Ok(task_id) => {
                post_to_thread(
                    &self.neo4j,
                    t,
                    "observation",
                    &format!("Run {run} of {MAX_RUNS} started: **{agent}** is working on it (task {task_id})."),
                )
                .await;
                info!(item = %t.id, agent, run, task = %task_id, "Item run started");
                Ok(Outcome::Started {
                    task_id,
                    agent: agent.to_string(),
                    run,
                })
            }
            Err(e) => {
                // Give the claim back, so the item is not stuck in progress.
                let _ = self
                    .neo4j
                    .run(
                        neo4rs::query(
                            "MATCH (t:Todo {id: $id}) WHERE t.status = 'in_progress' \
                             SET t.status = 'pending', t.dispatch_count = $count, t.updated_at = datetime()",
                        )
                        .param("id", t.id.as_str())
                        .param("count", t.dispatch_count),
                    )
                    .await;
                Err(format!("the run could not be started: {e}"))
            }
        }
    }

    async fn start_run(
        &self,
        t: &Todo,
        agent: &str,
        run: i64,
        prompt: &str,
    ) -> Result<String, String> {
        let goal = format!("Item: {}", t.title.trim());
        let context = format!(
            "Run {run} of item '{}' (item: {}) by agent {agent}",
            t.title.trim(),
            t.id
        );
        let task_id = self
            .neo4j
            .create_task(&goal, Some(&context), None)
            .await
            .map_err(|e| e.to_string())?;
        // In progress before anything is queued, so the planner never takes it.
        let _ = self
            .neo4j
            .update_task_status(&task_id, agent_brain_models::TaskStatus::InProgress)
            .await;
        let _ = self
            .neo4j
            .run(
                neo4rs::query(
                    "MATCH (td:Todo {id: $todo}), (k:Task {id: $task}) \
                     MERGE (td)-[r:SPAWNED]->(k) SET r.created_at = datetime(), r.run = $run",
                )
                .param("todo", t.id.as_str())
                .param("task", task_id.as_str())
                .param("run", run),
            )
            .await;
        let mut agent_args = json!({ "agent": agent, "task": prompt });
        if let Some(m) = t.model.as_deref().filter(|m| !m.trim().is_empty()) {
            agent_args["model"] = json!(m);
        }
        let steps = vec![
            ChainStep {
                arguments: Some(agent_args),
                priority: Some(2),
                max_attempts: Some(2),
                provider_hint: Some("ollama".into()),
                description: Some(format!("Run {agent} on the item")),
                ..ChainStep::new("run_agent")
            },
            ChainStep {
                arguments: Some(json!({ "action": "deliver", "id": t.id, "result": "{{_prev}}" })),
                priority: Some(2),
                max_attempts: Some(3),
                provider_hint: Some("ollama".into()),
                description: Some("Store the result and hand it to the user for review".into()),
                ..ChainStep::new("manage_todo")
            },
            ChainStep {
                arguments: Some(json!({ "task_id": task_id, "status": "completed" })),
                priority: Some(1),
                max_attempts: Some(3),
                provider_hint: Some("ollama".into()),
                ..ChainStep::new("update_task")
            },
        ];
        self.queue
            .enqueue_chain_owned(&steps, None, Some(&task_id))
            .await?;
        Ok(task_id)
    }

    /// Runs whose Task failed go back to `pending` while runs remain, or to
    /// review with the failure once they don't. Returns what changed.
    pub async fn reconcile(&self) -> Vec<Value> {
        let rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (t:Todo {status: 'in_progress'})-[:SPAWNED]->(k:Task) \
                 WITH t, k ORDER BY k.created_at DESC \
                 WITH t, collect(k)[0] AS last \
                 WHERE last.status IN ['failed', 'cancelled'] \
                 RETURN t.id AS id, last.status AS status, last.context AS context",
            ))
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for r in &rows {
            let Ok(id) = r.get::<String>("id") else {
                continue;
            };
            let Ok(Some(t)) = self.neo4j.get_todo(&id).await else {
                continue;
            };
            let ctx = r.get::<String>("context").unwrap_or_default();
            let reason = ctx
                .rsplit("[FAILURE] ")
                .next()
                .filter(|_| ctx.contains("[FAILURE] "))
                .unwrap_or("the run was cancelled")
                .chars()
                .take(400)
                .collect::<String>();
            let (status, note) = if t.dispatch_count < MAX_RUNS {
                (
                    "pending",
                    format!(
                        "Run {} failed: {reason}\nIt will be tried again ({} of {MAX_RUNS} runs used).",
                        t.dispatch_count, t.dispatch_count
                    ),
                )
            } else {
                (
                    "awaiting_review",
                    format!(
                        "Run {} failed: {reason}\nThat was the last of {MAX_RUNS} runs; it waits for you to decide.",
                        t.dispatch_count
                    ),
                )
            };
            let _ = self
                .neo4j
                .run(
                    neo4rs::query(
                        "MATCH (t:Todo {id: $id}) WHERE t.status = 'in_progress' \
                         SET t.status = $status, t.updated_at = datetime()",
                    )
                    .param("id", id.as_str())
                    .param("status", status),
                )
                .await;
            post_to_thread(&self.neo4j, &t, "observation", &note).await;
            out.push(json!({ "id": id, "title": t.title, "now": status, "reason": reason }));
        }
        out
    }

    /// One poll: reconcile, then offer every due agent item, starting at
    /// most [`MAX_PER_POLL`] runs. The report names every item it touched.
    pub async fn poll(&self) -> Value {
        let reconciled = self.reconcile().await;
        let all = match self.neo4j.list_todos(None).await {
            Ok(a) => a,
            Err(e) => {
                return json!({ "error": format!("could not list items: {e}"), "reconciled": reconciled });
            }
        };
        let today = agent_brain_graph::clock::today();
        let mut due: Vec<&Todo> = all
            .iter()
            .filter(|t| assigned_agent(t).is_some() && t.status == "pending" && is_due(t, &today))
            .collect();
        due.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.created_at.cmp(&b.created_at))
        });
        let mut started = Vec::new();
        let mut asked = Vec::new();
        let mut held = Vec::new();
        for t in due {
            if started.len() >= MAX_PER_POLL {
                held.push(json!({ "id": t.id, "title": t.title, "reason": "this poll's limit was reached" }));
                continue;
            }
            match self.dispatch(t, false).await {
                Ok(Outcome::Started {
                    task_id,
                    agent,
                    run,
                }) => started.push(json!({
                    "id": t.id, "title": t.title, "agent": agent, "run": run, "task_id": task_id,
                })),
                Ok(Outcome::AwaitingApproval { reason }) => {
                    asked.push(json!({ "id": t.id, "title": t.title, "reason": reason }))
                }
                Ok(Outcome::NotDue) => {}
                Ok(Outcome::NotReady(why)) => {
                    held.push(json!({ "id": t.id, "title": t.title, "reason": why }))
                }
                Err(e) => held.push(json!({ "id": t.id, "title": t.title, "reason": e })),
            }
        }
        let not_due = all
            .iter()
            .filter(|t| assigned_agent(t).is_some() && t.status == "pending" && !is_due(t, &today))
            .count();
        json!({
            "answer": format!(
                "{} started, {} awaiting approval, {} held, {} not yet due, {} reconciled",
                started.len(), asked.len(), held.len(), not_due, reconciled.len()
            ),
            "started": started,
            "awaiting_approval": asked,
            "held": held,
            "not_due": not_due,
            "reconciled": reconciled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        prompt: Option<&str>,
        desc: Option<&str>,
        comment: Option<&str>,
        due: Option<&str>,
    ) -> Todo {
        serde_json::from_value(json!({
            "id": "i1", "title": "Research Sodium batteries", "description": desc, "status": "pending",
            "priority": 2, "tags": [], "due_at": due, "created_at": "x", "updated_at": "x",
            "assignee": "agent:researcher", "prompt": prompt, "review_comment": comment,
        }))
        .unwrap()
    }

    #[test]
    fn the_prompt_falls_back_and_carries_a_send_back() {
        assert_eq!(
            run_prompt(&item(None, None, None, None), None),
            "Research Sodium batteries"
        );
        assert_eq!(
            run_prompt(&item(None, Some("cost per kWh"), None, None), None),
            "Research Sodium batteries\n\ncost per kWh"
        );
        assert_eq!(
            run_prompt(
                &item(Some("Compare Na-ion to LFP"), Some("x"), None, None),
                None
            ),
            "Compare Na-ion to LFP"
        );
        let again = run_prompt(
            &item(None, None, Some("cite prices"), None),
            Some("Na-ion is cheap."),
        );
        assert!(
            again.contains("Na-ion is cheap.") && again.contains("cite prices"),
            "{again}"
        );
    }

    #[test]
    fn an_item_is_due_on_its_date_and_with_none() {
        assert!(is_due(&item(None, None, None, None), "2026-10-09"));
        assert!(is_due(
            &item(None, None, None, Some("2026-10-09T00:00:00Z")),
            "2026-10-09"
        ));
        assert!(!is_due(
            &item(None, None, None, Some("2026-10-10T00:00:00Z")),
            "2026-10-09"
        ));
        assert_eq!(
            assigned_agent(&item(None, None, None, None)),
            Some("researcher")
        );
    }

    #[test]
    fn the_relay_header_is_dropped_and_the_receipt_kept() {
        let a = "[FINDINGS FROM THE `researcher` SUB-AGENT — relay these faithfully.]\n\nNa-ion.\n\n[TOOLS `researcher` ACTUALLY RAN: search_web ×2.]";
        assert_eq!(
            strip_relay_header(a),
            "Na-ion.\n\n[TOOLS `researcher` ACTUALLY RAN: search_web ×2.]"
        );
        assert_eq!(strip_relay_header("plain"), "plain");
    }
}
