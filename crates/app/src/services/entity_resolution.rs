//! Entity resolution: one real thing, one `:Entity` node.
//!
//! Entity extraction used to `MERGE (:Entity {name})` on the exact lowercased
//! string, so `us` / `u.s.` / `united states` / `usa` were five nodes and
//! `trump` / `donald trump` / `president trump` three. This module is the fix
//! (plan: `project-docs/HBI_REDESIGN_PLAN.md` §3.1):
//!
//! - **Key.** Every entity carries `key`: its name lowercased with a leading
//!   article dropped, a leading title dropped when a full name follows it, and
//!   everything but letters and digits removed (`"President Donald Trump"` →
//!   `donaldtrump`, `"U.S. Senate"` → `ussenate`, `"The Guardian"` →
//!   `guardian`). Two entities with one key are one thing — except a name in
//!   `keep_distinct`, an all-digit key (`911` is not `9/11`), and an entity a
//!   person split back out (`distinct: true`, key `~name`). A title before a
//!   bare surname (`senator johnson`) is not folded: there are several
//!   Johnsons, so that pair is a proposal.
//! - **Aliases.** `(:EntityAlias {name, key})-[:ALIAS_OF]->(:Entity)` comes
//!   from the curated seed (`entities/*.yaml`, `ENTITY_ALIASES_DIR`) and from
//!   every merge. A case-sensitive alias (`us`) resolves only when the note's
//!   text has it in capitals (`US`, `U.S.`), so the pronoun stays out.
//! - **Mentions keep the surface.** `MENTIONS.surface` is the text as written,
//!   `via` the name it resolved through (absent when it was the canonical name),
//!   `type` the type the extractor gave this time.
//! - **Merges are reversible.** A merge repoints mentions (stamping `via` and
//!   `merged_from`, or appending `"<id>:<count>"` to `absorbed` when the note
//!   already mentioned the target), keeps the old node's id/type on the alias,
//!   and deletes the old node. [`unmerge`] reverses it from the alias.
//! - **Curated + deterministic merges are automatic** ([`sync`], run by
//!   `init-db` after a snapshot). Everything else is a **proposal**
//!   ([`candidates`]) that a person accepts, rejects (`NOT_SAME`), or marks
//!   related (`RELATED_ENTITY`) in the HBI.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::Neo4jClient;

// ─── Rules (the seed) ─────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct RulesFile {
    #[serde(default)]
    titles: Vec<String>,
    #[serde(default)]
    articles: Vec<String>,
    #[serde(default)]
    keep_distinct: Vec<String>,
    #[serde(default)]
    types: HashMap<String, String>,
    #[serde(default)]
    entities: Vec<Canonical>,
}

/// A curated canonical entity and the names that mean it.
#[derive(Debug, Clone, Deserialize)]
pub struct Canonical {
    pub name: String,
    #[serde(rename = "type", default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Aliases that are also ordinary words: they resolve only when the note's
    /// text has them in capitals.
    #[serde(default)]
    pub case_sensitive: Vec<String>,
}

/// Normalisation rules plus the curated canonicals, from `entities/*.yaml`.
#[derive(Debug, Default)]
pub struct Rules {
    /// Leading titles, longest first, each as tokens. Dropped only when at
    /// least two words (a full name) follow.
    titles: Vec<Vec<String>>,
    /// Leading articles, dropped whenever a word follows.
    articles: Vec<Vec<String>>,
    keep_distinct: HashSet<String>,
    types: HashMap<String, String>,
    pub canonicals: Vec<Canonical>,
}

impl Rules {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let f: RulesFile = serde_yaml::from_str(text)?;
        Ok(Self::from_files(vec![f]))
    }

    fn from_files(files: Vec<RulesFile>) -> Self {
        let mut r = Rules::default();
        for f in files {
            for (list, out) in [(&f.titles, &mut r.titles), (&f.articles, &mut r.articles)] {
                for p in list {
                    let toks: Vec<String> = p.split_whitespace().map(clean_token).collect();
                    if !toks.is_empty() && !out.contains(&toks) {
                        out.push(toks);
                    }
                }
            }
            r.keep_distinct
                .extend(f.keep_distinct.iter().map(|n| display_name(n)));
            r.types.extend(
                f.types
                    .into_iter()
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_lowercase())),
            );
            r.canonicals.extend(f.entities.into_iter().map(|mut c| {
                c.name = display_name(&c.name);
                c.aliases = c.aliases.iter().map(|a| display_name(a)).collect();
                c.case_sensitive = c.case_sensitive.iter().map(|a| display_name(a)).collect();
                c
            }));
        }
        r.titles.sort_by_key(|t| std::cmp::Reverse(t.len()));
        r.articles.sort_by_key(|t| std::cmp::Reverse(t.len()));
        r
    }

    /// Every `*.yaml` in `dir`. A missing dir gives the built-in punctuation
    /// normalisation only (no titles, no curated aliases) and says so.
    pub fn load_dir(dir: &Path) -> Self {
        let mut files = Vec::new();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                let mut paths: Vec<_> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
                    .collect();
                paths.sort();
                for p in paths {
                    match std::fs::read_to_string(&p)
                        .map_err(anyhow::Error::from)
                        .and_then(|t| serde_yaml::from_str::<RulesFile>(&t).map_err(Into::into))
                    {
                        Ok(f) => files.push(f),
                        Err(e) => {
                            warn!(path = %p.display(), error = %e, "entity aliases: file skipped")
                        }
                    }
                }
            }
            Err(e) => warn!(
                dir = %dir.display(), error = %e,
                "entity aliases: no seed dir — names are folded by punctuation only, no curated aliases"
            ),
        }
        Self::from_files(files)
    }

    /// The process-wide rules, read once from `ENTITY_ALIASES_DIR` (default `entities`).
    pub fn global() -> &'static Rules {
        static RULES: OnceLock<Rules> = OnceLock::new();
        RULES.get_or_init(|| Rules::load_dir(&aliases_dir()))
    }

    /// The comparison key for a name. See the module docs.
    pub fn key(&self, name: &str) -> String {
        let display = display_name(name);
        if self.keep_distinct.contains(&display) {
            return display;
        }
        let mut toks: Vec<String> = display.split_whitespace().map(clean_token).collect();
        'strip: loop {
            for (list, keep) in [(&self.articles, 1), (&self.titles, 2)] {
                for p in list {
                    if toks.len() >= p.len() + keep && toks[..p.len()] == p[..] {
                        toks.drain(..p.len());
                        continue 'strip;
                    }
                }
            }
            break;
        }
        let k: String = toks
            .concat()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if k.is_empty() { display } else { k }
    }

    /// `senator johnson` → `johnson`: a title before one bare word, which the
    /// key does not fold (see the module docs) and proposals offer instead.
    pub fn titled_surname(&self, name: &str) -> Option<String> {
        let toks: Vec<String> = display_name(name)
            .split_whitespace()
            .map(clean_token)
            .collect();
        self.titles
            .iter()
            .find(|p| toks.len() == p.len() + 1 && toks[..p.len()] == p[..])
            .map(|_| toks[toks.len() - 1].clone())
    }

    /// The extractor's type folded into the vocabulary (`organization` → `organisation`).
    pub fn entity_type(&self, t: &str) -> String {
        let t = t.trim().to_lowercase();
        let t = if t.is_empty() {
            "unknown".to_string()
        } else {
            t
        };
        self.types.get(&t).cloned().unwrap_or(t)
    }

    /// Keys that only resolve through an alias when the text has them in capitals.
    pub fn case_sensitive_keys(&self) -> HashSet<String> {
        self.canonicals
            .iter()
            .flat_map(|c| c.case_sensitive.iter().map(|a| self.key(a)))
            .collect()
    }
}

