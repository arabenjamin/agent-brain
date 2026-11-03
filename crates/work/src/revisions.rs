//! Versioned self-edits (OUTSIDE_REVIEW_PLAN Phase 2): what every save of a
//! workflow, agent profile or program goes through. The storage is
//! `agent_brain_repository::revision`; this is the rules.
//!
//! A revision records a change of *definition*: what the object is, not its
//! state. Pausing a workflow or closing a program writes none, and needs no
//! reason. A change of definition needs a reason, and an edit names the version
//! it read (`based_on`), so an edit made on a stale read is refused rather than
//! silently undoing whatever came in between.
//!
//! The file a bundle ships is the `seed`; runtime saves stack on top. Each
//! owner node keeps `seed_definition` (the file now) and `drift` (how live
//! differs from it), which the `running_changes` view reads. The brain never
//! writes a repo file (§9.4): [`file_text`] renders what the file would say, the
//! owner writes, reviews and commits it, and the next start sees file and live
//! agree and hands the object back to the file.

use neo4rs::Query;
use serde_json::{Value, json};
use tracing::{info, warn};

use agent_brain_repository::{
    Appended, Neo4jClient, NewRevision, Revision, RevisionKind, canonical, chain_problems,
    content_hash,
};

/// Keys of a save body that are about the save, not the object.
pub const SAVE_KEYS: &[&str] = &["reason", "based_on", "reverts"];

/// Most of a diff kept on a node and shown by the view.
const MAX_DIFF_CHARS: usize = 6000;

/// Why a save was refused before anything was written.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// A change of definition with no reason given.
    NoReason(String),
    /// An edit that did not say which version it read.
    NoBase { message: String, current: i64 },
    /// The object moved since the version this edit read.
    Moved { message: String, current: i64 },
}

impl Refused {
    /// The version it is at now, when the refusal is about versions.
    pub fn current(&self) -> Option<i64> {
        match self {
            Refused::NoReason(_) => None,
            Refused::NoBase { current, .. } | Refused::Moved { current, .. } => Some(*current),
        }
    }

    /// The same save with the same read will fail again: re-read first.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Refused::Moved { .. })
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoReason(m) => write!(f, "{m}"),
            Refused::NoBase { message, .. } | Refused::Moved { message, .. } => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for Refused {}

/// A save that failed.
#[derive(Debug)]
pub enum CommitError {
    Refused(Refused),
    Storage(String),
}

impl From<agent_brain_repository::RepositoryError> for CommitError {
    fn from(e: agent_brain_repository::RepositoryError) -> Self {
        CommitError::Storage(e.to_string())
    }
}

/// What a save body says about the save itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaveRequest {
    pub reason: Option<String>,
    pub based_on: Option<i64>,
    /// The version this save copies back (a revert).
    pub reverts: Option<i64>,
}

