//! Reference links: which Wikidata item (and English Wikipedia article) an
//! entity is (HBI plan Phase 18, §11).
//!
//! - **Proposals, like relationships.** The model's pick is `proposed`; the
//!   user confirms it, rejects it, chooses another candidate or pastes a URL.
//!   A rejected item is never offered to the model for that entity again.
//! - **Public sources only, in the background.** A lookup sends the entity's
//!   name to Wikimedia. `link_entities` takes only entities a claim mentions
//!   (claims come from videos, articles and news); any other entity is looked
//!   up only when the user presses "Look up" (`on_demand`).
//! - **Measured before building** (§11.1, 53 entities): the top search result
//!   was wrong 13 of 37 times; the cloud model choosing from the candidates
//!   with claim context was right 33 of 35, with 0 wrong.
//! - **Polite to Wikimedia.** One request at a time, at least a second apart,
//!   a contact in the user agent (`WIKIMEDIA_CONTACT`, required), and a 429
//!   pauses every lookup for its `Retry-After`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::db::{Db, Row, query};
use agent_brain_plugin_api::Llm as LlmProvider;

/// Bumped when the pick prompt changes; recorded on each lookup.
pub const PROMPT_VERSION: &str = "v4";
const WIKIDATA_API: &str = "https://www.wikidata.org/w/api.php";
const SUMMARY_API: &str = "https://en.wikipedia.org/api/rest_v1/page/summary/";
const SEARCH_LIMIT: &str = "7";
const MAX_CANDIDATES: usize = 10;
/// Instance-of classes kept per item for the kind walk.
const MAX_CLASSES: usize = 6;
/// Entity types that get no model-suggested second search (see `link_one`).
const NO_SECOND_SEARCH: &[&str] = &["concept", "event", "programme"];
const CONTEXT_TEXTS: i64 = 3;
const CONTEXT_CHARS: usize = 400;
const MIN_SPACING: Duration = Duration::from_millis(1100);
/// A "no article" answer is checked again after this long: new things get articles.
pub const RECHECK_NONE_DAYS: i64 = 90;
const SUMMARY_TTL_DAYS: i64 = 30;
/// An on-demand lookup still waiting after this long is shown as not pending.
const PENDING_MINUTES: i64 = 15;

// ─── Wikimedia access ─────────────────────────────────────────────────────────

/// The contact Wikimedia asks automated clients to send. Lookups refuse to run
/// without one.
pub fn contact() -> Option<String> {
    std::env::var("WIKIMEDIA_CONTACT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

/// Wikimedia answered 429: every lookup waits `retry_after` seconds.
#[derive(Debug)]
pub struct Throttled {
    pub retry_after: u64,
}

impl std::fmt::Display for Throttled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wikimedia is rate-limiting us; retry after {} s",
            self.retry_after
        )
    }
}

impl std::error::Error for Throttled {}

struct Gate {
    next: Instant,
}

fn gate() -> &'static tokio::sync::Mutex<Gate> {
    static G: OnceLock<tokio::sync::Mutex<Gate>> = OnceLock::new();
    G.get_or_init(|| {
        tokio::sync::Mutex::new(Gate {
            next: Instant::now(),
        })
    })
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// One GET to Wikimedia, through the shared gate: requests are serialised,
/// spaced, and paused after a 429 (the error is [`Throttled`]).
async fn get(url: &str, query: &[(&str, &str)]) -> Result<Value> {
    let contact =
        contact().context("WIKIMEDIA_CONTACT is not set; Wikimedia asks for a contact")?;
    let mut g = gate().lock().await;
    let now = Instant::now();
    if g.next > now {
        let wait = g.next - now;
        if wait > Duration::from_secs(5) {
            bail!(Throttled {
                retry_after: wait.as_secs()
            });
        }
        tokio::time::sleep(wait).await;
    }
    let resp = http()
        .get(url)
        .query(query)
        .header(
            reqwest::header::USER_AGENT,
            format!("agent-brain/0.1 (personal knowledge graph; {contact})"),
        )
        .send()
        .await;
    g.next = Instant::now() + MIN_SPACING;
    let resp = resp.context("Wikimedia request failed")?;
    let status = resp.status();
    if status.as_u16() == 429 {
        let secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60);
        g.next = Instant::now() + Duration::from_secs(secs);
        bail!(Throttled { retry_after: secs });
    }
    if status.as_u16() == 404 {
        return Ok(Value::Null);
    }
    if !status.is_success() {
        bail!("Wikimedia answered {status}");
    }
    let v: Value = resp.json().await.context("Wikimedia answer was not JSON")?;
    if let Some(e) = v.get("error") {
        bail!("Wikimedia error: {e}");
    }
    Ok(v)
}

/// English Wikipedia's action API, through the same gate.
pub(crate) async fn wikipedia(params: &[(&str, &str)]) -> Result<Value> {
    let mut q: Vec<(&str, &str)> = vec![("format", "json"), ("formatversion", "2")];
    q.extend_from_slice(params);
    get("https://en.wikipedia.org/w/api.php", &q).await
}

async fn wikidata(params: &[(&str, &str)]) -> Result<Value> {
    let mut q: Vec<(&str, &str)> = vec![("format", "json")];
    q.extend_from_slice(params);
    get(WIKIDATA_API, &q).await
}

/// A Wikidata item as the model and the HBI see it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Item {
    pub qid: String,
    pub label: String,
    pub description: String,
    /// Labels of its instance-of (P31) values, at most 3.
    pub kinds: Vec<String>,
    /// The instance-of (P31) ids themselves, which `entity_kinds` walks up to
    /// a kind (HBI plan Phase 19).
    pub class_qids: Vec<String>,
    pub aliases: Vec<String>,
    /// English Wikipedia title, when it has an article.
    pub wiki_title: Option<String>,
    /// Where it is (P625), when Wikidata says (MAP_PLAN Phase 1).
    pub location: Option<LatLon>,
    /// How many coordinate claims it has; more than one is said, not hidden.
    pub coord_count: usize,
}

/// A WGS 84 point.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

/// An item's coordinate location (P625): the preferred-rank claim, else the
/// first normal one, never a deprecated one, and on Earth only (Wikidata also
/// places craters on the Moon and Mars). With how many claims it had.
pub(crate) fn coord_of(e: &Value) -> (Option<LatLon>, usize) {
    let claims: Vec<&Value> = e["claims"]["P625"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["rank"].as_str() != Some("deprecated"))
        .collect();
    let on_earth = |c: &&&Value| {
        c["mainsnak"]["datavalue"]["value"]["globe"]
            .as_str()
            .is_none_or(|g| g.ends_with("/Q2"))
    };
    let pick = claims
        .iter()
        .filter(on_earth)
        .find(|c| c["rank"].as_str() == Some("preferred"))
        .or_else(|| claims.iter().find(on_earth));
    let point = pick
        .and_then(|c| {
            let v = &c["mainsnak"]["datavalue"]["value"];
            Some(LatLon {
                lat: v["latitude"].as_f64()?,
                lon: v["longitude"].as_f64()?,
            })
        })
        .filter(|p| p.lat.abs() <= 90.0 && p.lon.abs() <= 180.0);
    (point, claims.len())
}

