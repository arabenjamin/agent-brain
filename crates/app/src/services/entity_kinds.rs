//! What kind of thing an entity is (HBI plan Phase 19, §12.2).
//!
//! - **Two levels.** `entity_type` is the coarse class the extractor gives
//!   (location, organisation, …); `kind` is the fine one (country,
//!   municipality, news outlet, …), derived from the entity's reference link.
//! - **Derived, not judged.** A linked Wikidata item's instance-of classes are
//!   walked up subclass-of to the roots in `entities/kinds.yaml`; the root
//!   reached in the fewest hops wins, and a `fallback` kind only when nothing
//!   specific is reachable. Measured on 228 linked entities (§12.1): matching
//!   in list order made YouTube an event and every U.S. state a country.
//! - **Precedence:** the user's kind, then the link's kind when its coarse
//!   type agrees with the filed one. A disagreement is a question for the
//!   user (`type_question_qid`), never a retype: a quarter of the measured
//!   disagreements came from wrong links.
//! - **Classes are cached** as `(:WikiClass {qid, label, parents})`, fetched
//!   through the Wikimedia gate in `wiki.rs`, so later walks cost little.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::Neo4jClient;
use crate::services::wiki;

const FILE: &str = "kinds.yaml";
/// Linked items derived per refresh (all 228 of today's fit in one).
const DERIVE_BATCH: i64 = 400;

#[derive(Debug, Deserialize)]
struct KindsFile {
    #[serde(default = "default_hops")]
    max_hops: usize,
    #[serde(default)]
    entity_types: Vec<String>,
    #[serde(default)]
    refuse: Vec<String>,
    #[serde(default)]
    kinds: Vec<KindDef>,
}

fn default_hops() -> usize {
    7
}

#[derive(Debug, Deserialize)]
struct KindDef {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    roots: Vec<String>,
    #[serde(default)]
    fallback: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Kind {
    pub name: String,
    pub entity_type: String,
    pub fallback: bool,
}

/// `entities/kinds.yaml`, parsed.
#[derive(Debug)]
pub struct KindTable {
    pub max_hops: usize,
    pub entity_types: Vec<String>,
    pub kinds: Vec<Kind>,
    refuse: HashSet<String>,
    /// Root class → index into `kinds` (the first kind to name it).
    roots: HashMap<String, usize>,
    /// A hash of the file: changing it re-derives every kind.
    pub version: String,
}

/// What a walk found for one item.
#[derive(Debug, PartialEq)]
pub enum Walk {
    /// Index into `kinds`, and the hops it took.
    Kind(usize, usize),
    /// A Wikimedia page (disambiguation, list, set index): never a reference.
    Refused,
    /// No root within `max_hops`, or no classes at all (a kind of thing).
    Unmapped,
    /// A class on the way has not been fetched yet.
    Incomplete,
}

impl KindTable {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let f: KindsFile = serde_yaml::from_str(text).context("kinds.yaml is not valid")?;
        let mut roots = HashMap::new();
        for (i, k) in f.kinds.iter().enumerate() {
            for r in &k.roots {
                roots.entry(r.clone()).or_insert(i);
            }
        }
        let mut h = crate::services::topics::Fnv::default();
        h.write(text.as_bytes());
        Ok(Self {
            max_hops: f.max_hops,
            entity_types: f.entity_types,
            kinds: f
                .kinds
                .into_iter()
                .map(|k| Kind {
                    name: k.name,
                    entity_type: k.entity_type,
                    fallback: k.fallback,
                })
                .collect(),
            refuse: f.refuse.into_iter().collect(),
            roots,
            version: format!("{:016x}", h.0),
        })
    }

