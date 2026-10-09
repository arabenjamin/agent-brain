//! Typed relationships between entities (HBI plan §10.5, Phase 17).
//!
//! `(:Entity)-[:RELATES {type, method, status, evidence, …}]->(:Entity)`:
//! - **A closed vocabulary.** `relations/*.yaml` (`RELATIONS_DIR`) lists the
//!   types, each with a definition and the entity types allowed at each end.
//!   The user picks from it and the extractor may use nothing else.
//! - **The user's edges are final.** `method: 'manual'`, `status: 'active'`.
//!   Removing one deletes it.
//! - **Extracted edges are proposals.** `method: 'llm'`, `status: 'proposed'`
//!   until the user accepts (`active`) or rejects (`rejected`). A rejected
//!   edge is kept, so a later extraction never proposes it again. Measured
//!   before writing anything: on a fresh held-out set of 30 claims the cloud
//!   model's relations were right 8 of 9 times leniently and 5 of 9 strictly,
//!   and the local model's 2 of 8, so nothing it extracts goes live unreviewed.
//! - **Symmetric types** are stored once, from the entity with the lower id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::repository::Neo4jClient;
use crate::services::LlmProvider;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct RelType {
    pub name: String,
    pub label: String,
    /// How it reads from the other end.
    pub inverse: String,
    pub definition: String,
    /// Entity types allowed at each end (empty: any).
    #[serde(default)]
    pub from: Vec<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub symmetric: bool,
    /// False: only the user sets it.
    #[serde(default = "yes")]
    pub extract: bool,
}

fn yes() -> bool {
    true
}

impl RelType {
    /// Do these entity types fit its ends?
    pub fn fits(&self, from_type: &str, to_type: &str) -> bool {
        (self.from.is_empty() || self.from.iter().any(|t| t == from_type))
            && (self.to.is_empty() || self.to.iter().any(|t| t == to_type))
    }
}

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    types: Vec<RelType>,
}

#[derive(Debug, Default)]
pub struct Vocabulary {
    pub types: Vec<RelType>,
}

impl Vocabulary {
    /// Every `*.yaml` in `dir` with a `types:` list. A missing dir: no types.
    pub fn load_dir(dir: &Path) -> Self {
        let mut types: Vec<RelType> = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            warn!(dir = %dir.display(), "relations: no vocabulary dir — no relationship types");
            return Self::default();
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|t| serde_yaml::from_str::<File>(&t).map_err(Into::into))
            {
                Ok(f) => {
                    for t in f.types {
                        if t.name.trim().is_empty() || types.iter().any(|x| x.name == t.name) {
                            warn!(path = %p.display(), name = %t.name, "relations: type skipped (empty or repeated)");
                            continue;
                        }
                        types.push(t);
                    }
                }
                Err(e) => warn!(path = %p.display(), error = %e, "relations: file skipped"),
            }
        }
        Self { types }
    }

    pub fn global() -> &'static Vocabulary {
        static V: OnceLock<Vocabulary> = OnceLock::new();
        V.get_or_init(|| Vocabulary::load_dir(&relations_dir()))
    }

    pub fn get(&self, name: &str) -> Option<&RelType> {
        self.types.iter().find(|t| t.name == name)
    }

    pub fn extractable(&self) -> impl Iterator<Item = &RelType> {
        self.types.iter().filter(|t| t.extract)
    }
}

pub fn relations_dir() -> PathBuf {
    std::env::var("RELATIONS_DIR")
        .unwrap_or_else(|_| "relations".into())
        .into()
}

/// The stored direction: symmetric types go from the lower id.
pub fn ordered<'a>(t: &RelType, a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if t.symmetric && b < a { (b, a) } else { (a, b) }
}

// ── Extraction (pure parts) ──────────────────────────────────────────────

/// An entity a note mentions, as the extractor sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Mentioned {
    pub id: String,
    /// The canonical name (lowercase, as stored).
    pub name: String,
    /// The name as the note writes it.
    pub surface: String,
    pub entity_type: String,
}

/// One relation the model proposed, and whether it was kept.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub from: String,
    pub to: String,
    pub r#type: String,
    /// Why it was dropped, or `None` when it is proposed.
    pub dropped: Option<String>,
}

