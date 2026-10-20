//! Todo tags are references to things the brain has (todo fec2fc34,
//! 2026-10-20): a tag is `kind:name` (`workflow:daily-news`,
//! `agent:researcher`, `entity:palantir`), never a free word. A tag says what
//! the item is *for*: what it serves or benefits. Who *does* it stays the
//! item's `assignee`, and the program it counts on stays `program`.
//!
//! - `GET /api/refs?q=&kinds=&limit=` — search every kind as the user types.
//! - `POST /api/refs/suggest {text}` — the refs a title and description name,
//!   in code (no model): names of workflows, agents, programs, topics, tools
//!   and skills found in the text, and the most-mentioned entities it names.
//! - `GET /api/refs/resolve?refs=a,b` — labels and drawer links for saved tags.
//! - `GET /api/refs/todos?ref=` — the items tagged with one ref.
//!
//! On every todo write, [`check_tags`] admits a tag that names something that
//! exists, or one the item already had (the free words from before
//! 2026-10-20 stay until removed, and cannot be added again); [`sync_links`]
//! mirrors graph-backed tags as `(:Todo)-[:TAGGED]->(node)` so graph queries
//! reach the items that serve a node. The `tags` JSON stays the record.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::Query,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, graph, or_500};

/// The kinds a tag may name, in the order the picker shows them.
pub const KINDS: &[&str] = &[
    "workflow", "agent", "program", "topic", "entity", "tool", "skill",
];

const DEFAULT_LIMIT: i64 = 6;
const MAX_LIMIT: i64 = 25;
/// Entity search needs this many characters: 11K entities match one letter.
const ENTITY_MIN_QUERY: usize = 2;
const MAX_SUGGESTIONS: usize = 10;

/// A graph-backed kind: its label, the drawer that opens it and the property
/// that drawer takes. Every one is keyed by a unique `name`.
struct GraphKind {
    label: &'static str,
    drawer: Option<&'static str>,
    open_id: &'static str,
}

fn graph_kind(kind: &str) -> Option<GraphKind> {
    let (label, drawer, open_id) = match kind {
        "workflow" => ("Workflow", Some("schedule"), "n.id"),
        "agent" => ("ContextProfile", None, "n.name"),
        "program" => ("Program", Some("program"), "n.name"),
        "topic" => ("Topic", Some("topic"), "n.name"),
        "entity" => ("Entity", Some("entity"), "n.id"),
        _ => return None,
    };
    Some(GraphKind {
        label,
        drawer,
        open_id,
    })
}

/// `kind:name` → `(kind, name)` when the kind is one of [`KINDS`].
pub fn parse(tag: &str) -> Option<(&str, &str)> {
    let (kind, name) = tag.split_once(':')?;
    (KINDS.contains(&kind) && !name.trim().is_empty()).then_some((kind, name))
}

/// One thing a tag can name.
fn ref_json(
    kind: &str,
    name: &str,
    label: &str,
    detail: &str,
    open: Option<(&str, String)>,
) -> Value {
    json!({
        "ref": format!("{kind}:{name}"),
        "kind": kind,
        "name": name,
        "label": if label.is_empty() { name } else { label },
        "detail": super::work::clip(detail, 90).0,
        "open": open.map(|(k, id)| json!({"kind": k, "id": id})),
    })
}

/// Lowercase, every non-alphanumeric run as one space, padded: so a name
/// matches the text on word boundaries (`daily-news` in "the daily news").
fn words(s: &str) -> String {
    let mut out = String::from(" ");
    let mut gap = false;
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            gap = false;
        } else if !gap {
            out.push(' ');
            gap = true;
        }
    }
    if !gap {
        out.push(' ');
    }
    out
}