impl SaveRequest {
    pub fn from_body(body: &Value) -> Self {
        Self {
            reason: body
                .get("reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            based_on: body.get("based_on").and_then(Value::as_i64),
            reverts: body.get("reverts").and_then(Value::as_i64),
        }
    }
}

/// A save body without [`SAVE_KEYS`].
pub fn without_save_keys(body: &Value) -> Value {
    let mut out = body.clone();
    if let Some(m) = out.as_object_mut() {
        for k in SAVE_KEYS {
            m.remove(*k);
        }
    }
    out
}

/// Whether a change of definition may be saved. `exists` = there is an object to
/// edit, at version `current`.
pub fn gate(
    kind: RevisionKind,
    name: &str,
    exists: bool,
    current: i64,
    req: &SaveRequest,
) -> Result<(), Refused> {
    let what = format!("{} '{name}'", kind.as_str());
    if req.reason.is_none() {
        return Err(Refused::NoReason(format!(
            "Not saved: changing {what} needs a `reason`, one line on why. It is kept with the revision."
        )));
    }
    if !exists {
        return Ok(());
    }
    match req.based_on {
        None => Err(Refused::NoBase {
            message: format!(
                "Not saved: {what} already exists, at version {current}. Read it, then save with \
                 `based_on`: the version you read."
            ),
            current,
        }),
        Some(read) if read != current => Err(Refused::Moved {
            message: moved_message(&what, read, current),
            current,
        }),
        Some(_) => Ok(()),
    }
}

fn moved_message(what: &str, read: i64, current: i64) -> String {
    format!(
        "Not saved: {what} changed since you read it (you read version {read}, it is at version \
         {current}). Read it again and redo the edit on top of that."
    )
}

/// Who is saving: the agent or chat turn whose model called the tool, the
/// workflow run a queued step belongs to, or the owner (the HBI, or a direct
/// MCP or REST call).
pub fn author() -> String {
    if let Some(agent) = agent_brain_llm::ctx::current_agent() {
        return format!("agent:{agent}");
    }
    if crate::validation::model_driven_call() {
        return "chat".to_string();
    }
    if let Some(task) = agent_brain_llm::ctx::current_owner_task() {
        return format!("workflow-run:{task}");
    }
    "owner".to_string()
}

/// A line diff of two texts, `-` and `+` with two lines of context. Long
/// results are cut and say so.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    let near = |k: usize| {
        let lo = k.saturating_sub(2);
        let hi = (k + 3).min(ops.len());
        ops[lo..hi].iter().any(|(c, _)| *c != ' ')
    };
    let mut out = String::new();
    let mut gap = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if near(k) {
            if gap && !out.is_empty() {
                out.push_str("…\n");
            }
            gap = false;
            out.push(*c);
            out.push(' ');
            out.push_str(line);
            out.push('\n');
        } else {
            gap = true;
        }
    }
    if out.chars().count() > MAX_DIFF_CHARS {
        let kept: String = out.chars().take(MAX_DIFF_CHARS).collect();
        let dropped = out.chars().count() - MAX_DIFF_CHARS;
        return format!("{kept}\n… cut, {dropped} more characters: read both revisions in full.");
    }
    out
}

fn pretty(canonical_json: &str) -> String {
    serde_json::from_str::<Value>(canonical_json)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| canonical_json.to_string())
}

/// How live differs from the file, for the owner node; `None` = it does not.
pub fn drift_of(seed: Option<&str>, live: &str) -> Option<String> {
    match seed {
        None => Some("No file: created at runtime.".to_string()),
        Some(s) if s == live => None,
        Some(s) => Some(diff(&pretty(s), &pretty(live))),
    }
}

async fn stored_seed(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
) -> Result<Option<String>, CommitError> {
    let cypher = format!(
        "MATCH (o:{} {{name: $name}}) RETURN o.seed_definition AS s",
        kind.label()
    );
    let rows = neo4j
        .execute(neo4rs::query(&cypher).param("name", name))
        .await?;
    Ok(rows.first().and_then(|r| r.get::<String>("s").ok()))
}

/// The write a save commits with its revision.
pub enum OwnerWrite {
    /// The object does not exist yet: runs before the revision hangs off it.
    Create(Query),
    /// It does: runs after the revision, once the version check has passed.
    Update(Query),
}

/// Commit a change of definition and its revision in one transaction. Refused
/// with nothing written when [`gate`] says so or another save got there first.
pub async fn commit(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    definition: &Value,
    req: &SaveRequest,
    write: OwnerWrite,
) -> Result<i64, CommitError> {
    let current = neo4j.live_version(kind, name).await?;
    let exists = matches!(write, OwnerWrite::Update(_));
    gate(kind, name, exists, current, req).map_err(CommitError::Refused)?;
    let (create, update) = match write {
        OwnerWrite::Create(q) => (vec![q], Vec::new()),
        OwnerWrite::Update(q) => (Vec::new(), vec![q]),
    };
    let text = canonical(definition);
    let drift = drift_of(stored_seed(neo4j, kind, name).await?.as_deref(), &text);
    let by = author();
    let appended = neo4j
        .append_revision(
            &NewRevision {
                kind,
                name,
                definition: &text,
                author: &by,
                reason: req.reason.as_deref(),
                based_on: req.based_on,
                op: if req.reverts.is_some() {
                    "revert"
                } else {
                    "save"
                },
            },
            current,
            drift.as_deref(),
            create,
            update,
        )
        .await?;
    match appended {
        Appended::Written(v) => Ok(v),
        Appended::Moved(now) => Err(CommitError::Refused(Refused::Moved {
            message: moved_message(&format!("{} '{name}'", kind.as_str()), current, now),
            current: now,
        })),
    }
}