/// The extraction prompt: the claim, the entities it mentions, the types.
/// Its examples are the measured failure cases (HBI plan Phase 17 outcome).
pub fn prompt(vocab: &Vocabulary, text: &str, ents: &[Mentioned]) -> String {
    let types: String = vocab
        .extractable()
        .map(|t| {
            let f = if t.from.is_empty() {
                "any".into()
            } else {
                t.from.join("/")
            };
            let to = if t.to.is_empty() {
                "any".into()
            } else {
                t.to.join("/")
            };
            format!("- {}: {} ({f} → {to})\n", t.name, t.definition)
        })
        .collect();
    let list: String = ents
        .iter()
        .map(|e| format!("- {} ({})\n", e.surface, e.entity_type))
        .collect();
    format!(
        "Which relationships between the listed entities does the text state? Types:\n{types}\
         Examples:\n\
         - \"Mossad Director Roman Gofman\" states: Roman Gofman leads Mossad.\n\
         - \"Amanda Thursk is Prince Andrew's chief of staff\" states: Amanda Thursk works_for Prince Andrew.\n\
         - \"DeepMind's AlphaFold\" states: AlphaFold made_by DeepMind.\n\
         - \"Ted Cruz blocked a bill on AI safety\" states nothing between Ted Cruz and AI: he opposed a bill, which is not a listed entity.\n\
         - \"a Ford plant in Mexico\" states nothing about where Ford is: the plant is in Mexico, and the plant is not a listed entity.\n\
         - \"a C-SPAN forum in Ohio\" states nothing about where C-SPAN is.\n\
         - \"X refused to say whether he was a member of Y\" states nothing: a refused, alleged or denied relationship is not stated.\n\
         Entities (use these names exactly):\n{list}\
         Rules:\n\
         - Both ends must be the listed entities themselves, not an event, product, plant or report of theirs.\n\
         - Only what the text itself states; never outside knowledge.\n\
         - The direction reads \"<from> <type> <to>\".\n\
         - Output ONLY JSON: {{\"relations\":[{{\"from\":\"...\",\"type\":\"...\",\"to\":\"...\"}}]}}\n\
         - If the text states none: {{\"relations\":[]}}\n\nTEXT:\n{text}"
    )
}