/// The coordinate location of each of `ids` (50 per request). An item
/// Wikidata no longer has comes back with none, so it is not asked again.
pub(crate) async fn coords_of(ids: &[String]) -> Result<HashMap<String, (Option<LatLon>, usize)>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(50) {
        let joined = chunk.join("|");
        let v = wikidata(&[
            ("action", "wbgetentities"),
            ("ids", &joined),
            ("props", "claims"),
        ])
        .await?;
        for id in chunk {
            let e = &v["entities"][id.as_str()];
            out.insert(
                id.clone(),
                if e.get("missing").is_some() {
                    (None, 0)
                } else {
                    coord_of(e)
                },
            );
        }
    }
    Ok(out)
}

/// The English label, else the language-neutral one.
fn label(e: &Value) -> Option<&str> {
    e["labels"]["en"]["value"]
        .as_str()
        .or_else(|| e["labels"]["mul"]["value"].as_str())
}

/// Labels, descriptions, aliases, kinds and article titles for `ids`, in order.
async fn details(ids: &[String]) -> Result<Vec<Item>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let joined = ids.join("|");
    let v = wikidata(&[
        ("action", "wbgetentities"),
        ("ids", &joined),
        ("props", "labels|descriptions|aliases|claims|sitelinks"),
        // Wikidata keeps many names only as a language-neutral `mul` label
        // (Q937, Albert Einstein, has no `en` label at all).
        ("languages", "en|mul"),
        ("sitefilter", "enwiki"),
    ])
    .await?;
    let ents = &v["entities"];
    let p31 = |e: &Value| -> Vec<String> { claim_ids(e, "P31").into_iter().take(3).collect() };
    let kind_ids: Vec<String> = ids
        .iter()
        .flat_map(|i| p31(&ents[i]))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut kind_labels: HashMap<String, String> = HashMap::new();
    for chunk in kind_ids.chunks(50) {
        let joined = chunk.join("|");
        let k = wikidata(&[
            ("action", "wbgetentities"),
            ("ids", &joined),
            ("props", "labels"),
            ("languages", "en|mul"),
        ])
        .await?;
        for (id, e) in k["entities"].as_object().into_iter().flatten() {
            if let Some(l) = label(e) {
                kind_labels.insert(id.clone(), l.to_string());
            }
        }
    }
    Ok(ids
        .iter()
        .filter(|i| ents[i.as_str()].get("missing").is_none())
        .map(|i| {
            let e = &ents[i.as_str()];
            let (location, coord_count) = coord_of(e);
            Item {
                qid: i.clone(),
                label: label(e).unwrap_or(i).to_string(),
                description: e["descriptions"]["en"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                kinds: p31(e)
                    .iter()
                    .filter_map(|k| kind_labels.get(k).cloned())
                    .collect(),
                class_qids: claim_ids(e, "P31").into_iter().take(MAX_CLASSES).collect(),
                aliases: e["aliases"]["en"]
                    .as_array()
                    .into_iter()
                    .chain(e["aliases"]["mul"].as_array())
                    .flatten()
                    .filter_map(|a| a["value"].as_str())
                    .take(8)
                    .map(String::from)
                    .collect(),
                wiki_title: e["sitelinks"]["enwiki"]["title"].as_str().map(String::from),
                location,
                coord_count,
            }
        })
        .collect())
}

/// The item ids a claim (`P31` instance of, `P279` subclass of) points at.
fn claim_ids(e: &Value, prop: &str) -> Vec<String> {
    e["claims"][prop]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["mainsnak"]["datavalue"]["value"]["id"].as_str())
        .map(String::from)
        .collect()
}

/// The instance-of (P31) ids of items stored before Phase 19 kept them.
pub(crate) async fn class_qids_of(ids: &[String]) -> Result<HashMap<String, Vec<String>>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(50) {
        let joined = chunk.join("|");
        let v = wikidata(&[
            ("action", "wbgetentities"),
            ("ids", &joined),
            ("props", "claims"),
        ])
        .await?;
        for (id, e) in v["entities"].as_object().into_iter().flatten() {
            if e.get("missing").is_none() {
                let c: Vec<String> = claim_ids(e, "P31").into_iter().take(MAX_CLASSES).collect();
                out.insert(id.clone(), c);
            }
        }
    }
    Ok(out)
}

/// A Wikidata class: its label and its subclass-of (P279) parents.
#[derive(Debug, Clone)]
pub(crate) struct ClassInfo {
    pub qid: String,
    pub label: String,
    pub parents: Vec<String>,
}

/// Labels and parents of up to 50 classes: one request.
pub(crate) async fn class_info(ids: &[String]) -> Result<Vec<ClassInfo>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let joined = ids.join("|");
    let v = wikidata(&[
        ("action", "wbgetentities"),
        ("ids", &joined),
        ("props", "labels|claims"),
        ("languages", "en|mul"),
    ])
    .await?;
    Ok(ids
        .iter()
        .map(|i| {
            let e = &v["entities"][i.as_str()];
            ClassInfo {
                qid: i.clone(),
                label: label(e).unwrap_or(i).to_string(),
                // A missing or deleted class has no parents: the walk stops there.
                parents: claim_ids(e, "P279"),
            }
        })
        .collect())
}

/// Wikimedia's own pages (a disambiguation page, a set index, a list) are a
/// list of things, never one. Caught here by their class label so a lookup
/// costs no extra walk; `entity_kinds` also refuses them by class.
pub fn is_wikimedia_page(item: &Item) -> bool {
    item.kinds.iter().any(|k| k.starts_with("Wikimedia "))
}

/// Item ids Wikidata's search finds for `term`.
async fn search_ids(term: &str) -> Result<Vec<String>> {
    let v = wikidata(&[
        ("action", "wbsearchentities"),
        ("search", term),
        ("language", "en"),
        ("uselang", "en"),
        ("type", "item"),
        ("limit", SEARCH_LIMIT),
    ])
    .await?;
    Ok(v["search"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["id"].as_str().map(String::from))
        .collect())
}