/// Note a delete in the history, before the object goes. The history outlives
/// it, so [`revert_body`] can bring it back.
pub async fn record_delete(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    reason: Option<&str>,
) -> Result<(), CommitError> {
    let current = neo4j.live_version(kind, name).await?;
    let by = author();
    neo4j
        .append_revision(
            &NewRevision {
                kind,
                name,
                definition: "",
                author: &by,
                reason,
                based_on: Some(current),
                op: "delete",
            },
            current,
            None,
            Vec::new(),
            Vec::new(),
        )
        .await?;
    Ok(())
}

/// The seeder found this file: remember what it says on the node, and when the
/// file owns the object and says something new, add the `seed` revision. Called
/// for a runtime-owned object too, whose file then only shows up as drift.
pub async fn record_seed(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    definition: &Value,
    source: &str,
) -> anyhow::Result<()> {
    let text = canonical(definition);
    let hash = content_hash(&text);
    let cypher = format!(
        "MATCH (o:{} {{name: $name}}) \
         SET o.seed_definition = $text, o.seed_hash = $hash, o.seed_source = $source \
         RETURN coalesce(o.managed_by, 'yaml') AS managed_by, coalesce(o.revision, 0) AS revision",
        kind.label()
    );
    let rows = neo4j
        .execute(
            neo4rs::query(&cypher)
                .param("name", name)
                .param("text", text.as_str())
                .param("hash", hash.as_str())
                .param("source", source),
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    if row.get::<String>("managed_by").unwrap_or_default() != "yaml" {
        return Ok(());
    }
    let latest = neo4j.latest_revision(kind, name).await?;
    if latest.as_ref().is_some_and(|r| r.content_hash == hash) {
        return Ok(());
    }
    let by = format!("seed:{}", source.trim_start_matches("plugins/"));
    let appended = neo4j
        .append_revision(
            &NewRevision {
                kind,
                name,
                definition: &text,
                author: &by,
                reason: Some("The file says this."),
                based_on: None,
                op: "seed",
            },
            row.get("revision").unwrap_or(0),
            None,
            Vec::new(),
            Vec::new(),
        )
        .await?;
    if let Appended::Moved(now) = appended {
        warn!(kind = kind.as_str(), %name, now, "Seed revision lost to a concurrent save");
    }
    Ok(())
}

/// After a kind is seeded, for everything of that kind stored: give anything
/// with no history its first revision (the backfill); record a live definition
/// no revision describes (a write that went around the save path); hand back to
/// its file an object the file has caught up with (the owner committed it,
/// §9.4); refresh the drift the view reads; and report a broken chain.
pub async fn reconcile(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    live: &[(String, Value)],
) -> anyhow::Result<()> {
    let cypher = format!(
        "MATCH (o:{} {{name: $name}}) \
         RETURN coalesce(o.managed_by, 'yaml') AS managed_by, o.seed_definition AS seed, \
                o.seed_hash AS seed_hash, coalesce(o.revision, 0) AS revision",
        kind.label()
    );
    let (mut baselines, mut outside, mut adopted) = (0, 0, 0);
    for (name, definition) in live {
        let rows = neo4j
            .execute(neo4rs::query(&cypher).param("name", name.as_str()))
            .await?;
        let Some(row) = rows.first() else { continue };
        let managed_by: String = row.get("managed_by").unwrap_or_default();
        let seed: Option<String> = row.get("seed").ok();
        let text = canonical(definition);
        let hash = content_hash(&text);
        let latest = neo4j.latest_revision(kind, name).await?;
        let first_write = match &latest {
            None => Some((
                "baseline",
                "It existed before revisions were kept.".to_string(),
            )),
            Some(r) if r.content_hash != hash => {
                warn!(
                    kind = kind.as_str(), %name, version = r.version,
                    "Live definition matches no revision: it was changed around the save path"
                );
                Some((
                    "outside",
                    format!("Found on start: live differs from v{}.", r.version),
                ))
            }
            Some(_) => None,
        };
        if let Some((op, reason)) = first_write {
            let by = if op == "baseline" {
                "baseline".to_string()
            } else {
                "outside-save-path".to_string()
            };
            neo4j
                .append_revision(
                    &NewRevision {
                        kind,
                        name,
                        definition: &text,
                        author: &by,
                        reason: Some(&reason),
                        based_on: None,
                        op,
                    },
                    row.get("revision").unwrap_or(0),
                    None,
                    Vec::new(),
                    Vec::new(),
                )
                .await?;
            if op == "baseline" {
                baselines += 1;
            } else {
                outside += 1;
            }
        }
        let caught_up = managed_by == "runtime"
            && row.get::<String>("seed_hash").ok().as_deref() == Some(hash.as_str());
        if caught_up {
            adopted += 1;
            info!(
                kind = kind.as_str(), %name,
                "File and live agree: the file owns it again"
            );
        }
        let version = neo4j
            .latest_revision(kind, name)
            .await?
            .map_or(0, |r| r.version);
        let update = format!(
            "MATCH (o:{} {{name: $name}}) \
             SET o.revision = $version, o.live_hash = $hash, o.drift = $drift, \
                 o.managed_by = CASE WHEN $caught_up THEN 'yaml' ELSE o.managed_by END",
            kind.label()
        );
        let drift = drift_of(seed.as_deref(), &text);
        neo4j
            .run(
                neo4rs::query(&update)
                    .param("name", name.as_str())
                    .param("version", version)
                    .param("hash", hash.as_str())
                    .param(
                        "drift",
                        drift
                            .map(neo4rs::BoltType::from)
                            .unwrap_or(neo4rs::BoltType::Null(neo4rs::BoltNull)),
                    )
                    .param("caught_up", caught_up),
            )
            .await?;
    }
    if baselines + outside + adopted > 0 {
        info!(
            kind = kind.as_str(),
            baselines, outside, adopted, "Revisions reconciled"
        );
    }
    check_chains(neo4j, kind).await
}

/// Walk every history of a kind; a broken one is said loudly and marked on its
/// owner, where the view shows it.
async fn check_chains(neo4j: &Neo4jClient, kind: RevisionKind) -> anyhow::Result<()> {
    let all = neo4j.revisions_of_kind(kind).await?;
    let mut start = 0;
    while start < all.len() {
        let name = &all[start].name;
        let end = start + all[start..].iter().take_while(|r| &r.name == name).count();
        let problems = chain_problems(&all[start..end]);
        if !problems.is_empty() {
            warn!(kind = kind.as_str(), %name, problems = %problems.join("; "), "A revision history was edited");
        }
        let cypher = format!(
            "MATCH (o:{} {{name: $name}}) SET o.revision_problem = $problem",
            kind.label()
        );
        neo4j
            .run(neo4rs::query(&cypher).param("name", name.as_str()).param(
                "problem",
                if problems.is_empty() {
                    neo4rs::BoltType::Null(neo4rs::BoltNull)
                } else {
                    neo4rs::BoltType::from(problems.join("; "))
                },
            ))
            .await?;
        start = end;
    }
    Ok(())
}

/// One revision as a list shows it: everything but the definition.
pub fn summary(r: &Revision) -> Value {
    json!({
        "version": r.version,
        "op": r.op,
        "author": r.author,
        "reason": r.reason,
        "based_on": r.based_on,
        "at": r.at,
        "content_hash": r.content_hash,
    })
}

/// An object's history, newest first, with where it stands against its file.
pub async fn history(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    limit: Option<i64>,
) -> anyhow::Result<Value> {
    let revisions = neo4j.revisions(kind, name, limit).await?;
    let cypher = format!(
        "MATCH (o:{} {{name: $name}}) \
         RETURN coalesce(o.managed_by, 'yaml') AS managed_by, coalesce(o.revision, 0) AS version, \
                o.seed_source AS seed_source, o.drift AS drift, o.revision_problem AS problem",
        kind.label()
    );
    let rows = neo4j
        .execute(neo4rs::query(&cypher).param("name", name))
        .await?;
    let row = rows.first();
    let opt = |k: &str| row.and_then(|r| r.get::<String>(k).ok());
    Ok(json!({
        "kind": kind.as_str(),
        "name": name,
        "exists": row.is_some(),
        "version": row.and_then(|r| r.get::<i64>("version").ok()).unwrap_or(0),
        "managed_by": opt("managed_by"),
        "seed_source": opt("seed_source"),
        "drift": opt("drift"),
        "problem": opt("problem"),
        "revisions": revisions.iter().map(summary).collect::<Vec<_>>(),
    }))
}

/// One revision in full, with what it changed from the one before.
pub async fn revision_detail(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    version: i64,
) -> anyhow::Result<Option<Value>> {
    let Some(r) = neo4j.revision(kind, name, version).await? else {
        return Ok(None);
    };
    let prev = match version {
        1 => None,
        v => neo4j.revision(kind, name, v - 1).await?,
    };
    let mut out = summary(&r);
    out["definition"] = serde_json::from_str(&r.definition).unwrap_or(Value::Null);
    out["diff_from_previous"] = match &prev {
        Some(p) => json!(diff(&pretty(&p.definition), &pretty(&r.definition))),
        None => Value::Null,
    };
    Ok(Some(out))
}

/// The save body that puts an old revision back: its definition, with anything
/// live has that it lacks cleared. Goes through the kind's own save, so every
/// check runs and the revert is a new revision. `live` is `None` when the
/// object was deleted; it comes back paused, for the owner to look at first.
pub async fn revert_body(
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
    version: i64,
    reason: Option<&str>,
    based_on: Option<i64>,
    live: Option<&Value>,
) -> anyhow::Result<Value> {
    let Some(r) = neo4j.revision(kind, name, version).await? else {
        anyhow::bail!("{} '{name}' has no version {version}", kind.as_str());
    };
    if r.definition.is_empty() {
        anyhow::bail!(
            "version {version} of '{name}' is its delete, which has no definition; pick an earlier one"
        );
    }
    let mut body: Value = serde_json::from_str(&r.definition)?;
    if let (Some(b), Some(Value::Object(l))) = (body.as_object_mut(), live) {
        for (k, v) in l {
            if !b.contains_key(k) {
                b.insert(k.clone(), cleared(kind, v));
            }
        }
    }
    let why = reason.map(str::trim).filter(|s| !s.is_empty());
    body["reason"] = match why {
        Some(w) => json!(format!("Revert to v{version}: {w}")),
        None => Value::Null,
    };
    body["reverts"] = json!(version);
    body["based_on"] = json!(based_on);
    if live.is_none() && kind == RevisionKind::Workflow {
        body["enabled"] = json!(false);
    }
    Ok(body)
}

/// What clears a key a save body must name. A profile's lists and strings do not
/// take null; a program and a workflow read null as "back to the default".
fn cleared(kind: RevisionKind, live_value: &Value) -> Value {
    if kind != RevisionKind::Profile {
        return Value::Null;
    }
    match live_value {
        Value::Array(_) => json!([]),
        Value::Object(_) => json!({}),
        Value::String(_) => json!(""),
        Value::Bool(_) => json!(false),
        _ => Value::Null,
    }
}

/// What a bundle file for this definition would say. For the owner to review and
/// commit (§9.4), never written here. A comment in the file is not carried over;
/// the diff shows that.
pub fn file_text(
    kind: RevisionKind,
    definition: &Value,
    enabled: Option<bool>,
) -> anyhow::Result<String> {
    let mut v = definition.clone();
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, x| match x {
            Value::Null => false,
            Value::Array(a) => !a.is_empty(),
            Value::Bool(b) => *b || kind != RevisionKind::Workflow,
            _ => true,
        });
        if kind == RevisionKind::Workflow
            && let Some(e) = enabled
        {
            m.insert("enabled".into(), json!(e));
        }
    }
    Ok(serde_yaml::to_string(&v)?)
}