pub fn aliases_dir() -> std::path::PathBuf {
    std::env::var("ENTITY_ALIASES_DIR")
        .unwrap_or_else(|_| "entities".into())
        .into()
}

/// Lowercased, curly quotes straightened, whitespace collapsed. This is what
/// `Entity.name` holds.
pub fn display_name(s: &str) -> String {
    s.replace(['\u{2019}', '\u{2018}'], "'")
        .replace(['\u{201c}', '\u{201d}'], "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn clean_token(t: &str) -> String {
    t.trim_matches(|c: char| c == '.' || c == ',')
        .to_lowercase()
}

/// A regex (Rust and Java/Cypher compatible) matching `key` written in capitals,
/// optionally with periods, as a whole word: `us` → `U.S.`, `US`.
pub fn upper_pattern(key: &str) -> String {
    let body: Vec<String> = key
        .chars()
        .map(|c| regex::escape(&c.to_uppercase().to_string()))
        .collect();
    format!("(^|[^A-Za-z]){}\\.?([^A-Za-z]|$)", body.join("\\.?"))
}

/// Does the text have `key` in capitals? (Case-sensitive aliases.)
pub fn text_has_upper(text: &str, key: &str) -> bool {
    regex::Regex::new(&upper_pattern(key))
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

// ─── Write-time resolution ───────────────────────────────────────────────────

/// One extracted entity, ready to resolve.
#[derive(Debug, Clone)]
pub struct Probe {
    pub name: String,
    pub key: String,
    pub surface: String,
    pub entity_type: String,
    /// The note's text has this name in capitals (only consulted for a
    /// case-sensitive alias).
    pub upper_in_text: bool,
}

/// What the graph already holds for one probe.
#[derive(Debug, Default, Clone)]
pub struct Found {
    /// Target of an alias with exactly this name, and whether it is case-sensitive.
    pub alias_by_name: Option<(String, bool)>,
    /// An entity with exactly this name.
    pub entity_by_name: Option<String>,
    /// Targets of aliases with this key; `true` if any of them is case-sensitive.
    pub alias_by_key: (Vec<String>, bool),
    /// An entity with this key.
    pub entity_by_key: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// Mention this existing entity; `via` is set when the probe's name differs.
    Existing { id: String, via_alias: bool },
    /// No match: create an entity named `probe.name`.
    Create,
}

/// Alias by name → entity by name → alias by key → entity by key → create.
/// A case-sensitive alias is skipped unless the text has the name in capitals.
pub fn resolve(p: &Probe, f: &Found) -> Target {
    if let Some((id, cs)) = &f.alias_by_name
        && (!cs || p.upper_in_text)
    {
        return Target::Existing {
            id: id.clone(),
            via_alias: true,
        };
    }
    if let Some(id) = &f.entity_by_name {
        return Target::Existing {
            id: id.clone(),
            via_alias: false,
        };
    }
    let (ids, cs) = &f.alias_by_key;
    if let Some(id) = ids.first()
        && (!cs || p.upper_in_text)
    {
        return Target::Existing {
            id: id.clone(),
            via_alias: true,
        };
    }
    if let Some(id) = &f.entity_by_key {
        return Target::Existing {
            id: id.clone(),
            via_alias: false,
        };
    }
    Target::Create
}

/// Look up every probe in one round trip.
pub async fn lookup(neo4j: &Neo4jClient, probes: &[Probe]) -> Result<HashMap<String, Found>> {
    let rows: Vec<neo4rs::BoltType> = probes
        .iter()
        .map(|p| {
            let mut m = HashMap::new();
            m.insert("name".to_string(), neo4rs::BoltType::from(p.name.clone()));
            m.insert("key".to_string(), p.key.clone().into());
            neo4rs::BoltType::from(m)
        })
        .collect();
    let out = neo4j
        .execute(
            neo4rs::query(
                "UNWIND $probes AS p \
                 CALL (p) { OPTIONAL MATCH (a:EntityAlias {name: p.name})-[:ALIAS_OF]->(t:Entity) \
                            RETURN t.id AS an_id, coalesce(a.case_sensitive, false) AS an_cs LIMIT 1 } \
                 CALL (p) { OPTIONAL MATCH (e:Entity {name: p.name}) RETURN e.id AS en_id LIMIT 1 } \
                 CALL (p) { OPTIONAL MATCH (a:EntityAlias {key: p.key})-[:ALIAS_OF]->(t:Entity) \
                            RETURN collect(DISTINCT t.id) AS ak_ids, \
                                   any(x IN collect(a.case_sensitive) WHERE x) AS ak_cs } \
                 CALL (p) { OPTIONAL MATCH (e:Entity {key: p.key}) RETURN e.id AS ek_id LIMIT 1 } \
                 RETURN p.name AS name, an_id, an_cs, en_id, ak_ids, ak_cs, ek_id",
            )
            .param("probes", rows),
        )
        .await?;
    let mut found = HashMap::new();
    for r in out {
        let name: String = r.get("name").unwrap_or_default();
        let an_id: Option<String> = r.get("an_id").ok();
        found.insert(
            name,
            Found {
                alias_by_name: an_id.map(|id| (id, r.get::<bool>("an_cs").unwrap_or(false))),
                entity_by_name: r.get("en_id").ok(),
                alias_by_key: (
                    r.get::<Vec<String>>("ak_ids").unwrap_or_default(),
                    r.get::<bool>("ak_cs").unwrap_or(false),
                ),
                entity_by_key: r.get("ek_id").ok(),
            },
        );
    }
    Ok(found)
}

// ─── Merge planning (sync) ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EntityRow {
    pub name: String,
    pub key: String,
    pub mentions: i64,
    pub seeded: bool,
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub struct AliasRow {
    pub name: String,
    pub key: String,
    /// The canonical entity's name.
    pub target: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedMerge {
    pub from: String,
    pub into: String,
    /// `seed` (a curated alias) or `normalise` (same key).
    pub method: &'static str,
    /// Set for a case-sensitive alias: only mentions from notes whose text has
    /// this key in capitals move; the rest stay on `from`.
    pub only_upper: Option<String>,
}

/// Which existing entities fold into which. Pure: the caller supplies the
/// graph's entities and aliases (seed aliases overlaid) and gets the merges.
pub fn plan_merges(entities: &[EntityRow], aliases: &[AliasRow]) -> Vec<PlannedMerge> {
    let alias_by_name: HashMap<&str, &AliasRow> =
        aliases.iter().map(|a| (a.name.as_str(), a)).collect();
    let mut alias_by_key: HashMap<&str, Vec<&AliasRow>> = HashMap::new();
    for a in aliases {
        alias_by_key.entry(a.key.as_str()).or_default().push(a);
    }
    let cs_keys: HashSet<&str> = aliases
        .iter()
        .filter(|a| a.case_sensitive)
        .map(|a| a.key.as_str())
        .collect();
    let exists: HashSet<&str> = entities.iter().map(|e| e.name.as_str()).collect();

    let mut merges: Vec<PlannedMerge> = Vec::new();
    let mut gone: HashSet<String> = HashSet::new();
    for e in entities.iter().filter(|e| !e.distinct && !e.seeded) {
        let target = alias_by_name
            .get(e.name.as_str())
            .map(|a| a.target.as_str())
            .or_else(|| {
                alias_by_key
                    .get(e.key.as_str())
                    .and_then(|v| v.first())
                    .map(|a| a.target.as_str())
            });
        let Some(target) = target else { continue };
        if target == e.name || !exists.contains(target) {
            continue;
        }
        let only_upper = cs_keys.contains(e.key.as_str()).then(|| e.key.clone());
        if only_upper.is_none() {
            gone.insert(e.name.clone());
        }
        merges.push(PlannedMerge {
            from: e.name.clone(),
            into: target.to_string(),
            method: "seed",
            only_upper,
        });
    }

    // Same key, no alias: fold into the seeded one, else the most mentioned,
    // else the shortest name.
    let mut groups: HashMap<&str, Vec<&EntityRow>> = HashMap::new();
    for e in entities
        .iter()
        .filter(|e| !e.distinct && !gone.contains(&e.name))
        .filter(|e| !e.key.chars().all(|c| c.is_ascii_digit()))
    {
        groups.entry(e.key.as_str()).or_default().push(e);
    }
    let mut keys: Vec<&&str> = groups.keys().collect();
    keys.sort();
    for k in keys {
        let g = &groups[*k];
        if g.len() < 2 {
            continue;
        }
        let mut g = g.clone();
        g.sort_by(|a, b| {
            b.seeded
                .cmp(&a.seeded)
                .then(b.mentions.cmp(&a.mentions))
                .then(a.name.len().cmp(&b.name.len()))
                .then(a.name.cmp(&b.name))
        });
        let canon = g[0];
        for e in &g[1..] {
            if e.seeded || merges.iter().any(|m| m.from == e.name) {
                continue;
            }
            merges.push(PlannedMerge {
                from: e.name.clone(),
                into: canon.name.clone(),
                method: "normalise",
                only_upper: None,
            });
        }
    }

    // Follow chains (a → b, b → c ⇒ a → c) so nothing merges into a node that
    // is about to disappear.
    let full: HashMap<String, String> = merges
        .iter()
        .filter(|m| m.only_upper.is_none())
        .map(|m| (m.from.clone(), m.into.clone()))
        .collect();
    for m in &mut merges {
        let mut seen = HashSet::new();
        while let Some(next) = full.get(&m.into) {
            if !seen.insert(m.into.clone()) || *next == m.from {
                break;
            }
            m.into = next.clone();
        }
    }
    merges.retain(|m| m.from != m.into);
    merges
}

// ─── Merge / unmerge ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct MergeOutcome {
    pub from: String,
    pub into: String,
    pub mentions_moved: i64,
    /// Mentions left on `from` because their note doesn't have the name in
    /// capitals (case-sensitive aliases only).
    pub mentions_kept: i64,
    pub from_deleted: bool,
}

/// Fold entity `from` into `into` (both by name). Reversible — see [`unmerge`].
pub async fn merge(
    neo4j: &Neo4jClient,
    rules: &Rules,
    from: &str,
    into: &str,
    method: &str,
    only_upper: Option<&str>,
) -> Result<MergeOutcome> {
    if from == into {
        anyhow::bail!("cannot merge an entity into itself");
    }
    let pattern = only_upper
        .map(|k| format!("(?s).*{}.*", upper_pattern(k)))
        .unwrap_or_default();
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (f:Entity {name: $from}), (t:Entity {name: $into}) \
                 OPTIONAL MATCH (n:Note)-[r:MENTIONS]->(f) \
                 WHERE $pattern = '' OR coalesce(n.content, '') =~ $pattern \
                 WITH f, t, n, r \
                 CALL (f, t, n, r) { \
                   WITH * WHERE r IS NOT NULL \
                   OPTIONAL MATCH (n)-[ex:MENTIONS]->(t) \
                   FOREACH (_ IN CASE WHEN ex IS NULL THEN [1] ELSE [] END | \
                     CREATE (n)-[nr:MENTIONS]->(t) \
                     SET nr = properties(r), nr.surface = coalesce(r.surface, f.name), \
                         nr.via = f.name, nr.merged_from = f.id) \
                   FOREACH (_ IN CASE WHEN ex IS NULL THEN [] ELSE [1] END | \
                     SET ex.count = coalesce(ex.count, 1) + coalesce(r.count, 1), \
                         ex.absorbed = coalesce(ex.absorbed, []) + [f.id + ':' + toString(coalesce(r.count, 1))]) \
                   DELETE r \
                   RETURN count(*) AS moved \
                 } \
                 RETURN sum(moved) AS moved",
            )
            .param("from", from)
            .param("into", into)
            .param("pattern", pattern),
        )
        .await
        .context("moving mentions")?;
    let Some(row) = rows.first() else {
        anyhow::bail!("no entity named '{from}' or '{into}'");
    };
    let moved = row.get::<i64>("moved").unwrap_or(0);

    if only_upper.is_some() {
        // The rest of `from` (lowercase mentions) stays as it is, for review.
        // With nothing left, it goes the way of a full merge.
        let kept = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (:Note)-[r:MENTIONS]->(:Entity {name: $from}) RETURN count(r) AS c",
                )
                .param("from", from),
            )
            .await?
            .first()
            .and_then(|r| r.get::<i64>("c").ok())
            .unwrap_or(0);
        if kept == 0 {
            return finish_merge(neo4j, rules, from, into, method, moved).await;
        }
        return Ok(MergeOutcome {
            from: from.into(),
            into: into.into(),
            mentions_moved: moved,
            mentions_kept: kept,
            from_deleted: false,
        });
    }

    finish_merge(neo4j, rules, from, into, method, moved).await
}