    /// `kinds.yaml` beside the alias seed (`ENTITY_ALIASES_DIR`).
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::services::entity_resolution::aliases_dir())
    }

    pub fn load_from(dir: &Path) -> Result<Self> {
        let p = dir.join(FILE);
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        Self::from_yaml(&text)
    }

    /// Walk up from `classes` (an item's instance-of ids) one layer at a time.
    /// A refused root anywhere wins; otherwise the nearest specific root
    /// (ties: table order); otherwise the nearest fallback.
    pub fn walk(&self, classes: &[String], parents: &HashMap<String, Vec<String>>) -> Walk {
        if classes.is_empty() {
            return Walk::Unmapped;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut layer: Vec<&str> = classes.iter().map(String::as_str).collect();
        let mut fallback: Option<(usize, usize)> = None;
        for hop in 0..=self.max_hops {
            if layer.iter().any(|c| self.refuse.contains(*c)) {
                return Walk::Refused;
            }
            let hits: Vec<usize> = layer
                .iter()
                .filter_map(|c| self.roots.get(*c).copied())
                .collect();
            if let Some(&i) = hits.iter().filter(|&&i| !self.kinds[i].fallback).min() {
                return Walk::Kind(i, hop);
            }
            if fallback.is_none()
                && let Some(&i) = hits.iter().min()
            {
                fallback = Some((i, hop));
            }
            if hop == self.max_hops {
                break;
            }
            let mut next = Vec::new();
            for c in &layer {
                seen.insert(c);
                match parents.get(*c) {
                    Some(ps) => next.extend(ps.iter().map(String::as_str)),
                    None => return Walk::Incomplete,
                }
            }
            next.retain(|c| !seen.contains(c));
            next.sort_unstable();
            next.dedup();
            if next.is_empty() {
                break;
            }
            layer = next;
        }
        match fallback {
            Some((i, hop)) => Walk::Kind(i, hop),
            None => Walk::Unmapped,
        }
    }

    /// Every class a walk from `classes` would read, up to `max_hops`, that
    /// `parents` lacks: what to fetch next.
    fn missing(&self, classes: &[String], parents: &HashMap<String, Vec<String>>) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut layer: Vec<String> = classes.to_vec();
        for _ in 0..self.max_hops {
            let mut next = Vec::new();
            for c in layer {
                if !seen.insert(c.clone()) {
                    continue;
                }
                match parents.get(&c) {
                    Some(ps) => next.extend(ps.iter().cloned()),
                    None => out.push(c),
                }
            }
            if next.is_empty() {
                break;
            }
            layer = next;
        }
        out.sort();
        out.dedup();
        out
    }
}

// ─── Deriving and applying ────────────────────────────────────────────────────

/// Wikimedia requests one refresh may spend (each is 50 items or classes).
pub const BACKGROUND_BUDGET: usize = 60;
const ENTITY_BUDGET: usize = 20;

#[derive(Debug, Default, serde::Serialize)]
pub struct Refreshed {
    pub items_derived: usize,
    pub items_waiting: usize,
    pub classes_fetched: usize,
    pub refused_links: i64,
    pub kinds_set: i64,
    pub questions_open: i64,
    /// Requests ran out (or Wikimedia throttled us); the rest wait for the next run.
    pub incomplete: bool,
}

async fn class_cache(neo4j: &Neo4jClient) -> Result<HashMap<String, Vec<String>>> {
    Ok(neo4j
        .execute(neo4rs::query(
            "MATCH (c:WikiClass) RETURN c.qid AS qid, coalesce(c.parents, []) AS parents",
        ))
        .await?
        .iter()
        .filter_map(|r| {
            Some((
                r.get::<String>("qid").ok()?,
                r.get::<Vec<String>>("parents").unwrap_or_default(),
            ))
        })
        .collect())
}

async fn store_classes(neo4j: &Neo4jClient, classes: &[wiki::ClassInfo]) -> Result<()> {
    let rows: Vec<neo4rs::BoltType> = classes
        .iter()
        .map(|c| {
            let mut m: HashMap<String, neo4rs::BoltType> = HashMap::new();
            m.insert("qid".into(), c.qid.clone().into());
            m.insert("label".into(), c.label.clone().into());
            m.insert("parents".into(), c.parents.clone().into());
            m.into()
        })
        .collect();
    neo4j
        .run(
            neo4rs::query(
                "UNWIND $rows AS r MERGE (c:WikiClass {qid: r.qid}) \
                 SET c.label = r.label, c.parents = r.parents, c.fetched_at = datetime()",
            )
            .param("rows", rows),
        )
        .await?;
    Ok(())
}