/// The 1–4 word runs of `text`, for matching entity names exactly. A single
/// word needs four letters: shorter ones are mostly function words.
fn grams(text: &str) -> Vec<String> {
    let hay = words(text);
    let ws: Vec<&str> = hay.split_whitespace().collect();
    let mut out = HashSet::new();
    for n in 1..=4 {
        for w in ws.windows(n) {
            if n == 1 && w[0].chars().count() < 4 {
                continue;
            }
            out.insert(w.join(" "));
        }
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    v
}

/// Search one graph kind; an empty `q` lists it (entities never: too many).
async fn search_graph(
    neo4j: &agent_brain_repository::Neo4jClient,
    kind: &str,
    q: &str,
    limit: i64,
) -> anyhow::Result<Vec<Value>> {
    let Some(g) = graph_kind(kind) else {
        return Ok(Vec::new());
    };
    let cypher = if kind == "entity" {
        if q.chars().count() < ENTITY_MIN_QUERY {
            return Ok(Vec::new());
        }
        format!(
            "MATCH (n:Entity) WHERE n.name CONTAINS $q AND NOT n.entity_type IN ['url', 'date'] \
             WITH n LIMIT 300 \
             WITH n, COUNT {{ (:Note)-[:MENTIONS]->(n) }} AS m \
             RETURN n.name AS name, n.name AS label, \
                    coalesce(n.entity_type, '') + ' · ' + toString(m) + ' notes' AS detail, \
                    {} AS open_id \
             ORDER BY CASE WHEN n.name STARTS WITH $q THEN 0 ELSE 1 END, m DESC, size(n.name) \
             LIMIT $limit",
            g.open_id
        )
    } else {
        format!(
            "MATCH (n:{label}) \
             WHERE $q = '' OR toLower(n.name) CONTAINS $q OR toLower(coalesce(n.label, '')) CONTAINS $q \
             RETURN n.name AS name, coalesce(n.label, n.name) AS label, \
                    coalesce(n.description, n.kind, '') AS detail, {open} AS open_id \
             ORDER BY CASE WHEN toLower(n.name) STARTS WITH $q THEN 0 ELSE 1 END, size(n.name), n.name \
             LIMIT $limit",
            label = g.label,
            open = g.open_id
        )
    };
    let rows = neo4j
        .execute(neo4rs::query(&cypher).param("q", q).param("limit", limit))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let name: String = r.get("name").ok()?;
            let label: String = r.get("label").unwrap_or_default();
            let detail: String = r.get("detail").unwrap_or_default();
            let open = g.drawer.zip(r.get::<String>("open_id").ok());
            Some(ref_json(kind, &name, &label, &detail, open))
        })
        .collect())
}

/// The registry's tools and skills: `(name, detail)` per kind.
async fn registry_refs(state: &RestState) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let Some(reg) = state.tool_registry.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    let reg = reg.read().await;
    let tools = reg
        .list()
        .into_iter()
        .map(|t| (t.name.clone(), t.description.clone()))
        .collect();
    let skills = reg
        .list_skills()
        .into_iter()
        .filter_map(|s| {
            let name = s["name"].as_str()?.to_string();
            let tools: Vec<&str> = s["tools"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some((name, format!("{} tools: {}", tools.len(), tools.join(", "))))
        })
        .collect();
    (tools, skills)
}

fn search_registry(kind: &str, list: &[(String, String)], q: &str, limit: usize) -> Vec<Value> {
    let mut hits: Vec<&(String, String)> = list
        .iter()
        .filter(|(n, _)| q.is_empty() || n.to_lowercase().contains(q))
        .collect();
    hits.sort_by_key(|(n, _)| (!n.to_lowercase().starts_with(q), n.len(), n.clone()));
    hits.into_iter()
        .take(limit)
        .map(|(n, d)| ref_json(kind, n, n, d, None))
        .collect()
}

fn wanted_kinds(params: &HashMap<String, String>) -> Vec<&'static str> {
    let asked: Vec<&str> = params
        .get("kinds")
        .map(|k| k.split(',').map(str::trim).collect())
        .unwrap_or_default();
    KINDS
        .iter()
        .copied()
        .filter(|k| asked.is_empty() || asked.contains(k))
        .collect()
}

/// GET /api/refs?q=&kinds=&limit= — matches per kind, grouped in [`KINDS`]
/// order. `limit` is per kind.
pub async fn handle_search(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let neo4j = graph!(state);
    let q = params
        .get("q")
        .map(|q| q.trim().to_lowercase())
        .unwrap_or_default();
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let (tools, skills) = registry_refs(&state).await;
    let mut refs = Vec::new();
    for kind in wanted_kinds(&params) {
        match kind {
            "tool" => refs.extend(search_registry(kind, &tools, &q, limit as usize)),
            "skill" => refs.extend(search_registry(kind, &skills, &q, limit as usize)),
            _ => refs.extend(or_500!(search_graph(neo4j, kind, &q, limit).await)),
        }
    }
    let note = (q.chars().count() < ENTITY_MIN_QUERY)
        .then_some("Entities are searched from two characters.");
    Json(json!({ "q": q, "refs": refs, "note": note })).into_response()
}