/// Mentions a capitals-only merge would move right now.
async fn movable_upper(neo4j: &Neo4jClient, from: &str, key: &str) -> Result<i64> {
    Ok(neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:MENTIONS]->(:Entity {name: $from}) \
                 WHERE coalesce(n.content, '') =~ $pattern RETURN count(n) AS c",
            )
            .param("from", from)
            .param("pattern", format!("(?s).*{}.*", upper_pattern(key))),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0))
}

/// Leave the old name behind as an alias carrying what is needed to undo,
/// re-point aliases of `from` at `into`, then drop the old node.
async fn finish_merge(
    neo4j: &Neo4jClient,
    rules: &Rules,
    from: &str,
    into: &str,
    method: &str,
    moved: i64,
) -> Result<MergeOutcome> {
    neo4j
        .run(
            neo4rs::query(
                "MATCH (f:Entity {name: $from}), (t:Entity {name: $into}) \
                 MERGE (a:EntityAlias {name: f.name}) \
                 SET a.key = $key, a.merged_from_id = f.id, a.entity_type = f.entity_type, \
                     a.entity_created_at = f.created_at, a.merged_at = datetime(), \
                     a.method = coalesce(a.method, $method), \
                     a.case_sensitive = coalesce(a.case_sensitive, false) \
                 WITH a, f, t \
                 OPTIONAL MATCH (a)-[old:ALIAS_OF]->() DELETE old \
                 WITH a, f, t \
                 MERGE (a)-[:ALIAS_OF]->(t) \
                 WITH f, t \
                 OPTIONAL MATCH (o:EntityAlias)-[ol:ALIAS_OF]->(f) \
                 FOREACH (_ IN CASE WHEN o IS NULL THEN [] ELSE [1] END | \
                   DELETE ol MERGE (o)-[:ALIAS_OF]->(t)) \
                 WITH DISTINCT f, t \
                 OPTIONAL MATCH (f)-[h:NOT_IN_TOPIC]->(tp:Topic) \
                 FOREACH (_ IN CASE WHEN tp IS NULL THEN [] ELSE [1] END | \
                   MERGE (t)-[nh:NOT_IN_TOPIC]->(tp) ON CREATE SET nh.at = h.at) \
                 WITH DISTINCT f \
                 DETACH DELETE f",
            )
            .param("from", from)
            .param("into", into)
            .param("key", rules.key(from))
            .param("method", method),
        )
        .await
        .context("writing the alias")?;
    Ok(MergeOutcome {
        from: from.into(),
        into: into.into(),
        mentions_moved: moved,
        mentions_kept: 0,
        from_deleted: true,
    })
}