/// Check the model's answer against the note's own entities and the
/// vocabulary. Every candidate is returned; the dropped ones say why.
pub fn validate(vocab: &Vocabulary, v: &Value, ents: &[Mentioned]) -> Vec<Candidate> {
    let find = |s: &str| {
        let s = s.trim().to_lowercase();
        ents.iter()
            .find(|e| e.surface.to_lowercase() == s)
            .or_else(|| ents.iter().find(|e| e.name == s))
    };
    let mut out: Vec<Candidate> = Vec::new();
    for r in v
        .get("relations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (f, t, ty) = (field("from"), field("to"), field("type"));
        let (fe, te) = (find(&f), find(&t));
        let rel = vocab.get(&ty);
        let dropped = match (fe, te, rel) {
            (None, _, _) | (_, None, _) => Some("not an entity the note mentions".to_string()),
            (Some(a), Some(b), _) if a.id == b.id => Some("an entity related to itself".into()),
            (_, _, None) => Some(format!("'{ty}' is not a relationship type")),
            (_, _, Some(rt)) if !rt.extract => Some(format!("'{ty}' is set only by the user")),
            (Some(a), Some(b), Some(rt)) if !rt.fits(&a.entity_type, &b.entity_type) => {
                Some(format!(
                    "{ty} does not join a {} to a {}",
                    a.entity_type, b.entity_type
                ))
            }
            _ => None,
        };
        let c = Candidate {
            from: fe.map(|e| e.id.clone()).unwrap_or(f),
            to: te.map(|e| e.id.clone()).unwrap_or(t),
            r#type: ty,
            dropped,
        };
        if !out
            .iter()
            .any(|o| o.from == c.from && o.to == c.to && o.r#type == c.r#type)
        {
            out.push(c);
        }
    }
    out
}

// ── Writes ───────────────────────────────────────────────────────────────

/// A refusal to show the user, or the database failing.
pub type Outcome<T> = Result<Result<T, String>>;

fn rel_type(name: &str) -> Result<&'static RelType, String> {
    Vocabulary::global().get(name).ok_or_else(|| {
        format!(
            "'{name}' is not a relationship type; the types are: {}",
            Vocabulary::global()
                .types
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Set a relationship by hand: active, and the user's. A proposed or
/// rejected extraction of the same edge becomes the user's.
pub async fn set_manual(
    neo4j: &Neo4jClient,
    from: &str,
    to: &str,
    type_name: &str,
    note_id: Option<&str>,
) -> Outcome<Value> {
    let t = match rel_type(type_name) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    if from == to {
        return Ok(Err("an entity can't be related to itself".into()));
    }
    let (a, b) = ordered(t, from, to);
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (a:Entity {id: $a}), (b:Entity {id: $b}) \
                 MERGE (a)-[r:RELATES {type: $type}]->(b) \
                 ON CREATE SET r.created_at = datetime(), r.evidence = [] \
                 SET r.method = 'manual', r.status = 'active', r.decided_at = datetime(), \
                     r.evidence = CASE WHEN $note = '' OR $note IN coalesce(r.evidence, []) \
                                  THEN coalesce(r.evidence, []) ELSE coalesce(r.evidence, []) + $note END \
                 RETURN a.name AS a, b.name AS b",
            )
            .param("a", a)
            .param("b", b)
            .param("type", type_name)
            .param("note", note_id.unwrap_or("")),
        )
        .await?;
    match rows.first() {
        Some(r) => Ok(Ok(json!({
            "from": a, "to": b, "type": type_name, "status": "active", "method": "manual",
            "reads": format!("{} {} {}", r.get::<String>("a").unwrap_or_default(), t.label,
                             r.get::<String>("b").unwrap_or_default()),
        }))),
        None => Ok(Err(format!("no entity '{from}' or '{to}'"))),
    }
}

/// Accept or reject a proposed (extracted) relationship.
pub async fn decide(
    neo4j: &Neo4jClient,
    from: &str,
    to: &str,
    type_name: &str,
    accept: bool,
) -> Outcome<Value> {
    let t = match rel_type(type_name) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let (a, b) = ordered(t, from, to);
    let status = if accept { "active" } else { "rejected" };
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (:Entity {id: $a})-[r:RELATES {type: $type}]->(:Entity {id: $b}) \
                 SET r.status = $status, r.decided_at = datetime() RETURN r.method AS method",
            )
            .param("a", a)
            .param("b", b)
            .param("type", type_name)
            .param("status", status),
        )
        .await?;
    if rows.is_empty() {
        return Ok(Err(format!(
            "no {type_name} relationship from '{a}' to '{b}'"
        )));
    }
    Ok(Ok(
        json!({ "from": a, "to": b, "type": type_name, "status": status }),
    ))
}

/// Remove a relationship. The user's own is deleted; an extracted one is
/// kept as `rejected`, so extraction never proposes it again.
pub async fn remove(neo4j: &Neo4jClient, from: &str, to: &str, type_name: &str) -> Outcome<Value> {
    let t = match rel_type(type_name) {
        Ok(t) => t,
        Err(e) => return Ok(Err(e)),
    };
    let (a, b) = ordered(t, from, to);
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (:Entity {id: $a})-[r:RELATES {type: $type}]->(:Entity {id: $b}) \
                 WITH r, r.method = 'llm' AS extracted \
                 FOREACH (_ IN CASE WHEN extracted THEN [1] ELSE [] END | \
                   SET r.status = 'rejected', r.decided_at = datetime()) \
                 FOREACH (_ IN CASE WHEN extracted THEN [] ELSE [1] END | DELETE r) \
                 RETURN extracted",
            )
            .param("a", a)
            .param("b", b)
            .param("type", type_name),
        )
        .await?;
    let Some(r) = rows.first() else {
        return Ok(Err(format!(
            "no {type_name} relationship from '{a}' to '{b}'"
        )));
    };
    let extracted = r.get::<bool>("extracted").unwrap_or(false);
    Ok(Ok(json!({
        "from": a, "to": b, "type": type_name,
        "removed": if extracted { "kept as rejected, so it is not proposed again" } else { "deleted" },
    })))
}