/// What each of `tags` names, in order: `exists: false` for a tag naming
/// nothing (a free word from before 2026-10-20, or a deleted thing).
pub async fn resolve(state: &RestState, tags: &[String]) -> anyhow::Result<Vec<Value>> {
    let neo4j = state
        .neo4j
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the graph is not available"))?;
    let (tools, skills) = registry_refs(state).await;
    let mut out = Vec::new();
    for tag in tags {
        let found = match parse(tag) {
            None => None,
            Some((kind @ ("tool" | "skill"), name)) => {
                let list = if kind == "tool" { &tools } else { &skills };
                list.iter()
                    .find(|(n, _)| n == name)
                    .map(|(n, d)| ref_json(kind, n, n, d, None))
            }
            Some((kind, name)) => {
                let g = graph_kind(kind).expect("a parsed kind is a graph kind or the registry's");
                let rows = neo4j
                    .execute(
                        neo4rs::query(&format!(
                            "MATCH (n:{}) WHERE n.name = $name \
                             RETURN n.name AS name, coalesce(n.label, n.name) AS label, \
                                    coalesce(n.description, n.kind, n.entity_type, '') AS detail, \
                                    {} AS open_id LIMIT 1",
                            g.label, g.open_id
                        ))
                        .param("name", name),
                    )
                    .await?;
                rows.first().map(|r| {
                    let open = g.drawer.zip(r.get::<String>("open_id").ok());
                    ref_json(
                        kind,
                        name,
                        &r.get::<String>("label").unwrap_or_default(),
                        &r.get::<String>("detail").unwrap_or_default(),
                        open,
                    )
                })
            }
        };
        out.push(match found {
            Some(mut v) => {
                v["exists"] = json!(true);
                v
            }
            None => json!({
                "ref": tag,
                "kind": parse(tag).map(|(k, _)| k),
                "label": parse(tag).map_or(tag.as_str(), |(_, n)| n),
                "exists": false,
                "legacy": parse(tag).is_none(),
            }),
        });
    }
    Ok(out)
}

/// GET /api/refs/resolve?refs=a,b
pub async fn handle_resolve(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let tags: Vec<String> = params
        .get("refs")
        .map(|r| {
            r.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Json(json!({ "refs": or_500!(resolve(&state, &tags).await) })).into_response()
}

/// The tags a todo write may save: each must name something that exists, or
/// already be on the item (`old`). Duplicates are dropped. `Err` names every
/// rejected tag and how to find a real one.
pub async fn check_tags(
    state: &RestState,
    new: &[String],
    old: &[String],
) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let tags: Vec<String> = new
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();
    let fresh: Vec<String> = tags.iter().filter(|t| !old.contains(t)).cloned().collect();
    if fresh.is_empty() {
        return Ok(tags);
    }
    let found = resolve(state, &fresh)
        .await
        .map_err(|e| format!("could not check the tags: {e}"))?;
    let bad: Vec<String> = found
        .iter()
        .filter(|r| r["exists"] != json!(true))
        .map(|r| {
            let tag = r["ref"].as_str().unwrap_or_default();
            if parse(tag).is_some() {
                format!("'{tag}' names nothing that exists")
            } else {
                format!("'{tag}' is not a reference")
            }
        })
        .collect();
    if bad.is_empty() {
        Ok(tags)
    } else {
        Err(format!(
            "{}. A tag names an existing {} as kind:name (search them with GET /api/refs?q=).",
            bad.join("; "),
            KINDS.join(", ")
        ))
    }
}

/// Mirror the item's graph-backed tags as `(:Todo)-[:TAGGED]->(node)`.
pub async fn sync_links(
    neo4j: &agent_brain_repository::Neo4jClient,
    todo_id: &str,
    tags: &[String],
) -> anyhow::Result<()> {
    neo4j
        .run(neo4rs::query("MATCH (:Todo {id: $id})-[r:TAGGED]->() DELETE r").param("id", todo_id))
        .await?;
    let mut by_kind: HashMap<&str, Vec<String>> = HashMap::new();
    for (kind, name) in tags.iter().filter_map(|t| parse(t)) {
        if graph_kind(kind).is_some() {
            by_kind.entry(kind).or_default().push(name.to_string());
        }
    }
    for (kind, names) in by_kind {
        let label = graph_kind(kind).map(|g| g.label).unwrap_or_default();
        neo4j
            .run(
                neo4rs::query(&format!(
                    "MATCH (t:Todo {{id: $id}}) UNWIND $names AS name \
                     MATCH (n:{label} {{name: name}}) MERGE (t)-[r:TAGGED]->(n) \
                     SET r.created_at = coalesce(r.created_at, datetime())"
                ))
                .param("id", todo_id)
                .param("names", names),
            )
            .await?;
    }
    Ok(())
}

/// POST /api/refs/suggest {text, limit?} — refs the text names, in code.
pub async fn handle_suggest(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = graph!(state);
    let text = body["text"].as_str().unwrap_or_default();
    if text.trim().is_empty() {
        return Json(json!({ "refs": [] })).into_response();
    }
    let hay = words(text);
    let limit = body["limit"]
        .as_u64()
        .map_or(MAX_SUGGESTIONS, |l| (l as usize).clamp(1, MAX_SUGGESTIONS));
    let mut out = Vec::new();
    let mut named: HashSet<String> = HashSet::new();
    for kind in ["workflow", "agent", "program", "topic"] {
        for r in or_500!(search_graph(neo4j, kind, "", 500).await) {
            let name = r["name"].as_str().unwrap_or_default();
            let label = r["label"].as_str().unwrap_or_default();
            let hit = [name, label].iter().any(|n| {
                let w = words(n);
                w.trim().chars().count() >= 3 && hay.contains(&w)
            });
            if hit && named.insert(words(name)) {
                out.push(r);
            }
        }
    }
    // A tool or skill is suggested only by its exact identifier
    // (`search_web`), not by the common word some are named after (`reason`).
    let raw = format!(" {} ", text.to_lowercase());
    let (tools, skills) = registry_refs(&state).await;
    for (kind, list) in [("tool", &tools), ("skill", &skills)] {
        for (n, d) in list.iter() {
            let ident = n.to_lowercase();
            let distinctive = ident.contains('_') || n.chars().any(char::is_uppercase);
            if distinctive && contains_word(&raw, &ident) && named.insert(words(n)) {
                out.push(ref_json(kind, n, n, d, None));
            }
        }
    }
    let rows = or_500!(
        neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Entity) WHERE n.name IN $grams AND NOT n.entity_type IN ['url', 'date'] \
                     WITH n, COUNT { (:Note)-[:MENTIONS]->(n) } AS m WHERE m >= 2 \
                     RETURN n.name AS name, n.id AS id, \
                            coalesce(n.entity_type, '') + ' · ' + toString(m) + ' notes' AS detail \
                     ORDER BY size(split(n.name, ' ')) DESC, m DESC LIMIT 6",
                )
                .param("grams", grams(text)),
            )
            .await
    );
    for r in rows {
        let name: String = r.get("name").unwrap_or_default();
        if named.insert(words(&name)) {
            let detail: String = r.get("detail").unwrap_or_default();
            out.push(ref_json(
                "entity",
                &name,
                &name,
                &detail,
                r.get::<String>("id").ok().map(|id| ("entity", id)),
            ));
        }
    }
    out.truncate(limit);
    Json(json!({ "refs": out })).into_response()
}