#[derive(Debug, Serialize)]
pub struct UnmergeOutcome {
    pub name: String,
    pub restored_id: String,
    pub restored_from: String,
    pub mentions_moved: i64,
}

/// Undo a merge: bring the alias back as its own entity, with the mentions
/// that came through it. Mentions other names folded into the same edge
/// (`absorbed`) stay with the target, so undoing one alias never undoes another. The restored entity is `distinct`, so neither the
/// seed nor normalisation folds it again; to stop a curated alias resolving
/// new mentions, remove it from `entities/*.yaml` too.
pub async fn unmerge(neo4j: &Neo4jClient, alias: &str) -> Result<UnmergeOutcome> {
    let alias = display_name(alias);
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (a:EntityAlias {name: $name})-[:ALIAS_OF]->(t:Entity) \
                 OPTIONAL MATCH (e:Entity {name: $name}) \
                 WITH a, t, e \
                 CALL (a, t, e) { \
                   WITH * WHERE e IS NULL \
                   CREATE (x:Entity {id: coalesce(a.merged_from_id, randomUUID()), name: a.name}) \
                   SET x.entity_type = coalesce(a.entity_type, t.entity_type), \
                       x.created_at = coalesce(a.entity_created_at, datetime()) \
                   RETURN x \
                   UNION \
                   WITH * WHERE e IS NOT NULL \
                   RETURN e AS x \
                 } \
                 SET x.distinct = true, x.key = '~' + x.name \
                 RETURN x.id AS id, t.name AS target, coalesce(a.merged_from_id, '') AS fid",
            )
            .param("name", alias.as_str()),
        )
        .await?;
    let Some(row) = rows.first() else {
        anyhow::bail!("no alias named '{alias}'");
    };
    let restored_id: String = row.get("id").unwrap_or_default();
    let target: String = row.get("target").unwrap_or_default();
    let fid: String = row.get("fid").unwrap_or_default();

    let moved = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Entity {name: $target}), (e:Entity {name: $name}) \
                 OPTIONAL MATCH (n:Note)-[r:MENTIONS]->(t) WHERE r.via = $name \
                 CALL (e, n, r) { \
                   WITH * WHERE r IS NOT NULL \
                   WITH e, n, r, reduce(k = 0, x IN coalesce(r.absorbed, []) | \
                                        k + toInteger(split(x, ':')[1])) AS kept \
                   MERGE (n)-[nr:MENTIONS]->(e) \
                   ON CREATE SET nr.count = coalesce(r.count, 1) - kept, nr.surface = r.surface, nr.type = r.type \
                   ON MATCH SET nr.count = coalesce(nr.count, 0) + coalesce(r.count, 1) - kept \
                   FOREACH (_ IN CASE WHEN kept > 0 THEN [1] ELSE [] END | \
                     SET r.count = kept REMOVE r.via, r.merged_from) \
                   FOREACH (_ IN CASE WHEN kept > 0 THEN [] ELSE [1] END | DELETE r) \
                   RETURN count(*) AS c \
                 } \
                 RETURN sum(c) AS moved",
            )
            .param("target", target.as_str())
            .param("name", alias.as_str()),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("moved").ok())
        .unwrap_or(0);

    // Mentions that were folded into an existing one (`absorbed`).
    let mut absorbed = 0;
    if !fid.is_empty() {
        absorbed = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Note)-[r:MENTIONS]->(t:Entity {name: $target}) \
                     WHERE any(x IN coalesce(r.absorbed, []) WHERE x STARTS WITH $fid + ':') \
                     MATCH (e:Entity {name: $name}) \
                     WITH n, r, e, [x IN r.absorbed WHERE x STARTS WITH $fid + ':'][0] AS entry \
                     WITH n, r, e, entry, toInteger(split(entry, ':')[1]) AS c \
                     MERGE (n)-[nr:MENTIONS]->(e) \
                     ON CREATE SET nr.count = c \
                     ON MATCH SET nr.count = coalesce(nr.count, 0) + c \
                     SET r.count = r.count - c, r.absorbed = [x IN r.absorbed WHERE x <> entry] \
                     WITH r, count(*) AS moved \
                     FOREACH (_ IN CASE WHEN r.count <= 0 THEN [1] ELSE [] END | DELETE r) \
                     RETURN sum(moved) AS moved",
                )
                .param("target", target.as_str())
                .param("name", alias.as_str())
                .param("fid", fid.as_str()),
            )
            .await?
            .first()
            .and_then(|r| r.get::<i64>("moved").ok())
            .unwrap_or(0);
    }

    neo4j
        .run(
            neo4rs::query("MATCH (a:EntityAlias {name: $name}) DETACH DELETE a")
                .param("name", alias.as_str()),
        )
        .await?;
    info!(alias = %alias, from = %target, moved = moved + absorbed, "Entity unmerged");
    Ok(UnmergeOutcome {
        name: alias,
        restored_id,
        restored_from: target,
        mentions_moved: moved + absorbed,
    })
}