/// Derive kinds for linked items (`entity` = only that entity's), spending at
/// most `budget` Wikimedia requests, then apply them to entities.
pub async fn refresh(
    neo4j: &Neo4jClient,
    entity: Option<&str>,
    budget: usize,
) -> Result<Refreshed> {
    let table = KindTable::load()?;
    let mut out = Refreshed::default();
    let mut budget = budget;
    let linked = "MATCH (e:Entity)-[l:REFERS_TO]->(w:WikiItem) \
                  WHERE l.status IN ['confirmed', 'proposed'] AND ($id IS NULL OR e.id = $id)";

    // 1. Items stored before Phase 19 have no class ids.
    let old: Vec<String> = neo4j
        .execute(
            neo4rs::query(&format!(
                "{linked} AND w.class_qids IS NULL RETURN DISTINCT w.qid AS qid LIMIT $n"
            ))
            .param("id", entity.map(String::from))
            .param("n", DERIVE_BATCH),
        )
        .await?
        .iter()
        .filter_map(|r| r.get::<String>("qid").ok())
        .collect();
    for chunk in old.chunks(50) {
        if budget == 0 {
            out.incomplete = true;
            break;
        }
        budget -= 1;
        match wiki::class_qids_of(chunk).await {
            Ok(found) => {
                let rows: Vec<neo4rs::BoltType> = chunk
                    .iter()
                    .map(|q| {
                        let mut m: HashMap<String, neo4rs::BoltType> = HashMap::new();
                        m.insert("qid".into(), q.clone().into());
                        m.insert("c".into(), found.get(q).cloned().unwrap_or_default().into());
                        m.into()
                    })
                    .collect();
                neo4j
                    .run(
                        neo4rs::query("UNWIND $rows AS r MATCH (w:WikiItem {qid: r.qid}) SET w.class_qids = r.c")
                            .param("rows", rows),
                    )
                    .await?;
            }
            Err(e) => {
                warn!(error = %e, "kinds: class ids not fetched");
                out.incomplete = true;
                break;
            }
        }
    }

    // 2. Items whose kind is missing or from an older kinds.yaml.
    let items: Vec<(String, Vec<String>)> = neo4j
        .execute(
            neo4rs::query(&format!(
                "{linked} AND w.class_qids IS NOT NULL AND coalesce(w.kind_version, '') <> $v \
                 RETURN DISTINCT w.qid AS qid, w.class_qids AS c LIMIT $n"
            ))
            .param("id", entity.map(String::from))
            .param("v", table.version.as_str())
            .param("n", DERIVE_BATCH),
        )
        .await?
        .iter()
        .filter_map(|r| {
            Some((
                r.get::<String>("qid").ok()?,
                r.get::<Vec<String>>("c").unwrap_or_default(),
            ))
        })
        .collect();

    // 3. Fetch the classes their walks need, a layer at a time.
    let mut parents = class_cache(neo4j).await?;
    loop {
        let mut need: Vec<String> = items
            .iter()
            .flat_map(|(_, c)| table.missing(c, &parents))
            .collect();
        need.sort();
        need.dedup();
        if need.is_empty() {
            break;
        }
        let mut fetched_any = false;
        for chunk in need.chunks(50) {
            if budget == 0 {
                out.incomplete = true;
                break;
            }
            budget -= 1;
            match wiki::class_info(chunk).await {
                Ok(cs) => {
                    store_classes(neo4j, &cs).await?;
                    out.classes_fetched += cs.len();
                    for c in cs {
                        parents.insert(c.qid, c.parents);
                    }
                    fetched_any = true;
                }
                Err(e) => {
                    warn!(error = %e, "kinds: classes not fetched");
                    out.incomplete = true;
                    break;
                }
            }
        }
        if out.incomplete || !fetched_any {
            break;
        }
    }

    // 4. Walk and stamp. An incomplete walk waits for the next run.
    let mut rows: Vec<neo4rs::BoltType> = Vec::new();
    for (qid, classes) in &items {
        let (kind, ktype, refused) = match table.walk(classes, &parents) {
            Walk::Incomplete => {
                out.items_waiting += 1;
                continue;
            }
            Walk::Kind(i, _) => (
                Some(table.kinds[i].name.clone()),
                Some(table.kinds[i].entity_type.clone()),
                false,
            ),
            Walk::Refused => (None, None, true),
            Walk::Unmapped => (None, None, false),
        };
        let mut m: HashMap<String, neo4rs::BoltType> = HashMap::new();
        m.insert("qid".into(), qid.clone().into());
        m.insert(
            "kind".into(),
            kind.map(Into::into)
                .unwrap_or(neo4rs::BoltType::Null(neo4rs::BoltNull)),
        );
        m.insert(
            "ktype".into(),
            ktype
                .map(Into::into)
                .unwrap_or(neo4rs::BoltType::Null(neo4rs::BoltNull)),
        );
        m.insert("refused".into(), refused.into());
        rows.push(m.into());
    }
    out.items_derived = rows.len();
    if !rows.is_empty() {
        neo4j
            .run(
                neo4rs::query(
                    "UNWIND $rows AS r MATCH (w:WikiItem {qid: r.qid}) \
                     SET w.kind = r.kind, w.kind_type = r.ktype, w.wikimedia_page = r.refused, \
                         w.kind_version = $v",
                )
                .param("rows", rows)
                .param("v", table.version.as_str()),
            )
            .await?;
    }

    let applied = apply(neo4j, entity, &table.version).await?;
    out.refused_links = applied.0;
    out.kinds_set = applied.1;
    out.questions_open = applied.2;
    info!(?out, "entity kinds refreshed");
    Ok(out)
}