/// Change a relationship's type: the old one is removed (an extracted one
/// rejected), and the new one is the user's, with the old evidence.
pub async fn relabel(
    neo4j: &Neo4jClient,
    from: &str,
    to: &str,
    old: &str,
    new: &str,
) -> Outcome<Value> {
    let (t_old, t_new) = match (rel_type(old), rel_type(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
    };
    let (a, b) = ordered(t_old, from, to);
    let evidence: Vec<String> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (:Entity {id: $a})-[r:RELATES {type: $type}]->(:Entity {id: $b}) \
                 RETURN coalesce(r.evidence, []) AS ev",
            )
            .param("a", a)
            .param("b", b)
            .param("type", old),
        )
        .await?
        .first()
        .and_then(|r| r.get::<Vec<String>>("ev").ok())
        .unwrap_or_default();
    if let Err(e) = remove(neo4j, from, to, old).await? {
        return Ok(Err(e));
    }
    let (na, nb) = ordered(t_new, from, to);
    let out = set_manual(neo4j, na, nb, new, None).await?;
    if out.is_ok() && !evidence.is_empty() {
        neo4j
            .run(
                neo4rs::query(
                    "MATCH (:Entity {id: $a})-[r:RELATES {type: $type}]->(:Entity {id: $b}) \
                     SET r.evidence = coalesce(r.evidence, []) + \
                         [x IN $ev WHERE NOT x IN coalesce(r.evidence, [])]",
                )
                .param("a", na)
                .param("b", nb)
                .param("type", new)
                .param("ev", evidence),
            )
            .await?;
    }
    Ok(out)
}

/// Write one extracted relation as a proposal. Never touches the user's
/// edges or a rejected one, other than counting that it was seen again.
async fn propose(
    neo4j: &Neo4jClient,
    c: &Candidate,
    note_id: &str,
    model: &str,
) -> Result<&'static str> {
    let Some(t) = Vocabulary::global().get(&c.r#type) else {
        return Ok("skipped");
    };
    let (a, b) = ordered(t, &c.from, &c.to);
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (a:Entity {id: $a}), (b:Entity {id: $b}) \
                 MERGE (a)-[r:RELATES {type: $type}]->(b) \
                 ON CREATE SET r.method = 'llm', r.status = 'proposed', r.created_at = datetime(), \
                               r.evidence = [], r.model = $model \
                 WITH r, r.status AS status \
                 FOREACH (_ IN CASE WHEN status = 'rejected' THEN [1] ELSE [] END | \
                   SET r.seen_again = coalesce(r.seen_again, 0) + 1) \
                 FOREACH (_ IN CASE WHEN status <> 'rejected' AND NOT $note IN r.evidence THEN [1] ELSE [] END | \
                   SET r.evidence = r.evidence + $note) \
                 RETURN status",
            )
            .param("a", a)
            .param("b", b)
            .param("type", c.r#type.as_str())
            .param("note", note_id)
            .param("model", model),
        )
        .await?;
    Ok(
        match rows
            .first()
            .and_then(|r| r.get::<String>("status").ok())
            .as_deref()
        {
            Some("rejected") => "already rejected",
            Some("active") => "already set",
            Some(_) => "proposed",
            None => "skipped",
        },
    )
}