// ─── Sync: seed + keys + automatic merges ────────────────────────────────────

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub dry_run: bool,
    pub canonicals_seeded: usize,
    pub aliases_seeded: usize,
    /// Seed aliases not written because a person split that name out.
    pub aliases_skipped_distinct: Vec<String>,
    pub keys_written: usize,
    pub planned: Vec<PlannedMerge>,
    pub merged: Vec<MergeOutcome>,
    pub failed: Vec<String>,
    pub snapshot: Option<String>,
}

/// Apply the seed and fold entities that are the same thing. Idempotent;
/// `init-db` runs it on every start. With merges to make, a snapshot is taken
/// first (`snapshot_dir`), and a failed snapshot stops the merges.
pub async fn sync(
    neo4j: &Neo4jClient,
    rules: &Rules,
    dry_run: bool,
    snapshot_dir: Option<&Path>,
) -> Result<SyncReport> {
    let mut report = SyncReport {
        dry_run,
        ..Default::default()
    };

    // Entities as they stand, keyed fresh (rules may have changed).
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (e:Entity) \
             RETURN e.id AS id, e.name AS name, coalesce(e.key, '') AS key, \
                    coalesce(e.distinct, false) AS distinct, \
                    coalesce(e.type_source, '') = 'seed' AS seeded, \
                    COUNT { (:Note)-[:MENTIONS]->(e) } AS mentions",
        ))
        .await?;
    let mut entities: Vec<EntityRow> = Vec::with_capacity(rows.len());
    let mut key_updates: Vec<neo4rs::BoltType> = Vec::new();
    for r in &rows {
        let name: String = r.get("name").unwrap_or_default();
        let distinct = r.get::<bool>("distinct").unwrap_or(false);
        let key = if distinct {
            format!("~{name}")
        } else {
            rules.key(&name)
        };
        if r.get::<String>("key").unwrap_or_default() != key {
            let mut m = HashMap::new();
            m.insert(
                "id".to_string(),
                neo4rs::BoltType::from(r.get::<String>("id").unwrap_or_default()),
            );
            m.insert("key".to_string(), key.clone().into());
            key_updates.push(m.into());
        }
        entities.push(EntityRow {
            name,
            key,
            mentions: r.get::<i64>("mentions").unwrap_or(0),
            seeded: r.get::<bool>("seeded").unwrap_or(false),
            distinct,
        });
    }
    let distinct_names: HashSet<String> = entities
        .iter()
        .filter(|e| e.distinct)
        .map(|e| e.name.clone())
        .collect();

    // Seed canonicals + aliases (in memory for planning; written unless dry-run).
    let cs_keys = rules.case_sensitive_keys();
    let mut aliases: Vec<AliasRow> = Vec::new();
    for c in &rules.canonicals {
        match entities.iter_mut().find(|e| e.name == c.name) {
            Some(e) => e.seeded = true,
            None => entities.push(EntityRow {
                name: c.name.clone(),
                key: rules.key(&c.name),
                mentions: 0,
                seeded: true,
                distinct: false,
            }),
        }
        for a in &c.aliases {
            if distinct_names.contains(a) {
                report.aliases_skipped_distinct.push(a.clone());
                continue;
            }
            let key = rules.key(a);
            aliases.push(AliasRow {
                name: a.clone(),
                case_sensitive: cs_keys.contains(&key),
                key,
                target: c.name.clone(),
            });
        }
    }
    // Aliases already in the graph (from earlier merges) take part too.
    let graph_aliases = neo4j
        .execute(neo4rs::query(
            "MATCH (a:EntityAlias)-[:ALIAS_OF]->(t:Entity) \
             RETURN a.name AS name, t.name AS target, coalesce(a.case_sensitive, false) AS cs",
        ))
        .await?;
    for r in graph_aliases {
        let name: String = r.get("name").unwrap_or_default();
        if aliases.iter().any(|a| a.name == name) {
            continue;
        }
        aliases.push(AliasRow {
            key: rules.key(&name),
            target: r.get("target").unwrap_or_default(),
            case_sensitive: r.get::<bool>("cs").unwrap_or(false),
            name,
        });
    }

    report.planned = plan_merges(&entities, &aliases);
    // A capitals-only merge stays planned while its lowercase mentions remain;
    // keep it only when something would actually move (or the node is empty).
    let mut planned = Vec::new();
    for m in std::mem::take(&mut report.planned) {
        if let Some(k) = &m.only_upper {
            let left = entities
                .iter()
                .find(|e| e.name == m.from)
                .map(|e| e.mentions)
                .unwrap_or(0);
            if left > 0 && movable_upper(neo4j, &m.from, k).await? == 0 {
                continue;
            }
        }
        planned.push(m);
    }
    report.planned = planned;
    report.canonicals_seeded = rules.canonicals.len();
    report.aliases_seeded = aliases.len();
    report.keys_written = key_updates.len();
    if dry_run {
        return Ok(report);
    }

    // Keys.
    for chunk in key_updates.chunks(2000) {
        neo4j
            .run(
                neo4rs::query("UNWIND $rows AS r MATCH (e:Entity {id: r.id}) SET e.key = r.key")
                    .param("rows", chunk.to_vec()),
            )
            .await
            .context("writing entity keys")?;
    }
    // Canonicals.
    for c in &rules.canonicals {
        let t = c.entity_type.as_deref().map(|t| rules.entity_type(t));
        neo4j
            .run(
                neo4rs::query(
                    "MERGE (e:Entity {name: $name}) \
                     ON CREATE SET e.id = randomUUID(), e.created_at = datetime() \
                     SET e.key = $key, e.type_source = 'seed', \
                         e.entity_type = CASE WHEN $type = '' THEN coalesce(e.entity_type, 'unknown') ELSE $type END",
                )
                .param("name", c.name.as_str())
                .param("key", rules.key(&c.name))
                .param("type", t.unwrap_or_default()),
            )
            .await
            .with_context(|| format!("seeding canonical '{}'", c.name))?;
    }
    // Seed aliases (graph aliases are already there).
    let seed_aliases: Vec<neo4rs::BoltType> = aliases
        .iter()
        .filter(|a| {
            rules
                .canonicals
                .iter()
                .any(|c| c.name == a.target && c.aliases.contains(&a.name))
        })
        .map(|a| {
            let mut m = HashMap::new();
            m.insert("name".to_string(), neo4rs::BoltType::from(a.name.clone()));
            m.insert("key".to_string(), a.key.clone().into());
            m.insert("target".to_string(), a.target.clone().into());
            m.insert("cs".to_string(), a.case_sensitive.into());
            m.into()
        })
        .collect();
    neo4j
        .run(
            neo4rs::query(
                "UNWIND $rows AS r \
                 MATCH (t:Entity {name: r.target}) \
                 MERGE (a:EntityAlias {name: r.name}) \
                 ON CREATE SET a.method = 'seed', a.created_at = datetime() \
                 SET a.key = r.key, a.case_sensitive = r.cs \
                 WITH a, t \
                 OPTIONAL MATCH (a)-[old:ALIAS_OF]->(o) WHERE o <> t \
                 DELETE old \
                 MERGE (a)-[:ALIAS_OF]->(t)",
            )
            .param("rows", seed_aliases),
        )
        .await
        .context("seeding aliases")?;

    if report.planned.is_empty() {
        return Ok(report);
    }
    if let Some(dir) = snapshot_dir {
        let svc = crate::services::snapshot::SnapshotService::new(neo4j.clone(), dir.to_path_buf());
        let (path, _) = svc
            .take_snapshot(Some("pre_entity_merge"))
            .await
            .context("snapshot before entity merges — no merges made")?;
        report.snapshot = Some(path.display().to_string());
    }
    for m in report.planned.clone() {
        match merge(
            neo4j,
            rules,
            &m.from,
            &m.into,
            m.method,
            m.only_upper.as_deref(),
        )
        .await
        {
            Ok(o) => report.merged.push(o),
            Err(e) => report
                .failed
                .push(format!("{} → {}: {e:#}", m.from, m.into)),
        }
    }
    info!(
        merged = report.merged.len(),
        failed = report.failed.len(),
        "Entity resolution applied"
    );
    Ok(report)
}