/// Refresh one entity after the user changed its link. Never fails the
/// caller: the hourly run catches up.
pub async fn refresh_entity(neo4j: &Neo4jClient, entity: &str) {
    if let Err(e) = refresh(neo4j, Some(entity), ENTITY_BUDGET).await {
        warn!(entity, error = %e, "kinds: refresh after a reference change failed");
    }
}

/// Apply derived kinds to entities. Returns (links refused, entities with a
/// reference kind, questions open).
async fn apply(
    neo4j: &Neo4jClient,
    entity: Option<&str>,
    version: &str,
) -> Result<(i64, i64, i64)> {
    // A proposed link to a Wikimedia page is withdrawn. A confirmed one is the
    // user's and stays.
    let refused = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity)-[l:REFERS_TO {status: 'proposed'}]->(w:WikiItem {wikimedia_page: true}) \
                 WHERE $id IS NULL OR e.id = $id \
                 SET l.status = 'rejected', l.decided_at = datetime(), \
                     l.why = 'Withdrawn: a Wikimedia list or disambiguation page, not one thing' \
                 WITH e, count(l) AS n \
                 WITH e, n, EXISTS { MATCH (e)-[x:REFERS_TO]->() WHERE x.status IN ['proposed', 'confirmed'] } AS active \
                 FOREACH (_ IN CASE WHEN active THEN [] ELSE [1] END | SET e.wiki_result = 'none') \
                 RETURN sum(n) AS c",
            )
            .param("id", entity.map(String::from)),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0);

    // Entities whose active link has a derived item: the link's kind when the
    // coarse types agree; a question when they don't, unless the user has set
    // the type (or kept it against this item) or it is a curated seed.
    neo4j
        .run(
            neo4rs::query(
                "MATCH (e:Entity)-[l:REFERS_TO]->(w:WikiItem) \
                 WHERE l.status IN ['confirmed', 'proposed'] AND w.kind_version = $v \
                   AND ($id IS NULL OR e.id = $id) \
                 WITH e, w ORDER BY CASE l.status WHEN 'confirmed' THEN 0 ELSE 1 END \
                 WITH e, collect(w)[0] AS w \
                 WITH e, w, w.kind IS NOT NULL AND w.kind_type = e.entity_type AS agrees, \
                      w.kind IS NOT NULL AND w.kind_type <> e.entity_type \
                        AND NOT coalesce(e.type_source, '') IN ['manual', 'seed'] \
                        AND NOT w.qid IN coalesce(e.type_kept_qids, []) AS asks, \
                      coalesce(e.kind_source, '') = 'manual' AS mine \
                 SET e.kind = CASE WHEN mine THEN e.kind WHEN agrees THEN w.kind END, \
                     e.kind_source = CASE WHEN mine THEN 'manual' WHEN agrees THEN 'reference' END, \
                     e.kind_qid = CASE WHEN mine THEN e.kind_qid WHEN agrees THEN w.qid END, \
                     e.type_question_qid = CASE WHEN asks THEN w.qid END",
            )
            .param("id", entity.map(String::from))
            .param("v", version),
        )
        .await?;

    // A link that went away takes its kind and its question with it.
    neo4j
        .run(
            neo4rs::query(
                "MATCH (e:Entity) WHERE ($id IS NULL OR e.id = $id) \
                   AND (e.kind_source = 'reference' OR e.type_question_qid IS NOT NULL) \
                   AND NOT EXISTS { MATCH (e)-[x:REFERS_TO]->() WHERE x.status IN ['proposed', 'confirmed'] } \
                 SET e.type_question_qid = null, \
                     e.kind = CASE WHEN e.kind_source = 'reference' THEN null ELSE e.kind END, \
                     e.kind_qid = CASE WHEN e.kind_source = 'reference' THEN null ELSE e.kind_qid END, \
                     e.kind_source = CASE WHEN e.kind_source = 'reference' THEN null ELSE e.kind_source END",
            )
            .param("id", entity.map(String::from)),
        )
        .await?;

    let counts = neo4j
        .execute(neo4rs::query(
            "MATCH (e:Entity) RETURN sum(CASE WHEN e.kind_source = 'reference' THEN 1 ELSE 0 END) AS k, \
                    sum(CASE WHEN e.type_question_qid IS NOT NULL THEN 1 ELSE 0 END) AS q",
        ))
        .await?;
    let r = counts.first();
    Ok((
        refused,
        r.and_then(|r| r.get::<i64>("k").ok()).unwrap_or(0),
        r.and_then(|r| r.get::<i64>("q").ok()).unwrap_or(0),
    ))
}