/// A bundle's file path for this object, for one that has no file yet.
pub fn suggested_path(kind: RevisionKind, name: &str) -> String {
    let dir = match kind {
        RevisionKind::Workflow => "workflows",
        RevisionKind::Profile => "contexts",
        RevisionKind::Program => "programs",
    };
    format!("plugins/core/{dir}/{name}.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(reason: Option<&str>, based_on: Option<i64>) -> SaveRequest {
        SaveRequest {
            reason: reason.map(String::from),
            based_on,
            reverts: None,
        }
    }

    #[test]
    fn a_change_needs_a_reason_and_an_edit_needs_the_version_it_read() {
        let k = RevisionKind::Workflow;
        assert!(matches!(
            gate(k, "w", false, 0, &req(None, None)),
            Err(Refused::NoReason(_))
        ));
        assert!(gate(k, "w", false, 0, &req(Some("new"), None)).is_ok());
        assert!(matches!(
            gate(k, "w", true, 3, &req(None, Some(3))),
            Err(Refused::NoReason(_))
        ));
        assert!(matches!(
            gate(k, "w", true, 3, &req(Some("x"), None)),
            Err(Refused::NoBase { current: 3, .. })
        ));
        let moved = gate(k, "w", true, 3, &req(Some("x"), Some(2))).unwrap_err();
        assert!(moved.is_conflict());
        assert_eq!(moved.current(), Some(3));
        assert!(moved.to_string().contains("version 3"));
        assert!(gate(k, "w", true, 3, &req(Some("x"), Some(3))).is_ok());
    }

    #[test]
    fn a_blank_reason_is_no_reason_and_save_keys_are_not_the_object() {
        let body = json!({"name": "w", "reason": "  ", "based_on": 2, "reverts": 1});
        let r = SaveRequest::from_body(&body);
        assert_eq!(r.reason, None);
        assert_eq!(r.based_on, Some(2));
        assert_eq!(r.reverts, Some(1));
        assert_eq!(without_save_keys(&body), json!({"name": "w"}));
    }

    #[test]
    fn diff_shows_what_changed_with_a_little_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let new = "a\nb\nc\nd\nX\nf\ng\nh\ni\n";
        let d = diff(old, new);
        assert!(d.contains("- e\n"), "{d}");
        assert!(d.contains("+ X\n"), "{d}");
        assert!(d.contains("  d\n"), "{d}");
        assert!(!d.contains("  a\n"), "{d}");
        assert_eq!(diff("same\n", "same\n"), "");
    }

    #[test]
    fn a_long_diff_says_it_was_cut() {
        let old = "x\n".repeat(10);
        let new = "y".repeat(MAX_DIFF_CHARS * 2);
        assert!(diff(&old, &new).contains("more characters"));
    }

    #[test]
    fn drift_is_none_only_when_live_is_the_file() {
        assert_eq!(drift_of(Some("{\"a\":1}"), "{\"a\":1}"), None);
        assert!(drift_of(None, "{}").unwrap().contains("No file"));
        let d = drift_of(Some("{\"a\":1}"), "{\"a\":2}").unwrap();
        assert!(d.contains("-") && d.contains("+"), "{d}");
    }

    #[test]
    fn a_rendered_file_leaves_out_what_a_file_would_not_say() {
        let t = file_text(
            RevisionKind::Workflow,
            &json!({"name": "w", "topic": null, "checks": [], "default": false, "steps": [{"tool_name": "x"}]}),
            Some(false),
        )
        .unwrap();
        assert!(t.contains("name: w"), "{t}");
        assert!(t.contains("enabled: false"), "{t}");
        assert!(
            !t.contains("topic") && !t.contains("checks") && !t.contains("default"),
            "{t}"
        );
    }

    #[test]
    fn clearing_a_key_fits_what_the_kind_accepts() {
        assert_eq!(cleared(RevisionKind::Program, &json!(["a"])), Value::Null);
        assert_eq!(cleared(RevisionKind::Profile, &json!(["a"])), json!([]));
        assert_eq!(cleared(RevisionKind::Profile, &json!("a")), json!(""));
    }
}