// ─── Merge candidates (proposals) ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CandEntity {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub mentions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub from_id: String,
    pub into_id: String,
    /// `surname` · `acronym` · `title` · `plural`.
    pub reason: &'static str,
}

const NAME_SUFFIXES: &[&str] = &["jr", "sr", "ii", "iii", "iv"];
const ACRONYM_SKIP: &[&str] = &["of", "the", "and", "for", "on", "in", "&"];

/// Pairs that may be one thing, `from` → `into`. Pure: a person decides.
/// - **surname:** a one-word person whose name is the last word of a longer person name.
/// - **acronym:** a 2–6 letter name that is the initials of a longer one.
/// - **title:** a title before a bare surname (`senator johnson`) and that
///   surname, or a full name ending in it.
/// - **plural:** a name that is another plus `s`.
pub fn find_candidates(entities: &[CandEntity], rules: &Rules) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut by_last: HashMap<String, Vec<&CandEntity>> = HashMap::new();
    let mut by_initials: HashMap<String, Vec<&CandEntity>> = HashMap::new();
    let by_name: HashMap<&str, &CandEntity> =
        entities.iter().map(|e| (e.name.as_str(), e)).collect();
    for e in entities {
        let toks: Vec<String> = e.name.split_whitespace().map(clean_token).collect();
        if toks.len() < 2 {
            continue;
        }
        if e.entity_type == "person" {
            let last = toks
                .iter()
                .rev()
                .find(|t| !NAME_SUFFIXES.contains(&t.as_str()))
                .cloned()
                .unwrap_or_default();
            by_last.entry(last).or_default().push(e);
        }
        let initials: String = toks
            .iter()
            .filter(|t| !ACRONYM_SKIP.contains(&t.as_str()))
            .filter_map(|t| t.chars().next())
            .collect();
        if initials.len() >= 2 {
            by_initials.entry(initials).or_default().push(e);
        }
    }
    for e in entities {
        if e.name.contains(' ') {
            continue;
        }
        if e.entity_type == "person"
            && e.name.chars().count() >= 3
            && let Some(full) = by_last.get(&e.name)
        {
            for f in full {
                out.push(Candidate {
                    from_id: e.id.clone(),
                    into_id: f.id.clone(),
                    reason: "surname",
                });
            }
        }
        let n = e.name.chars().count();
        if (2..=6).contains(&n)
            && e.name.chars().all(|c| c.is_alphabetic())
            && e.entity_type != "person"
            && let Some(long) = by_initials.get(&e.name)
        {
            for f in long.iter().filter(|f| f.entity_type != "person") {
                out.push(Candidate {
                    from_id: e.id.clone(),
                    into_id: f.id.clone(),
                    reason: "acronym",
                });
            }
        }
    }
    for e in entities.iter().filter(|e| e.entity_type == "person") {
        let Some(surname) = rules.titled_surname(&e.name) else {
            continue;
        };
        let bare = by_name
            .get(surname.as_str())
            .filter(|b| b.entity_type == "person");
        for f in bare
            .into_iter()
            .chain(by_last.get(&surname).into_iter().flatten())
        {
            if f.id != e.id {
                out.push(Candidate {
                    from_id: e.id.clone(),
                    into_id: f.id.clone(),
                    reason: "title",
                });
            }
        }
    }
    for e in entities {
        if let Some(stem) = e.name.strip_suffix('s')
            && stem.chars().count() >= 3
            && let Some(s) = by_name.get(stem)
            && s.entity_type == e.entity_type
        {
            out.push(Candidate {
                from_id: e.id.clone(),
                into_id: s.id.clone(),
                reason: "plural",
            });
        }
    }
    out
}