// ─── The user's answers ───────────────────────────────────────────────────────

/// Ok, or why the request was refused (shown to the user).
pub type Outcome = Result<std::result::Result<(), String>>;

/// Set an entity's type (and optionally its kind) by hand. Final: votes and
/// links never change it until the user unlocks it.
pub async fn set_type(
    neo4j: &Neo4jClient,
    entity: &str,
    entity_type: &str,
    kind: Option<&str>,
) -> Outcome {
    let table = KindTable::load()?;
    let t = entity_type.trim().to_lowercase();
    if !table.entity_types.contains(&t) {
        return Ok(Err(format!(
            "type must be one of {}",
            table.entity_types.join(", ")
        )));
    }
    let kind = kind
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_lowercase);
    if let Some(k) = &kind
        && k.chars().count() > 40
    {
        return Ok(Err("a kind is at most 40 characters".into()));
    }
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity {id: $id}) \
                 SET e.entity_type = $t, e.type_source = 'manual', e.type_question_qid = null, \
                     e.kind = CASE WHEN $k IS NULL THEN null ELSE $k END, \
                     e.kind_source = CASE WHEN $k IS NULL THEN null ELSE 'manual' END, \
                     e.kind_qid = null \
                 RETURN e.id AS id",
            )
            .param("id", entity)
            .param("t", t)
            .param("k", kind),
        )
        .await?;
    if rows.is_empty() {
        return Ok(Err("no such entity".into()));
    }
    // With no kind given, the link may supply one that agrees.
    apply(neo4j, Some(entity), &table.version).await?;
    Ok(Ok(()))
}