/// `word` in `hay` with no identifier character either side.
fn contains_word(hay: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    hay.match_indices(word).any(|(i, _)| {
        let before = hay[..i].chars().next_back().is_none_or(|c| !ident(c));
        let after = hay[i + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !ident(c));
        before && after
    })
}

/// GET /api/refs/todos?ref=kind:name — the items tagged with it, open first.
pub async fn handle_todos(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let neo4j = graph!(state);
    let Some(tag) = params.get("ref").filter(|r| parse(r).is_some()) else {
        return bad_request(format!(
            "`ref` must be kind:name, a kind one of {}",
            KINDS.join(", ")
        ));
    };
    let mut todos: Vec<_> = or_500!(neo4j.list_todos(None).await)
        .into_iter()
        .filter(|t| t.tags.iter().any(|x| x == tag))
        .collect();
    todos.sort_by_key(|t| (t.status == "done", t.priority));
    let items: Vec<Value> = todos
        .iter()
        .map(agent_brain_scheduler::items::item_json)
        .collect();
    Json(json!({ "ref": tag, "todos": items })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_a_known_kind_and_a_name() {
        assert_eq!(
            parse("workflow:daily-news"),
            Some(("workflow", "daily-news"))
        );
        assert_eq!(
            parse("entity:new york city"),
            Some(("entity", "new york city"))
        );
        assert_eq!(parse("HBI"), None, "a free word is not a reference");
        assert_eq!(parse("colour:red"), None, "unknown kind");
        assert_eq!(parse("agent: "), None, "no name");
    }

    #[test]
    fn names_match_on_word_boundaries() {
        let hay = words("Fix the Daily-News brief; ask the researcher");
        assert!(hay.contains(&words("daily-news")));
        assert!(hay.contains(&words("researcher")));
        assert!(!hay.contains(&words("news brief fix")));
        assert!(!words("the newsroom").contains(&words("news")));
    }

    #[test]
    fn entity_grams_skip_short_single_words() {
        let g = grams("The ACLU of NJ sued");
        assert!(g.contains(&"aclu".to_string()));
        assert!(g.contains(&"aclu of nj".to_string()));
        assert!(
            !g.contains(&"the".to_string()),
            "short single words are skipped"
        );
        assert!(
            g.contains(&"the aclu".to_string()),
            "multi-word runs keep them"
        );
    }

    #[test]
    fn tools_match_their_exact_identifier() {
        assert!(contains_word(" use search_web here ", "search_web"));
        assert!(!contains_word(" use search_web_v2 ", "search_web"));
        assert!(contains_word(" (search_web). ", "search_web"));
    }
}