/// Extract relations from up to `limit` claims that have entities and have
/// not been read for relations, newest first; stamp each (`relations_at`).
/// `only` re-reads one claim, read before or not.
/// The model is whatever the caller's LLM routes to (the schedule pins one).
pub async fn extract_from_claims(
    neo4j: &Neo4jClient,
    llm: &dyn LlmProvider,
    limit: i64,
    only: Option<&str>,
) -> Result<Value> {
    let vocab = Vocabulary::global();
    if vocab.extractable().next().is_none() {
        return Ok(
            json!({ "claims": 0, "note": "no extractable relationship types (relations/*.yaml)" }),
        );
    }
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note {note_type: 'claim'}) \
                 WHERE (CASE WHEN $only = '' THEN n.entities_at IS NOT NULL AND n.relations_at IS NULL \
                        ELSE n.id = $only END) \
                   AND COUNT { (n)-[:MENTIONS]->(e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] } >= 2 \
                 WITH n ORDER BY n.created_at DESC LIMIT $limit \
                 RETURN n.id AS id, n.content AS content, \
                        [(n)-[m:MENTIONS]->(e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] | \
                          {id: e.id, name: e.name, surface: coalesce(m.surface, e.name), \
                           entity_type: coalesce(e.entity_type, 'concept')}] AS ents",
            )
            .param("limit", limit)
            .param("only", only.unwrap_or("")),
        )
        .await?;
    if let Some(id) = only
        && rows.is_empty()
    {
        return Ok(json!({
            "claims_read": 0,
            "note": format!("'{id}' is not a claim naming two or more entities (URLs and dates aside)"),
        }));
    }
    let waiting = neo4j
        .execute(neo4rs::query(
            "MATCH (n:Note {note_type: 'claim'}) \
             WHERE n.entities_at IS NOT NULL AND n.relations_at IS NULL \
               AND COUNT { (n)-[:MENTIONS]->(e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] } >= 2 \
             RETURN count(n) AS c",
        ))
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0);
    // The model the step pinned (`SharedLlm::model_name` says only "dynamic").
    let model = crate::services::queue::SELECTED_LLM
        .try_with(|v| v.as_ref().map(|c| c.model.clone()))
        .ok()
        .flatten()
        .unwrap_or_else(|| llm.model_name().to_string());
    let mut tally: HashMap<&'static str, i64> = HashMap::new();
    let mut dropped: HashMap<String, i64> = HashMap::new();
    let (mut read, mut failed) = (0i64, 0i64);
    for r in &rows {
        let (Ok(id), Ok(content)) = (r.get::<String>("id"), r.get::<String>("content")) else {
            continue;
        };
        let ents: Vec<Mentioned> = r
            .get::<Vec<neo4rs::BoltMap>>("ents")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                Some(Mentioned {
                    id: m.get::<String>("id").ok()?,
                    name: m.get::<String>("name").unwrap_or_default(),
                    surface: m.get::<String>("surface").unwrap_or_default(),
                    entity_type: m.get::<String>("entity_type").unwrap_or_default(),
                })
            })
            .collect();
        let answer = match llm
            .generate_json(&prompt(vocab, &content, &ents), None, &["relations"], 1)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(note_id = %id, error = %e, "relation extraction failed; the claim is retried next run");
                failed += 1;
                continue;
            }
        };
        let mut found = 0i64;
        for c in validate(vocab, &answer, &ents) {
            match &c.dropped {
                Some(why) => *dropped.entry(why.clone()).or_default() += 1,
                None => {
                    *tally
                        .entry(propose(neo4j, &c, &id, &model).await?)
                        .or_default() += 1;
                    found += 1;
                }
            }
        }
        neo4j
            .run(
                neo4rs::query(
                    "MATCH (n:Note {id: $id}) SET n.relations_at = datetime(), n.relations_found = $found",
                )
                .param("id", id.as_str())
                .param("found", found),
            )
            .await?;
        read += 1;
    }
    Ok(json!({
        "claims_read": read,
        "failed": failed,
        "waiting": (waiting - read).max(0),
        "proposed": tally.get("proposed").copied().unwrap_or(0),
        "already_set": tally.get("already set").copied().unwrap_or(0),
        "already_rejected": tally.get("already rejected").copied().unwrap_or(0),
        "dropped": dropped,
        "model": model,
        "note": "Extracted relationships wait for the user's review (Work → Feed).",
    }))
}