/// GET /api/entities/candidates — proposals, ranked by shared notes, then
/// by how often the rarer of the two is mentioned. Pairs a person already
/// answered (`NOT_SAME`, `RELATED_ENTITY`) are left out. `entity` narrows to
/// pairs that involve one entity.
pub async fn candidates(neo4j: &Neo4jClient, entity: Option<&str>, limit: usize) -> Result<Value> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] \
             WITH e, COUNT { (:Note)-[:MENTIONS]->(e) } AS m WHERE m > 0 \
             RETURN e.id AS id, e.name AS name, coalesce(e.entity_type, 'unknown') AS t, m",
        ))
        .await?;
    let ents: Vec<CandEntity> = rows
        .iter()
        .map(|r| CandEntity {
            id: r.get("id").unwrap_or_default(),
            name: r.get("name").unwrap_or_default(),
            entity_type: r.get("t").unwrap_or_default(),
            mentions: r.get("m").unwrap_or(0),
        })
        .collect();
    let by_id: HashMap<&str, &CandEntity> = ents.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut cands = find_candidates(&ents, Rules::global());
    if let Some(id) = entity {
        cands.retain(|c| c.from_id == id || c.into_id == id);
    }
    if cands.is_empty() {
        return Ok(json!({ "total": 0, "candidates": [], "checked": ents.len() }));
    }

    let pairs: Vec<neo4rs::BoltType> = cands
        .iter()
        .map(|c| {
            let mut m = HashMap::new();
            m.insert("a".to_string(), neo4rs::BoltType::from(c.from_id.clone()));
            m.insert("b".to_string(), c.into_id.clone().into());
            m.into()
        })
        .collect();
    let scored = neo4j
        .execute(
            neo4rs::query(
                "UNWIND $pairs AS p \
                 MATCH (a:Entity {id: p.a}), (b:Entity {id: p.b}) \
                 WHERE NOT (a)-[:NOT_SAME|RELATED_ENTITY]-(b) \
                 RETURN p.a AS a, p.b AS b, \
                        COUNT { (a)<-[:MENTIONS]-(n:Note)-[:MENTIONS]->(b) } AS shared",
            )
            .param("pairs", pairs),
        )
        .await?;
    let shared: HashMap<(String, String), i64> = scored
        .iter()
        .map(|r| {
            (
                (
                    r.get("a").unwrap_or_default(),
                    r.get("b").unwrap_or_default(),
                ),
                r.get::<i64>("shared").unwrap_or(0),
            )
        })
        .collect();

    let mut out: Vec<(i64, i64, Value)> = Vec::new();
    for c in &cands {
        let Some(s) = shared.get(&(c.from_id.clone(), c.into_id.clone())) else {
            continue; // already answered
        };
        // An acronym that never shares a note with its expansion is mostly noise.
        if c.reason == "acronym" && *s < 2 {
            continue;
        }
        let (Some(f), Some(t)) = (by_id.get(c.from_id.as_str()), by_id.get(c.into_id.as_str()))
        else {
            continue;
        };
        // Rivals: other full names the same short name could be.
        let rivals = cands
            .iter()
            .filter(|o| o.from_id == c.from_id && o.into_id != c.into_id)
            .count();
        out.push((
            *s,
            f.mentions.min(t.mentions),
            json!({
                "from": { "id": f.id, "name": f.name, "entity_type": f.entity_type, "mentions": f.mentions },
                "into": { "id": t.id, "name": t.name, "entity_type": t.entity_type, "mentions": t.mentions },
                "reason": c.reason,
                "shared_notes": s,
                "rivals": rivals,
            }),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let total = out.len();
    let list: Vec<Value> = out.into_iter().take(limit).map(|(_, _, v)| v).collect();
    Ok(json!({ "total": total, "candidates": list, "checked": ents.len() }))
}

/// Record a person's answer to a candidate: `different` (NOT_SAME) or
/// `related` (RELATED_ENTITY). Either one stops the pair being proposed again.
pub async fn decide(neo4j: &Neo4jClient, a: &str, b: &str, verdict: &str) -> Result<()> {
    let rel = match verdict {
        "different" => "NOT_SAME",
        "related" => "RELATED_ENTITY",
        other => anyhow::bail!("verdict must be 'different' or 'related', not '{other}'"),
    };
    let rows = neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (a:Entity {{id: $a}}), (b:Entity {{id: $b}}) \
                 MERGE (a)-[r:{rel}]->(b) ON CREATE SET r.decided_at = datetime() \
                 RETURN count(r) AS c"
            ))
            .param("a", a)
            .param("b", b),
        )
        .await?;
    if rows
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0)
        == 0
    {
        anyhow::bail!("no entity with id '{a}' or '{b}'");
    }
    if verdict == "related" {
        // "Related, not the same" is also a relationship the user set.
        crate::services::relations::fold_related_entity(neo4j).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules::from_yaml(
            r#"
titles: [president, former president, senator, sen, dr, mr]
articles: [the]
keep_distinct: [the hacker news]
types: { organization: organisation }
entities:
  - name: United States
    type: location
    aliases: [us, U.S., usa, america]
    case_sensitive: [us]
  - name: donald trump
    type: person
    aliases: [trump]
"#,
        )
        .unwrap()
    }

    #[test]
    fn keys_ignore_case_punctuation_spacing_and_titles() {
        let r = rules();
        assert_eq!(r.key("U.S."), "us");
        assert_eq!(r.key("us"), "us");
        assert_eq!(r.key("Open AI"), "openai");
        assert_eq!(r.key("open-ai"), "openai");
        assert_eq!(r.key("video_learning"), "videolearning");
        assert_eq!(r.key("President Donald Trump"), "donaldtrump");
        assert_eq!(r.key("former president donald trump"), "donaldtrump");
        assert_eq!(r.key("Dr. Rachel Danis"), "racheldanis");
        assert_eq!(
            r.key("China\u{2019}s interim measures"),
            "chinasinterimmeasures"
        );
        assert_eq!(r.key("The Guardian"), "guardian");
    }

    #[test]
    fn a_title_before_a_bare_surname_is_not_folded() {
        let r = rules();
        // Senator Johnson and Speaker Johnson are different people.
        assert_eq!(r.key("Senator Johnson"), "senatorjohnson");
        assert_eq!(r.key("President Trump"), "presidenttrump");
        assert_eq!(r.titled_surname("Sen. Tillis").as_deref(), Some("tillis"));
        assert_eq!(r.titled_surname("Dr. Rachel Danis"), None);
        assert_eq!(r.titled_surname("tillis"), None);
    }

    #[test]
    fn all_digit_keys_never_fold() {
        let ents = vec![ent("9/11", 26), ent("911", 4)];
        assert!(plan_merges(&ents, &[]).is_empty());
    }

    #[test]
    fn a_name_that_is_only_a_title_keeps_its_key() {
        let r = rules();
        assert_eq!(r.key("president"), "president");
        assert_eq!(r.key("the"), "the");
    }

    #[test]
    fn keep_distinct_names_are_never_folded() {
        let r = rules();
        assert_eq!(r.key("The Hacker News"), "the hacker news");
        assert_eq!(r.key("hacker news"), "hackernews");
    }

    #[test]
    fn seed_names_are_normalised_and_types_folded() {
        let r = rules();
        assert_eq!(r.canonicals[0].name, "united states");
        assert!(r.canonicals[0].aliases.contains(&"u.s.".to_string()));
        assert_eq!(r.entity_type("Organization"), "organisation");
        assert_eq!(r.entity_type("Concept"), "concept");
        assert_eq!(r.entity_type(""), "unknown");
        assert!(r.case_sensitive_keys().contains("us"));
    }

    #[test]
    fn capitals_are_detected_as_whole_words() {
        assert!(text_has_upper("Tariffs on the US economy", "us"));
        assert!(text_has_upper("the U.S. said", "us"));
        assert!(text_has_upper("U.S.-based firms", "us"));
        assert!(text_has_upper("(US)", "us"));
        assert!(!text_has_upper("let us know", "us"));
        assert!(!text_has_upper("USB drives", "us"));
        assert!(!text_has_upper("in the USA", "us"));
        assert!(!text_has_upper("BUS stop", "us"));
        assert!(text_has_upper("ICE raids", "ice"));
        assert!(!text_has_upper("ice cream", "ice"));
    }

    fn probe(name: &str, upper: bool) -> Probe {
        Probe {
            name: name.into(),
            key: rules().key(name),
            surface: name.into(),
            entity_type: "organisation".into(),
            upper_in_text: upper,
        }
    }

    #[test]
    fn a_case_sensitive_alias_needs_capitals_in_the_text() {
        let f = Found {
            alias_by_name: Some(("usa-id".into(), true)),
            entity_by_name: Some("pronoun-us".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve(&probe("us", true), &f),
            Target::Existing {
                id: "usa-id".into(),
                via_alias: true
            }
        );
        assert_eq!(
            resolve(&probe("us", false), &f),
            Target::Existing {
                id: "pronoun-us".into(),
                via_alias: false
            }
        );
    }

    #[test]
    fn a_case_sensitive_key_guards_every_alias_sharing_it() {
        // "u.s." is not case-sensitive itself, but it shares key "us" with one that is.
        let f = Found {
            alias_by_key: (vec!["usa-id".into()], true),
            ..Default::default()
        };
        assert_eq!(resolve(&probe("u. s", false), &f), Target::Create);
        assert_eq!(
            resolve(&probe("u. s", true), &f),
            Target::Existing {
                id: "usa-id".into(),
                via_alias: true
            }
        );
    }

    #[test]
    fn resolution_order_is_alias_then_name_then_key() {
        let f = Found {
            entity_by_name: Some("by-name".into()),
            entity_by_key: Some("by-key".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve(&probe("openai", false), &f),
            Target::Existing {
                id: "by-name".into(),
                via_alias: false
            }
        );
        let f = Found {
            entity_by_key: Some("by-key".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve(&probe("open ai", false), &f),
            Target::Existing {
                id: "by-key".into(),
                via_alias: false
            }
        );
        assert_eq!(
            resolve(&probe("new thing", false), &Found::default()),
            Target::Create
        );
    }

    fn ent(name: &str, mentions: i64) -> EntityRow {
        EntityRow {
            name: name.into(),
            key: rules().key(name),
            mentions,
            seeded: false,
            distinct: false,
        }
    }
    fn seed_aliases() -> Vec<AliasRow> {
        let r = rules();
        let cs = r.case_sensitive_keys();
        r.canonicals
            .iter()
            .flat_map(|c| {
                c.aliases.iter().map(|a| AliasRow {
                    name: a.clone(),
                    key: r.key(a),
                    target: c.name.clone(),
                    case_sensitive: cs.contains(&r.key(a)),
                })
            })
            .collect()
    }

    #[test]
    fn the_seed_folds_aliases_into_the_canonical() {
        let mut us = ent("united states", 60);
        us.seeded = true;
        let mut dt = ent("donald trump", 149);
        dt.seeded = true;
        let ents = vec![
            us,
            dt,
            ent("us", 137),
            ent("u.s.", 52),
            ent("usa", 13),
            ent("trump", 207),
            ent("president donald trump", 2),
            ent("kai trump", 6),
        ];
        let m = plan_merges(&ents, &seed_aliases());
        let find = |f: &str| m.iter().find(|x| x.from == f).cloned();
        // Case-sensitive key: capitals-only move, and the node stays.
        assert_eq!(find("us").unwrap().only_upper.as_deref(), Some("us"));
        assert_eq!(find("u.s.").unwrap().only_upper.as_deref(), Some("us"));
        assert_eq!(find("usa").unwrap().into, "united states");
        assert!(find("usa").unwrap().only_upper.is_none());
        // A title before a full name folds by key.
        assert_eq!(find("trump").unwrap().into, "donald trump");
        assert_eq!(find("president donald trump").unwrap().into, "donald trump");
        assert!(find("kai trump").is_none());
        assert!(find("united states").is_none() && find("donald trump").is_none());
    }

    #[test]
    fn same_key_folds_into_the_most_mentioned() {
        let ents = vec![
            ent("openai", 56),
            ent("open ai", 1),
            ent("deepfakes", 19),
            ent("deep fakes", 5),
        ];
        let m = plan_merges(&ents, &[]);
        assert_eq!(m.len(), 2);
        assert!(m.contains(&PlannedMerge {
            from: "open ai".into(),
            into: "openai".into(),
            method: "normalise",
            only_upper: None
        }));
        assert!(m.contains(&PlannedMerge {
            from: "deep fakes".into(),
            into: "deepfakes".into(),
            method: "normalise",
            only_upper: None
        }));
    }

    #[test]
    fn a_split_out_entity_is_left_alone() {
        let mut restored = ent("open ai", 1);
        restored.distinct = true;
        restored.key = "~open ai".into();
        assert!(plan_merges(&[ent("openai", 56), restored], &[]).is_empty());
    }

    #[test]
    fn a_canonical_that_does_not_exist_yet_is_not_a_target() {
        let ents = vec![ent("usa", 13)];
        assert!(plan_merges(&ents, &seed_aliases()).is_empty());
    }

    #[test]
    fn chains_resolve_to_the_final_target() {
        // "open-ai" folds into "open ai" by key while "open ai" is itself an
        // alias of "openai": both end at "openai".
        let ents = vec![ent("openai", 56), ent("open ai", 1), ent("open_ai", 1)];
        let aliases = vec![AliasRow {
            name: "open ai".into(),
            key: "openai".into(),
            target: "openai".into(),
            case_sensitive: false,
        }];
        let m = plan_merges(&ents, &aliases);
        assert!(m.iter().all(|x| x.into == "openai"), "{m:?}");
        assert_eq!(m.len(), 2);
    }

    fn ce(id: &str, name: &str, t: &str) -> CandEntity {
        CandEntity {
            id: id.into(),
            name: name.into(),
            entity_type: t.into(),
            mentions: 1,
        }
    }

    #[test]
    fn candidates_cover_surnames_acronyms_and_plurals() {
        let ents = vec![
            ce("1", "vance", "person"),
            ce("2", "jd vance", "person"),
            ce("3", "ron vance", "person"),
            ce("4", "rfk", "person"),
            ce("5", "nato", "organisation"),
            ce("6", "north atlantic treaty organisation", "organisation"),
            ce("7", "llms", "technology"),
            ce("8", "llm", "technology"),
            ce("9", "martin luther king jr", "person"),
            ce("10", "king", "person"),
            ce("11", "news", "concept"),
            ce("12", "new", "organisation"),
            ce("13", "senator tillis", "person"),
            ce("14", "tillis", "person"),
            ce("15", "thom tillis", "person"),
        ];
        let c = find_candidates(&ents, &rules());
        let has = |f: &str, t: &str, r: &str| {
            c.iter()
                .any(|x| x.from_id == f && x.into_id == t && x.reason == r)
        };
        assert!(has("1", "2", "surname") && has("1", "3", "surname"));
        assert!(
            has("10", "9", "surname"),
            "a suffix is skipped to find the last name"
        );
        assert!(has("5", "6", "acronym"));
        assert!(has("7", "8", "plural"));
        assert!(has("13", "14", "title") && has("13", "15", "title"));
        assert!(has("14", "15", "surname"));
        assert!(
            !c.iter().any(|x| x.from_id == "11"),
            "a plural must share the type"
        );
        assert!(
            !c.iter().any(|x| x.from_id == "4" && x.reason == "acronym"),
            "people are not acronyms"
        );
    }
}