/// The names to search for: the name, and for `x (y)` also `x` and `y`.
/// "metropolitan police department (mpd)" found nothing until the bracket
/// went (§11.1).
pub fn name_variants(name: &str) -> Vec<String> {
    let mut out = vec![name.trim().to_string()];
    if let (Some(open), Some(close)) = (name.find('('), name.rfind(')'))
        && open < close
    {
        let outer = format!("{} {}", &name[..open], &name[close + 1..]);
        let outer = outer.split_whitespace().collect::<Vec<_>>().join(" ");
        let inner = name[open + 1..close].trim().to_string();
        for v in [outer, inner] {
            if v.chars().count() >= 2 && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// Candidates for several search terms, deduplicated by item, in order.
pub(crate) async fn candidates_for(terms: &[String], skip: &HashSet<String>) -> Result<Vec<Item>> {
    let mut ids: Vec<String> = Vec::new();
    for t in terms {
        for id in search_ids(t).await? {
            if !skip.contains(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids.truncate(MAX_CANDIDATES);
    Ok(details(&ids)
        .await?
        .into_iter()
        .filter(|i| !is_wikimedia_page(i))
        .collect())
}

/// `base` (ending in `/`) plus the title as one encoded path segment.
pub fn article_url(base: &str, title: &str) -> String {
    match url::Url::parse(base) {
        Ok(mut u) => {
            if let Ok(mut seg) = u.path_segments_mut() {
                seg.pop_if_empty().push(&title.replace(' ', "_"));
            }
            u.to_string()
        }
        Err(_) => format!("{base}{title}"),
    }
}

/// Percent-decoding for a pasted URL's path.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first paragraph of an English Wikipedia article (`None` when it has none).
async fn fetch_summary(title: &str) -> Result<Option<String>> {
    let v = get(&article_url(SUMMARY_API, title), &[]).await?;
    Ok(v["extract"]
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty()))
}

// ─── The model's part (pure) ──────────────────────────────────────────────────

/// The pick prompt. v3 = the §11.1 v2 prompt (the filed type "may be wrong")
/// plus the rule against close-but-not-it picks (decision §11.9.2). v4 names
/// the cases the held-out set still got wrong: a nationality adjective
/// ("Italian" → Italy) and a video or show linked to its subject.
pub fn pick_prompt(name: &str, entity_type: &str, texts: &[String], cands: &[Item]) -> String {
    let list: String = cands
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let desc = if c.description.is_empty() {
                "(no description)"
            } else {
                &c.description
            };
            let kind = if c.kinds.is_empty() {
                "kind unknown".to_string()
            } else {
                c.kinds.join(", ")
            };
            format!("{}. {} — {} [{}]\n", i + 1, c.label, desc, kind)
        })
        .collect();
    let ctx: String = texts.iter().map(|t| format!("- {t}\n")).collect();
    format!(
        "Which Wikidata item is the entity \"{name}\" that these texts mention? (It was filed as a {entity_type}, which may be wrong.)\n\n\
         TEXTS:\n{ctx}\nCANDIDATES:\n{list}\n\
         Rules:\n\
         - Pick the candidate the texts mean, judging by what the texts say about it, not by the name alone.\n\
         - The candidate must be the entity itself, not something related to it. If the name is a nationality, language or people word (\"Italian\", \"Cuban\", \"Emirati\"), answer 0: it is not the country. A vague group (\"U.S. administrations\") is not one body. A video, episode or show is not its subject or host.\n\
         - If none of them is it, or the texts don't say enough to tell, answer 0. A wrong pick is worse than 0.\n\
         - Output ONLY JSON: {{\"pick\": <number or 0>, \"why\": \"<one short sentence>\"}}"
    )
}

/// The 0-based candidate the answer picks, or `None` for 0 or nonsense.
pub fn parse_pick(v: &Value, n: usize) -> Option<usize> {
    let p = v["pick"]
        .as_u64()
        .or_else(|| v["pick"].as_str().and_then(|s| s.trim().parse().ok()))?;
    (p >= 1 && (p as usize) <= n).then(|| p as usize - 1)
}

/// Asked only when the first search found nothing that fits.
pub fn queries_prompt(name: &str, texts: &[String]) -> String {
    let ctx: String = texts.iter().map(|t| format!("- {t}\n")).collect();
    format!(
        "The texts below mention \"{name}\". A Wikipedia search for that name found nothing that fits.\n\
         TEXTS:\n{ctx}\n\
         If the name is an abbreviation, a plural, a misspelling or a transcription error, or the thing has a fuller \
         official name, give up to 2 search terms Wikipedia would use for it. Use only what the texts support. \
         If it is a private person, a generic phrase, or something too obscure or new for Wikipedia, give none.\n\
         Output ONLY JSON: {{\"queries\": [\"...\"]}}"
    )
}

/// At most two new, non-empty search terms.
pub fn parse_queries(v: &Value, name: &str) -> Vec<String> {
    v["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| q.as_str())
        .map(|q| q.trim().to_string())
        .filter(|q| q.chars().count() >= 2 && !q.eq_ignore_ascii_case(name))
        .take(2)
        .collect()
}

/// A Wikidata id, from `Q123`, a Wikidata URL, or `None` for anything else.
pub fn qid_from(s: &str) -> Option<String> {
    let last = s.trim().trim_end_matches('/').rsplit('/').next()?;
    let ok = last.len() > 1
        && last.starts_with(['Q', 'q'])
        && last[1..].chars().all(|c| c.is_ascii_digit());
    ok.then(|| last.to_uppercase())
}

/// The article title from an English Wikipedia URL.
pub fn wiki_title_from(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .split_once("en.wikipedia.org/wiki/")
        .map(|(_, r)| r)?;
    let title = rest.split(['#', '?']).next()?.replace('_', " ");
    let title = percent_decode(&title);
    (!title.trim().is_empty()).then(|| title.trim().to_string())
}

// ─── Storage ──────────────────────────────────────────────────────────────────

async fn store_items(neo4j: &Db, items: &[Item]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let rows: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut m = serde_json::Map::new();
            m.insert("qid".into(), i.qid.clone().into());
            m.insert("label".into(), i.label.clone().into());
            m.insert("description".into(), i.description.clone().into());
            m.insert("kinds".into(), i.kinds.clone().into());
            m.insert("class_qids".into(), i.class_qids.clone().into());
            m.insert("aliases".into(), i.aliases.clone().into());
            m.insert(
                "wiki_title".into(),
                i.wiki_title.clone().map(Into::into).unwrap_or(Value::Null),
            );
            m.insert(
                "lat".into(),
                i.location.map(|p| p.lat.into()).unwrap_or(Value::Null),
            );
            m.insert(
                "lon".into(),
                i.location.map(|p| p.lon.into()).unwrap_or(Value::Null),
            );
            m.insert("coord_count".into(), i.coord_count.into());
            Value::Object(m)
        })
        .collect();
    neo4j
        .run(
            query(
                "UNWIND $rows AS r MERGE (w:WikiItem {qid: r.qid}) \
                 SET w.label = r.label, w.description = r.description, w.kinds = r.kinds, \
                     w.class_qids = r.class_qids, w.aliases = r.aliases, w.wiki_title = r.wiki_title, \
                     w.location = CASE WHEN r.lat IS NULL THEN null \
                                       ELSE point({latitude: r.lat, longitude: r.lon}) END, \
                     w.coord_count = r.coord_count, w.coords_at = datetime(), \
                     w.fetched_at = datetime()",
            )
            .param("rows", rows),
        )
        .await?;
    Ok(())
}

/// What one lookup did.
#[derive(Debug, Clone, Serialize)]
pub struct Looked {
    pub entity_id: String,
    pub name: String,
    /// `proposed` · `none` · `kept` (a confirmed link is not looked up again)
    /// · `skipped_private` (no claim mentions it and this was not on demand).
    pub result: String,
    pub picked: Option<Item>,
    pub why: Option<String>,
    /// The search term the model suggested, when the first search had nothing.
    pub query: Option<String>,
    pub candidates: usize,
}

/// Look one entity up and store what was found.
pub async fn link_one(
    neo4j: &Db,
    llm: &dyn LlmProvider,
    id: &str,
    on_demand: bool,
    model: &str,
) -> Result<Looked> {
    let rows = neo4j
        .execute(
            query(
                "MATCH (e:Entity {id: $id}) \
                 RETURN e.name AS name, coalesce(e.entity_type, 'unknown') AS t, \
                   EXISTS { (:Note {note_type: 'claim'})-[:MENTIONS]->(e) } AS public, \
                   EXISTS { (e)-[:REFERS_TO {status: 'confirmed'}]->() } AS confirmed, \
                   [(e)-[r:REFERS_TO {status: 'rejected'}]->(w) | w.qid] AS rejected, \
                   [(c:Note {note_type: 'claim'})-[:MENTIONS]->(e) | c.content][..$k] AS claims, \
                   [(n:Note)-[:MENTIONS]->(e) | n.content][..$k] AS notes",
            )
            .param("id", id)
            .param("k", CONTEXT_TEXTS),
        )
        .await?;
    let r = rows.first().context("no entity with that id")?;
    let name: String = r.get("name").unwrap_or_default();
    let etype: String = r.get("t").unwrap_or_default();
    let public: bool = r.get("public").unwrap_or(false);
    let base = Looked {
        entity_id: id.to_string(),
        name: name.clone(),
        result: String::new(),
        picked: None,
        why: None,
        query: None,
        candidates: 0,
    };
    if r.get::<bool>("confirmed").unwrap_or(false) {
        return Ok(Looked {
            result: "kept".into(),
            ..base
        });
    }
    if !public && !on_demand {
        return Ok(Looked {
            result: "skipped_private".into(),
            ..base
        });
    }
    // Background lookups read only claims; on demand, whatever mentions it.
    let texts: Vec<String> = r
        .get::<Vec<String>>(if public { "claims" } else { "notes" })
        .unwrap_or_default()
        .iter()
        .map(|t| t.chars().take(CONTEXT_CHARS).collect())
        .collect();
    let rejected: HashSet<String> = r
        .get::<Vec<String>>("rejected")
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut cands = candidates_for(&name_variants(&name), &rejected).await?;
    let mut pick = None;
    let mut why = None;
    if !cands.is_empty() {
        let a = llm
            .generate_json(
                &pick_prompt(&name, &etype, &texts, &cands),
                None,
                &["pick"],
                1,
            )
            .await?;
        pick = parse_pick(&a, cands.len());
        why = a["why"].as_str().map(String::from);
    }
    let mut query = None;
    // A second search only for named things. For vague phrases it found a
    // match that wasn't there ("system intelligence" → AGI, §11.1), and for
    // video and show titles it found their subject (a makemore video →
    // WaveNet; "Cuomo Full Show" → CNN's old Cuomo Prime Time).
    if pick.is_none() && !NO_SECOND_SEARCH.contains(&etype.as_str()) {
        let q = llm
            .generate_json(&queries_prompt(&name, &texts), None, &["queries"], 1)
            .await
            .map(|v| parse_queries(&v, &name))
            .unwrap_or_default();
        if !q.is_empty() {
            let mut skip = rejected.clone();
            skip.extend(cands.iter().map(|c| c.qid.clone()));
            let extra = candidates_for(&q, &skip).await?;
            if !extra.is_empty() {
                cands.extend(extra);
                cands.truncate(MAX_CANDIDATES + 4);
                let a = llm
                    .generate_json(
                        &pick_prompt(&name, &etype, &texts, &cands),
                        None,
                        &["pick"],
                        1,
                    )
                    .await?;
                pick = parse_pick(&a, cands.len());
                why = a["why"].as_str().map(String::from);
                if pick.is_some() {
                    query = Some(q.join(" | "));
                }
            }
        }
    }

    store_items(neo4j, &cands).await?;
    let picked = pick.map(|i| cands[i].clone());
    let result = if picked.is_some() { "proposed" } else { "none" };
    // A new lookup replaces an earlier proposal; confirmed and rejected links stay.
    neo4j
        .run(
            crate::db::query(
                "MATCH (e:Entity {id: $id}) \
                 OPTIONAL MATCH (e)-[old:REFERS_TO {status: 'proposed'}]->() DELETE old \
                 WITH DISTINCT e \
                 SET e.wiki_checked_at = datetime(), e.wiki_result = $result, \
                     e.wiki_candidates = $cands, e.wiki_version = $version \
                 REMOVE e.wiki_pending_at \
                 WITH e WHERE $qid IS NOT NULL \
                 MATCH (w:WikiItem {qid: $qid}) \
                 MERGE (e)-[l:REFERS_TO]->(w) \
                 SET l.status = 'proposed', l.method = 'llm', l.why = $why, l.query = $query, \
                     l.model = $model, l.at = datetime()",
            )
            .param("id", id)
            .param("result", result)
            .param(
                "cands",
                cands.iter().map(|c| c.qid.clone()).collect::<Vec<_>>(),
            )
            .param("version", PROMPT_VERSION)
            .param("qid", picked.as_ref().map(|p| p.qid.clone()))
            .param("why", why.clone().unwrap_or_default())
            .param("query", query.clone().unwrap_or_default())
            .param("model", model),
        )
        .await?;
    Ok(Looked {
        result: result.into(),
        candidates: cands.len(),
        picked,
        why,
        query,
        ..base
    })
}

/// The `link_entities` tool: one entity (`entity_id`), or up to `limit` of the
/// public entities waiting, review queues first, then by how many claims
/// mention them.
pub async fn link_entities(
    neo4j: &Db,
    llm: &dyn LlmProvider,
    limit: i64,
    entity_id: Option<&str>,
    on_demand: bool,
) -> Result<Value> {
    if contact().is_none() {
        bail!(
            "WIKIMEDIA_CONTACT is not set: Wikimedia asks automated clients for a contact, so no lookups run until it is"
        );
    }
    let model = llm.model_name();
    let ids: Vec<String> = match entity_id {
        Some(id) => vec![id.to_string()],
        None => {
            let review: Vec<String> = neo4j
                .merge_candidates(None, usize::MAX)
                .await
                .ok()
                .and_then(|v| v["candidates"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .flat_map(|c| [c["from"]["id"].as_str(), c["into"]["id"].as_str()])
                .flatten()
                .map(String::from)
                .collect();
            neo4j
                .execute(
                    query(&format!(
                        "{WAITING} \
                         WITH e, COUNT {{ (:Note {{note_type: 'claim'}})-[:MENTIONS]->(e) }} AS m, \
                              (e.id IN $review OR EXISTS {{ (e)-[:RELATES {{status: 'proposed'}}]-() }}) AS in_review \
                         RETURN e.id AS id ORDER BY in_review DESC, m DESC LIMIT $n"
                    ))
                    .param("review", review)
                    .param("demonyms", demonyms())
                    .param("days", RECHECK_NONE_DAYS)
                    .param("n", limit),
                )
                .await?
                .iter()
                .filter_map(|r| r.get::<String>("id").ok())
                .collect()
        }
    };
    let mut results = Vec::new();
    let mut tally: HashMap<String, i64> = HashMap::new();
    let mut failed = Vec::new();
    let mut throttled = None;
    for id in &ids {
        match link_one(neo4j, llm, id, on_demand, &model).await {
            Ok(l) => {
                *tally.entry(l.result.clone()).or_default() += 1;
                results.push(l);
            }
            Err(e) => {
                if let Some(t) = e.downcast_ref::<Throttled>() {
                    throttled = Some(t.retry_after);
                    break;
                }
                warn!(entity = %id, error = %e, "reference lookup failed");
                failed.push(json!({ "entity_id": id, "error": e.to_string() }));
                if entity_id.is_some() {
                    let _ = neo4j
                        .run(
                            query("MATCH (e:Entity {id: $id}) REMOVE e.wiki_pending_at")
                                .param("id", id.as_str()),
                        )
                        .await;
                }
            }
        }
    }
    // Kinds for what is linked now (Phase 19), unless Wikimedia is throttling us.
    let kinds = if throttled.is_none() {
        match crate::kinds::refresh(neo4j, None, crate::kinds::BACKGROUND_BUDGET).await {
            Ok(k) => json!(k),
            Err(e) => json!({ "error": e.to_string() }),
        }
    } else {
        json!({ "skipped": "throttled" })
    };
    // Coordinates for linked items stored before MAP_PLAN Phase 1.
    let located = if throttled.is_none() {
        match crate::geo::refresh(neo4j, None, crate::geo::BACKGROUND_BUDGET).await {
            Ok(g) => json!(g),
            Err(e) => json!({ "error": e.to_string() }),
        }
    } else {
        json!({ "skipped": "throttled" })
    };
    let waiting = waiting_count(neo4j).await.unwrap_or(-1);
    info!(
        read = results.len(),
        ?tally,
        waiting,
        "reference lookups done"
    );
    Ok(json!({
        "read": results.len(),
        "proposed": tally.get("proposed").copied().unwrap_or(0),
        "none": tally.get("none").copied().unwrap_or(0),
        "kept": tally.get("kept").copied().unwrap_or(0),
        "skipped_private": tally.get("skipped_private").copied().unwrap_or(0),
        "failed": failed,
        "throttled_retry_after": throttled,
        "waiting": waiting,
        "model": model,
        "kinds": kinds,
        "located": located,
        "results": results,
    }))
}

/// Public entities due a lookup: never checked, or "none" more than 90 days ago.
/// Demonyms are left out (Phase 20): "Chinese" is China, not an article of its
/// own, and linking them gave Phase 18's worst picks.
const WAITING: &str = "MATCH (e:Entity) \
     WHERE NOT e.entity_type IN ['url', 'date'] AND NOT e.name IN $demonyms \
       AND EXISTS { (:Note {note_type: 'claim'})-[:MENTIONS]->(e) } \
       AND NOT EXISTS { (e)-[:REFERS_TO {status: 'confirmed'}]->() } \
       AND (e.wiki_checked_at IS NULL \
            OR (e.wiki_result = 'none' AND e.wiki_checked_at < datetime() - duration({days: $days})))";

fn demonyms() -> Vec<String> {
    crate::demonym_words()
}

async fn waiting_count(neo4j: &Db) -> Result<i64> {
    Ok(neo4j
        .execute(
            query(&format!("{WAITING} RETURN count(e) AS c"))
                .param("demonyms", demonyms())
                .param("days", RECHECK_NONE_DAYS),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0))
}

// ─── The user's decisions ─────────────────────────────────────────────────────

/// Ok, or why the request was refused (shown to the user).
pub type Outcome<T> = Result<std::result::Result<T, String>>;

/// Make `qid` the entity's confirmed reference. Any other confirmed or
/// proposed link becomes rejected, so the entity has one reference.
async fn confirm(neo4j: &Db, entity: &str, qid: &str, method: &str) -> Result<()> {
    neo4j
        .run(
            query(
                "MATCH (e:Entity {id: $e}), (w:WikiItem {qid: $q}) \
                 OPTIONAL MATCH (e)-[o:REFERS_TO]->(other:WikiItem) \
                   WHERE other.qid <> $q AND o.status IN ['proposed', 'confirmed'] \
                 SET o.status = 'rejected', o.decided_at = datetime() \
                 WITH DISTINCT e, w \
                 MERGE (e)-[l:REFERS_TO]->(w) \
                 ON CREATE SET l.method = $method, l.at = datetime() \
                 SET l.status = 'confirmed', l.decided_at = datetime(), \
                     e.wiki_result = 'proposed', e.wiki_checked_at = coalesce(e.wiki_checked_at, datetime())",
            )
            .param("e", entity)
            .param("q", qid)
            .param("method", method),
        )
        .await?;
    Ok(())
}

/// Confirm or reject the item linked (or offered) to an entity.
pub async fn decide(neo4j: &Db, entity: &str, qid: &str, decision: &str) -> Outcome<()> {
    let known = neo4j
        .execute(
            query(
                "MATCH (e:Entity {id: $e}), (w:WikiItem {qid: $q}) \
                 RETURN EXISTS { (e)-[:REFERS_TO]->(w) } OR w.qid IN coalesce(e.wiki_candidates, []) AS ok",
            )
            .param("e", entity)
            .param("q", qid),
        )
        .await?
        .first()
        .and_then(|r| r.get::<bool>("ok").ok());
    match known {
        None => return Ok(Err("no such entity or item".into())),
        Some(false) => {
            return Ok(Err(format!(
                "{qid} was never a candidate for this entity; paste its URL instead"
            )));
        }
        Some(true) => {}
    }
    match decision {
        "confirm" => confirm(neo4j, entity, qid, "manual").await?,
        "reject" => {
            neo4j
                .run(
                    query(
                        "MATCH (e:Entity {id: $e}), (w:WikiItem {qid: $q}) \
                         MERGE (e)-[l:REFERS_TO]->(w) \
                         ON CREATE SET l.method = 'manual', l.at = datetime() \
                         SET l.status = 'rejected', l.decided_at = datetime() \
                         WITH e WHERE NOT EXISTS { (e)-[:REFERS_TO {status: 'proposed'}]->() } \
                           AND NOT EXISTS { (e)-[:REFERS_TO {status: 'confirmed'}]->() } \
                         SET e.wiki_result = 'none'",
                    )
                    .param("e", entity)
                    .param("q", qid),
                )
                .await?
        }
        other => {
            return Ok(Err(format!(
                "decision must be 'confirm' or 'reject', not '{other}'"
            )));
        }
    }
    crate::kinds::refresh_entity(neo4j, entity).await;
    Ok(Ok(()))
}

/// Link an entity to the item behind a Wikipedia or Wikidata URL (or a `Q` id).
pub async fn set_url(neo4j: &Db, entity: &str, url: &str) -> Outcome<Item> {
    let qid = if let Some(q) = qid_from(url) {
        q
    } else if let Some(title) = wiki_title_from(url) {
        let v = wikidata(&[
            ("action", "wbgetentities"),
            ("sites", "enwiki"),
            ("titles", &title),
            ("props", "info"),
        ])
        .await?;
        match v["entities"]
            .as_object()
            .and_then(|m| m.keys().find(|k| k.starts_with('Q')).cloned())
        {
            Some(q) => q,
            None => {
                return Ok(Err(format!(
                    "English Wikipedia has no article titled \"{title}\""
                )));
            }
        }
    } else {
        return Ok(Err(
            "paste an English Wikipedia URL (https://en.wikipedia.org/wiki/…), a Wikidata URL, or a Q id".into(),
        ));
    };
    let Some(item) = details(std::slice::from_ref(&qid))
        .await?
        .into_iter()
        .next()
    else {
        return Ok(Err(format!("Wikidata has no item {qid}")));
    };
    if is_wikimedia_page(&item) {
        return Ok(Err(format!(
            "{} is a Wikimedia page ({}), a list of things rather than one; link the article for the thing itself",
            item.label,
            item.kinds.join(", ")
        )));
    }
    let exists = neo4j
        .execute(query("MATCH (e:Entity {id: $e}) RETURN e.id AS id").param("e", entity))
        .await?;
    if exists.is_empty() {
        return Ok(Err("no such entity".into()));
    }
    store_items(neo4j, std::slice::from_ref(&item)).await?;
    confirm(neo4j, entity, &qid, "manual").await?;
    crate::kinds::refresh_entity(neo4j, entity).await;
    Ok(Ok(item))
}

/// Mark an on-demand lookup as waiting, so the drawer can say so.
pub async fn mark_pending(neo4j: &Db, entity: &str) -> Result<bool> {
    Ok(!neo4j
        .execute(
            query("MATCH (e:Entity {id: $e}) SET e.wiki_pending_at = datetime() RETURN e.id AS id")
                .param("e", entity),
        )
        .await?
        .is_empty())
}

// ─── Reading ──────────────────────────────────────────────────────────────────

fn item_json(r: &Row, prefix: &str) -> Option<Value> {
    let g = |k: &str| r.get::<String>(&format!("{prefix}{k}")).ok();
    let qid = g("qid")?;
    let title = g("wiki_title");
    Some(json!({
        "qid": qid,
        "label": g("label").unwrap_or_default(),
        "description": g("description").unwrap_or_default(),
        "kinds": r.get::<Vec<String>>(&format!("{prefix}kinds")).unwrap_or_default(),
        "aliases": r.get::<Vec<String>>(&format!("{prefix}aliases")).unwrap_or_default(),
        "wiki_title": title,
        "wiki_url": title.as_ref().map(|t| article_url("https://en.wikipedia.org/wiki/", t)),
        "wikidata_url": format!("https://www.wikidata.org/wiki/{qid}"),
    }))
}

/// A stored item's point and how many coordinates it had: `{lat, lon,
/// coord_count}`, `{lat: null, ..}` when read and none, or null when unread.
async fn item_location(neo4j: &Db, qid: &str) -> Result<Value> {
    let rows = neo4j
        .execute(
            query(
                "MATCH (w:WikiItem {qid: $q}) \
                 RETURN w.coords_at IS NOT NULL AS read, w.location.latitude AS lat, \
                        w.location.longitude AS lon, coalesce(w.coord_count, 0) AS n",
            )
            .param("q", qid),
        )
        .await?;
    let Some(r) = rows.first() else {
        return Ok(Value::Null);
    };
    if !r.get::<bool>("read").unwrap_or(false) {
        return Ok(Value::Null);
    }
    Ok(json!({
        "lat": r.get::<f64>("lat").ok(),
        "lon": r.get::<f64>("lon").ok(),
        "coord_count": r.get::<i64>("n").unwrap_or(0),
    }))
}

/// GET /api/entities/{id}/wiki — the entity's reference link, the candidates
/// the lookup saw, what the user rejected, and the article's first paragraph.
pub async fn entity_reference(neo4j: &Db, id: &str) -> Result<Option<Value>> {
    let rows = neo4j
        .execute(
            query(
                "MATCH (e:Entity {id: $id}) \
                 OPTIONAL MATCH (e)-[l:REFERS_TO]->(w:WikiItem) WHERE l.status IN ['confirmed', 'proposed'] \
                 WITH e, l, w ORDER BY CASE l.status WHEN 'confirmed' THEN 0 ELSE 1 END LIMIT 1 \
                 RETURN e.wiki_result AS result, toString(e.wiki_checked_at) AS checked_at, \
                   e.wiki_pending_at IS NOT NULL AND e.wiki_pending_at > datetime() - duration({minutes: $pm}) AS pending, \
                   EXISTS { (:Note {note_type: 'claim'})-[:MENTIONS]->(e) } AS public, \
                   coalesce(e.wiki_candidates, []) AS cand_ids, \
                   l.status AS status, l.method AS method, l.why AS why, l.query AS query, l.model AS model, \
                   toString(l.at) AS at, \
                   w.qid AS w_qid, w.label AS w_label, w.description AS w_description, w.kinds AS w_kinds, \
                   w.aliases AS w_aliases, w.wiki_title AS w_wiki_title, w.summary AS summary, \
                   w.coords_at IS NOT NULL AS coords_read, \
                   w.summary_at IS NOT NULL AND w.summary_at > datetime() - duration({days: $sd}) AS summary_fresh",
            )
            .param("id", id)
            .param("pm", PENDING_MINUTES)
            .param("sd", SUMMARY_TTL_DAYS),
        )
        .await?;
    let Some(r) = rows.first() else {
        return Ok(None);
    };
    let link = item_json(r, "w_").map(|mut v| {
        v["status"] = json!(r.get::<String>("status").ok());
        v["method"] = json!(r.get::<String>("method").ok());
        v["why"] = json!(r.get::<String>("why").ok().filter(|s| !s.is_empty()));
        v["query"] = json!(r.get::<String>("query").ok().filter(|s| !s.is_empty()));
        v["model"] = json!(r.get::<String>("model").ok());
        v["at"] = json!(r.get::<String>("at").ok());
        v
    });
    // Where it is (MAP_PLAN Phase 1): read now if this item never was.
    let location = match link.as_ref().and_then(|l| l["qid"].as_str()) {
        Some(qid) => {
            if !r.get::<bool>("coords_read").unwrap_or(true)
                && let Err(e) = crate::geo::refresh(neo4j, Some(qid), 1).await
            {
                warn!(qid, error = %e, "geo: on-demand read failed");
            }
            item_location(neo4j, qid).await?
        }
        None => Value::Null,
    };
    let link = link.map(|mut l| {
        l["location"] = location;
        l
    });
    let mut summary = r.get::<String>("summary").ok();
    let mut summary_error = None;
    if let (Some(l), false) = (&link, r.get::<bool>("summary_fresh").unwrap_or(false))
        && let (Some(title), Some(qid)) = (l["wiki_title"].as_str(), l["qid"].as_str())
    {
        match fetch_summary(title).await {
            Ok(s) => {
                summary = s.clone();
                let _ = neo4j
                    .run(
                        query("MATCH (w:WikiItem {qid: $q}) SET w.summary = $s, w.summary_at = datetime()")
                            .param("q", qid)
                            .param("s", s.unwrap_or_default()),
                    )
                    .await;
            }
            Err(e) => summary_error = Some(e.to_string()),
        }
    }
    let cand_ids: Vec<String> = r.get("cand_ids").unwrap_or_default();
    let linked = link
        .as_ref()
        .and_then(|l| l["qid"].as_str())
        .map(String::from);
    let cands = neo4j
        .execute(
            query(
                "UNWIND range(0, size($ids) - 1) AS i \
                 MATCH (w:WikiItem {qid: $ids[i]}) \
                 RETURN w.qid AS qid, w.label AS label, w.description AS description, w.kinds AS kinds, \
                        w.aliases AS aliases, w.wiki_title AS wiki_title ORDER BY i",
            )
            .param("ids", cand_ids),
        )
        .await?;
    let rejected_rows = neo4j
        .execute(
            query(
                "MATCH (:Entity {id: $id})-[x:REFERS_TO {status: 'rejected'}]->(w:WikiItem) \
                 RETURN w.qid AS qid, w.label AS label ORDER BY x.decided_at DESC",
            )
            .param("id", id),
        )
        .await?;
    let rejected: Vec<Value> = rejected_rows
        .iter()
        .map(|x| json!({ "qid": x.get::<String>("qid").ok(), "label": x.get::<String>("label").ok() }))
        .collect();
    let rejected_ids: HashSet<String> = rejected
        .iter()
        .filter_map(|x| x["qid"].as_str().map(String::from))
        .collect();
    Ok(Some(json!({
        "link": link,
        "summary": summary.filter(|s| !s.is_empty()),
        "summary_error": summary_error,
        "result": r.get::<String>("result").ok(),
        "checked_at": r.get::<String>("checked_at").ok(),
        "pending": r.get::<bool>("pending").unwrap_or(false),
        "public": r.get::<bool>("public").unwrap_or(false),
        "candidates": cands
            .iter()
            .filter_map(|c| item_json(c, ""))
            .filter(|c| Some(c["qid"].as_str().unwrap_or_default().to_string()) != linked
                && !rejected_ids.contains(c["qid"].as_str().unwrap_or_default()))
            .collect::<Vec<_>>(),
        "rejected": rejected,
        "contact_set": contact().is_some(),
    })))
}

/// POST /api/wiki/describe — the one-line description beside each entity name
/// in review lists. Only entities with a proposed or confirmed link appear.
pub async fn describe(neo4j: &Db, ids: &[String]) -> Result<Value> {
    let rows = neo4j
        .execute(
            query(
                "UNWIND $ids AS id MATCH (e:Entity {id: id})-[l:REFERS_TO]->(w:WikiItem) \
                 WHERE l.status IN ['confirmed', 'proposed'] \
                 RETURN e.id AS id, w.label AS label, w.description AS description, l.status AS status",
            )
            .param("ids", ids.to_vec()),
        )
        .await?;
    let mut out = serde_json::Map::new();
    for r in &rows {
        if let Ok(id) = r.get::<String>("id") {
            out.insert(
                id,
                json!({
                    "label": r.get::<String>("label").ok(),
                    "description": r.get::<String>("description").ok(),
                    "status": r.get::<String>("status").ok(),
                }),
            );
        }
    }
    Ok(Value::Object(out))
}

/// GET /api/wiki/coverage?bucket=&limit= — how many public entities are
/// linked, have no article, or are waiting, and (with `bucket`) which ones.
pub async fn coverage(
    neo4j: &Db,
    bucket: Option<&str>,
    limit: i64,
) -> Result<std::result::Result<Value, String>> {
    const BUCKET: &str = "MATCH (e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] \
         AND NOT e.name IN $demonyms \
         AND EXISTS { (:Note {note_type: 'claim'})-[:MENTIONS]->(e) } \
         WITH e, CASE \
           WHEN EXISTS { (e)-[:REFERS_TO {status: 'confirmed'}]->() } THEN 'confirmed' \
           WHEN EXISTS { (e)-[:REFERS_TO {status: 'proposed'}]->() } THEN 'proposed' \
           WHEN e.wiki_checked_at IS NULL \
             OR (e.wiki_result = 'none' AND e.wiki_checked_at < datetime() - duration({days: $days})) THEN 'waiting' \
           ELSE 'none' END AS bucket";
    let counts = neo4j
        .execute(
            query(&format!("{BUCKET} RETURN bucket, count(*) AS c"))
                .param("demonyms", demonyms())
                .param("days", RECHECK_NONE_DAYS),
        )
        .await?;
    let mut by: serde_json::Map<String, Value> = ["confirmed", "proposed", "none", "waiting"]
        .iter()
        .map(|b| (b.to_string(), json!(0)))
        .collect();
    let mut total = 0i64;
    for r in &counts {
        let c = r.get::<i64>("c").unwrap_or(0);
        total += c;
        if let Ok(b) = r.get::<String>("bucket") {
            by.insert(b, json!(c));
        }
    }
    let on_demand = neo4j
        .execute(query(
            "MATCH (e:Entity)-[l:REFERS_TO]->() WHERE l.status IN ['confirmed', 'proposed'] \
               AND NOT EXISTS { (:Note {note_type: 'claim'})-[:MENTIONS]->(e) } RETURN count(DISTINCT e) AS c",
        ))
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0);
    let mut out = json!({
        "public_entities": total,
        "buckets": by,
        "linked_on_demand": on_demand,
        "contact_set": contact().is_some(),
    });
    if let Some(b) = bucket {
        if !["confirmed", "proposed", "none", "waiting"].contains(&b) {
            return Ok(Err(format!(
                "bucket must be confirmed, proposed, none or waiting, not '{b}'"
            )));
        }
        let rows = neo4j
            .execute(
                query(&format!(
                    "{BUCKET} WHERE bucket = $b \
                     WITH e, COUNT {{ (:Note {{note_type: 'claim'}})-[:MENTIONS]->(e) }} AS m \
                     OPTIONAL MATCH (e)-[l:REFERS_TO]->(w:WikiItem) WHERE l.status IN ['confirmed', 'proposed'] \
                     RETURN e.id AS id, e.name AS name, e.entity_type AS t, m, w.label AS label, \
                            w.description AS description, toString(e.wiki_checked_at) AS checked_at \
                     ORDER BY m DESC LIMIT $n"
                ))
                .param("days", RECHECK_NONE_DAYS)
                .param("demonyms", demonyms())
                .param("b", b)
                .param("n", limit),
            )
            .await?;
        out["bucket"] = json!(b);
        out["entities"] = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.get::<String>("id").ok(),
                    "name": r.get::<String>("name").ok(),
                    "entity_type": r.get::<String>("t").ok(),
                    "claims": r.get::<i64>("m").unwrap_or(0),
                    "label": r.get::<String>("label").ok(),
                    "description": r.get::<String>("description").ok(),
                    "checked_at": r.get::<String>("checked_at").ok(),
                })
            })
            .collect();
    }
    Ok(Ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, desc: &str) -> Item {
        Item {
            qid: "Q1".into(),
            label: label.into(),
            description: desc.into(),
            kinds: vec!["human".into()],
            class_qids: vec!["Q5".into()],
            aliases: vec![],
            wiki_title: None,
            location: None,
            coord_count: 0,
        }
    }

    fn coord(lat: f64, lon: f64, rank: &str, globe: &str) -> Value {
        json!({"rank": rank, "mainsnak": {"datavalue": {"value": {
            "latitude": lat, "longitude": lon, "globe": format!("http://www.wikidata.org/entity/{globe}")}}}})
    }

    #[test]
    fn a_coordinate_is_the_preferred_earthly_one() {
        // Detroit-ish, a normal and a preferred claim: the preferred wins.
        let e = json!({"claims": {"P625": [coord(1.0, 2.0, "normal", "Q2"), coord(42.33, -83.05, "preferred", "Q2")]}});
        assert_eq!(
            coord_of(&e),
            (
                Some(LatLon {
                    lat: 42.33,
                    lon: -83.05
                }),
                2
            )
        );
        // A crater on Mars (Q111) is not a place on Earth's map.
        let e = json!({"claims": {"P625": [coord(4.5, 137.4, "normal", "Q111")]}});
        assert_eq!(coord_of(&e), (None, 1));
        // Deprecated claims are neither picked nor counted.
        let e = json!({"claims": {"P625": [coord(9.0, 9.0, "deprecated", "Q2"), coord(48.85, 2.35, "normal", "Q2")]}});
        assert_eq!(
            coord_of(&e),
            (
                Some(LatLon {
                    lat: 48.85,
                    lon: 2.35
                }),
                1
            )
        );
        // No claim, or an impossible value: no point.
        assert_eq!(coord_of(&json!({"claims": {}})), (None, 0));
        let e = json!({"claims": {"P625": [coord(123.0, 2.0, "normal", "Q2")]}});
        assert_eq!(coord_of(&e).0, None);
    }

    #[test]
    fn a_bracket_is_searched_without_it_and_on_its_own() {
        assert_eq!(
            name_variants("metropolitan police department (mpd)"),
            vec![
                "metropolitan police department (mpd)",
                "metropolitan police department",
                "mpd"
            ]
        );
        assert_eq!(name_variants("peter thiel"), vec!["peter thiel"]);
    }

    #[test]
    fn the_pick_prompt_numbers_candidates_and_shows_kinds() {
        let p = pick_prompt(
            "shergar",
            "person",
            &["A ransom demand followed the abduction of Shergar.".into()],
            &[
                item("Shergar", "Irish bay stallion"),
                item("Shergar Cup", ""),
            ],
        );
        assert!(p.contains("1. Shergar — Irish bay stallion [human]"));
        assert!(p.contains("2. Shergar Cup — (no description)"));
        assert!(p.contains("which may be wrong"));
        assert!(p.contains("it is not the country"));
        assert!(p.contains("not its subject or host"));
    }

    #[test]
    fn a_pick_of_zero_or_out_of_range_is_none() {
        assert_eq!(parse_pick(&json!({"pick": 2}), 3), Some(1));
        assert_eq!(parse_pick(&json!({"pick": "1"}), 3), Some(0));
        assert_eq!(parse_pick(&json!({"pick": 0}), 3), None);
        assert_eq!(parse_pick(&json!({"pick": 4}), 3), None);
        assert_eq!(parse_pick(&json!({"why": "x"}), 3), None);
    }

    #[test]
    fn suggested_queries_drop_the_name_itself_and_keep_two() {
        let v = json!({"queries": ["Kapo", "Estonian Internal Security Service", "Kaitsepolitsei", "x"]});
        assert_eq!(
            parse_queries(&v, "kapo"),
            vec!["Estonian Internal Security Service", "Kaitsepolitsei"]
        );
        assert!(parse_queries(&json!({"queries": []}), "kapo").is_empty());
    }

    #[test]
    fn urls_and_ids_are_recognised() {
        assert_eq!(
            qid_from("https://www.wikidata.org/wiki/Q184560"),
            Some("Q184560".into())
        );
        assert_eq!(qid_from("q42"), Some("Q42".into()));
        assert_eq!(qid_from("https://en.wikipedia.org/wiki/MI6"), None);
        assert_eq!(
            wiki_title_from("https://en.wikipedia.org/wiki/VK_(service)#History"),
            Some("VK (service)".into())
        );
        assert_eq!(
            wiki_title_from("https://en.wikipedia.org/wiki/Caf%C3%A9"),
            Some("Café".into())
        );
        assert_eq!(wiki_title_from("https://example.com/wiki/X"), None);
        assert_eq!(
            article_url("https://en.wikipedia.org/wiki/", "VK (service)"),
            "https://en.wikipedia.org/wiki/VK_(service)"
        );
        assert_eq!(
            article_url(SUMMARY_API, "Café"),
            format!("{SUMMARY_API}Caf%C3%A9")
        );
    }
}