/// Fold the merge list's "related" answers (`RELATED_ENTITY`, Phase 4a) into
/// `related_to` relationships the user set. Run by `init-db`; idempotent.
pub async fn fold_related_entity(neo4j: &Neo4jClient) -> Result<i64> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (a:Entity)-[x:RELATED_ENTITY]-(b:Entity) WHERE a.id < b.id \
             MERGE (a)-[r:RELATES {type: 'related_to'}]->(b) \
             ON CREATE SET r.method = 'manual', r.status = 'active', r.evidence = [], \
                           r.created_at = coalesce(x.decided_at, datetime()), r.decided_at = x.decided_at, \
                           r.from_merge_review = true \
             RETURN count(r) AS c",
        ))
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocabulary {
        Vocabulary::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../relations"))
    }

    fn ents() -> Vec<Mentioned> {
        vec![
            Mentioned {
                id: "e1".into(),
                name: "roman gofman".into(),
                surface: "Roman Gofman".into(),
                entity_type: "person".into(),
            },
            Mentioned {
                id: "e2".into(),
                name: "mossad".into(),
                surface: "Mossad".into(),
                entity_type: "organisation".into(),
            },
            Mentioned {
                id: "e3".into(),
                name: "united arab emirates".into(),
                surface: "United Arab Emirates".into(),
                entity_type: "location".into(),
            },
        ]
    }

    #[test]
    fn the_shipped_vocabulary_loads_and_is_closed() {
        let v = vocab();
        assert!(v.types.len() >= 8);
        assert!(
            v.get("works_for")
                .is_some_and(|t| t.fits("person", "organisation"))
        );
        assert!(
            v.get("works_for")
                .is_some_and(|t| !t.fits("organisation", "person"))
        );
        assert!(
            v.get("related_to")
                .is_some_and(|t| !t.extract && t.symmetric)
        );
        assert!(
            v.types
                .iter()
                .all(|t| !t.label.is_empty() && !t.inverse.is_empty() && !t.definition.is_empty())
        );
    }

    #[test]
    fn validate_keeps_stated_relations_and_says_why_it_drops_the_rest() {
        let v = vocab();
        let answer = json!({"relations": [
            {"from": "Roman Gofman", "type": "leads", "to": "Mossad"},
            {"from": "roman gofman", "type": "leads", "to": "mossad"},
            {"from": "Mossad", "type": "works_for", "to": "Roman Gofman"},
            {"from": "Benjamin Netanyahu", "type": "leads", "to": "Mossad"},
            {"from": "Mossad", "type": "employs", "to": "Roman Gofman"},
            {"from": "Mossad", "type": "related_to", "to": "United Arab Emirates"},
            {"from": "Mossad", "type": "opposes", "to": "Mossad"}
        ]});
        let got = validate(&v, &answer, &ents());
        assert_eq!(
            got[0],
            Candidate {
                from: "e1".into(),
                to: "e2".into(),
                r#type: "leads".into(),
                dropped: None
            }
        );
        assert_eq!(
            got.iter()
                .filter(|c| c.r#type == "leads" && c.from == "e1")
                .count(),
            1,
            "repeats dropped"
        );
        let why = |i: usize| got[i].dropped.clone().unwrap_or_default();
        assert!(why(1).contains("does not join"), "{}", why(1));
        assert!(why(2).contains("not an entity the note mentions"));
        assert!(why(3).contains("not a relationship type"));
        assert!(why(4).contains("only by the user"));
        assert!(why(5).contains("itself"));
        assert!(validate(&v, &json!({"relations": []}), &ents()).is_empty());
        assert!(validate(&v, &json!({"other": 1}), &ents()).is_empty());
    }

    #[test]
    fn symmetric_types_are_stored_one_way() {
        let v = vocab();
        let allied = v.get("allied_with").unwrap();
        assert_eq!(ordered(allied, "b", "a"), ("a", "b"));
        let leads = v.get("leads").unwrap();
        assert_eq!(ordered(leads, "b", "a"), ("b", "a"), "direction kept");
    }

    #[test]
    fn the_prompt_lists_only_extractable_types_and_the_notes_entities() {
        let p = prompt(
            &vocab(),
            "Mossad Director Roman Gofman flew to the UAE.",
            &ents(),
        );
        assert!(p.contains("- works_for:"));
        assert!(
            !p.contains("- related_to:"),
            "user-only types are not offered"
        );
        assert!(p.contains("- Roman Gofman (person)"));
        assert!(p.ends_with("TEXT:\nMossad Director Roman Gofman flew to the UAE."));
    }
}