/// Answer a type question: `keep` the filed type, or take the `reference`'s.
pub async fn answer(neo4j: &Neo4jClient, entity: &str, answer: &str) -> Outcome {
    let table = KindTable::load()?;
    let cypher = match answer {
        "keep" => {
            "MATCH (e:Entity {id: $id}) WHERE e.type_question_qid IS NOT NULL \
             SET e.type_source = 'manual', \
                 e.type_kept_qids = coalesce(e.type_kept_qids, []) + e.type_question_qid, \
                 e.type_question_qid = null \
             RETURN e.id AS id"
        }
        "reference" => {
            "MATCH (e:Entity {id: $id}) WHERE e.type_question_qid IS NOT NULL \
             MATCH (w:WikiItem {qid: e.type_question_qid}) WHERE w.kind IS NOT NULL \
             SET e.entity_type = w.kind_type, e.type_source = 'manual', e.kind = w.kind, \
                 e.kind_source = 'reference', e.kind_qid = w.qid, e.type_question_qid = null \
             RETURN e.id AS id"
        }
        other => {
            return Ok(Err(format!(
                "answer must be 'keep' or 'reference', not '{other}'"
            )));
        }
    };
    let rows = neo4j
        .execute(neo4rs::query(cypher).param("id", entity))
        .await?;
    if rows.is_empty() {
        return Ok(Err("this entity has no open type question".into()));
    }
    apply(neo4j, Some(entity), &table.version).await?;
    Ok(Ok(()))
}

/// "Let the brain decide": drop the user's type and kind, re-run the vote
/// over its mentions, and re-apply the link's kind. Curated seeds stay.
pub async fn unlock(neo4j: &Neo4jClient, entity: &str) -> Outcome {
    let table = KindTable::load()?;
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity {id: $id}) WHERE coalesce(e.type_source, '') <> 'seed' \
                 REMOVE e.type_source, e.type_kept_qids \
                 SET e.kind = CASE WHEN e.kind_source = 'manual' THEN null ELSE e.kind END, \
                     e.kind_source = CASE WHEN e.kind_source = 'manual' THEN null ELSE e.kind_source END \
                 WITH e \
                 OPTIONAL MATCH (:Note)-[m:MENTIONS]->(e) WHERE m.type IS NOT NULL \
                 WITH e, m.type AS t, count(m) AS c ORDER BY c DESC \
                 WITH e, collect(t)[0] AS top \
                 SET e.entity_type = coalesce(top, e.entity_type) \
                 RETURN e.id AS id",
            )
            .param("id", entity),
        )
        .await?;
    if rows.is_empty() {
        return Ok(Err(
            "no such entity, or it is a curated seed whose type is fixed".into(),
        ));
    }
    apply(neo4j, Some(entity), &table.version).await?;
    Ok(Ok(()))
}

/// GET /api/entities/types — open questions (`questions`) and the vocabulary
/// the HBI offers (`entity_types`, `kinds`).
pub async fn questions(neo4j: &Neo4jClient, limit: i64) -> Result<Value> {
    let table = KindTable::load()?;
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity) WHERE e.type_question_qid IS NOT NULL \
                 MATCH (w:WikiItem {qid: e.type_question_qid}) \
                 OPTIONAL MATCH (e)-[l:REFERS_TO]->(w) \
                 WITH e, w, l, COUNT { (:Note)-[:MENTIONS]->(e) } AS m \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS t, m, \
                        w.qid AS qid, w.label AS label, w.description AS description, \
                        w.kind AS kind, w.kind_type AS kind_type, l.status AS status \
                 ORDER BY m DESC, name LIMIT $n",
            )
            .param("n", limit),
        )
        .await?;
    let total = neo4j
        .execute(neo4rs::query(
            "MATCH (e:Entity) WHERE e.type_question_qid IS NOT NULL RETURN count(e) AS c",
        ))
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0);
    Ok(json!({
        "total": total,
        "questions": rows.iter().map(|r| json!({
            "id": r.get::<String>("id").ok(),
            "name": r.get::<String>("name").ok(),
            "entity_type": r.get::<String>("t").ok(),
            "mentions": r.get::<i64>("m").unwrap_or(0),
            "qid": r.get::<String>("qid").ok(),
            "label": r.get::<String>("label").ok(),
            "description": r.get::<String>("description").ok(),
            "kind": r.get::<String>("kind").ok(),
            "kind_type": r.get::<String>("kind_type").ok(),
            "link_status": r.get::<String>("status").ok(),
        })).collect::<Vec<_>>(),
        "entity_types": table.entity_types,
        "kinds": table.kinds.iter().map(|k| json!({ "name": k.name, "entity_type": k.entity_type })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> KindTable {
        KindTable::load_from(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../entities"
        )))
        .expect("the shipped kinds.yaml parses")
    }

    fn graph(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(c, ps)| (c.to_string(), ps.iter().map(|p| p.to_string()).collect()))
            .collect()
    }

    fn kind(t: &KindTable, w: Walk) -> String {
        match w {
            Walk::Kind(i, _) => t.kinds[i].name.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn the_shipped_table_parses_and_names_known_types() {
        let t = table();
        assert!(t.kinds.len() >= 20);
        for k in &t.kinds {
            assert!(
                t.entity_types.contains(&k.entity_type),
                "{} implies an unknown type",
                k.name
            );
        }
    }

    #[test]
    fn a_us_state_is_a_state_not_a_country() {
        // Michigan: P31 U.S. state (Q35657), which is under "state" (Q7275),
        // a far-off country ancestor. List order once made it a country.
        let t = table();
        let g = graph(&[("Q35657", &["Q7275", "Q107390"]), ("Q7275", &["Q6256"])]);
        assert_eq!(
            kind(&t, t.walk(&["Q35657".into()], &g)),
            "state or province"
        );
    }

    #[test]
    fn a_specific_kind_beats_a_nearer_fallback() {
        // The Wall Street Journal: a daily newspaper and a website (a fallback, hop 0).
        let t = table();
        let g = graph(&[("Q1110794", &["Q11032"]), ("Q35127", &["Q7397"])]);
        assert_eq!(
            kind(&t, t.walk(&["Q1110794".into(), "Q35127".into()], &g)),
            "news outlet"
        );
        // TikTok: only website-ish classes → the fallback.
        let g = graph(&[("Q6576792", &["Q35127"]), ("Q35127", &[])]);
        assert_eq!(kind(&t, t.walk(&["Q6576792".into()], &g)), "website");
    }

    #[test]
    fn a_set_index_is_refused_and_a_class_item_is_unmapped() {
        let t = table();
        let g = graph(&[("Q15623926", &["Q13406463"])]);
        assert_eq!(t.walk(&["Q15623926".into()], &g), Walk::Refused);
        assert_eq!(t.walk(&[], &g), Walk::Unmapped);
    }

    #[test]
    fn a_walk_needing_an_unfetched_class_waits() {
        let t = table();
        let g = graph(&[("Qa", &["Qb"])]);
        assert_eq!(t.walk(&["Qa".into()], &g), Walk::Incomplete);
        assert_eq!(t.missing(&["Qa".into()], &g), vec!["Qb".to_string()]);
    }

    #[test]
    fn the_version_follows_the_file() {
        let a = KindTable::from_yaml("kinds: []").unwrap();
        let b = KindTable::from_yaml("kinds: []\nmax_hops: 3").unwrap();
        assert_ne!(a.version, b.version);
    }

    #[test]
    fn the_alias_loader_reads_kinds_yaml_as_empty() {
        // kinds.yaml shares entities/ with the alias seed.
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../entities/kinds.yaml"
        ))
        .unwrap();
        let r = crate::services::entity_resolution::Rules::from_yaml(&text).unwrap();
        assert_eq!(
            r.entity_type("country"),
            "country",
            "kinds.yaml must not add type folds"
        );
    }
}
