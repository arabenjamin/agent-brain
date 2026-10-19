//! API tags (API_USE_PLAN Phase 1): what an `(:ApiContext)` is about and what
//! kind of data it gives, from two closed lists in the graph — `(:Topic)` and
//! `(:ApiKind)` (`apis/_kinds.yaml`). Stored as `api_kind`, `tags_by`,
//! `tagged_at` and `(:ApiContext)-[:SERVES]->(:Topic)`.
//!
//! Who tags: the caller of `define_api_context` / `ingest_api` when it passes
//! `kind` or `topics` (`tags_by: caller`); otherwise, for an API not yet
//! tagged, nimble on host Ollama (`tags_by: nimble`), measured in Phase 0:
//! kind 12 of 12; topics right once kind `search` serves none. A tag below
//! 0.7 is left off, never guessed.

use serde_json::{Value, json};
use tracing::warn;

use agent_brain_repository::Neo4jClient;

/// The confidence a nimble answer needs to become a tag (Phase 0, as the alias judge).
const SURE: f64 = 0.7;
/// A search engine answers any query: by definition it serves no topic.
const SEARCH: &str = "search";

/// The two closed lists, read from the graph.
#[derive(Debug, Default, Clone)]
pub struct Vocab {
    /// `(name, description)` in name order.
    pub kinds: Vec<(String, String)>,
    /// `(name, label, description)` in name order.
    pub topics: Vec<(String, String, String)>,
}

impl Vocab {
    pub async fn load(neo4j: &Neo4jClient) -> Result<Self, String> {
        let kinds = neo4j
            .execute(neo4rs::query(
                "MATCH (k:ApiKind) RETURN k.name AS name, coalesce(k.description, '') AS d ORDER BY name",
            ))
            .await
            .map_err(|e| format!("reading API kinds failed: {e}"))?
            .iter()
            .filter_map(|r| Some((r.get::<String>("name").ok()?, r.get::<String>("d").unwrap_or_default())))
            .collect();
        let topics = neo4j
            .execute(neo4rs::query(
                "MATCH (t:Topic) RETURN t.name AS name, coalesce(t.label, t.name) AS label, \
                 coalesce(t.description, '') AS d ORDER BY name",
            ))
            .await
            .map_err(|e| format!("reading topics failed: {e}"))?
            .iter()
            .filter_map(|r| {
                Some((
                    r.get::<String>("name").ok()?,
                    r.get::<String>("label").unwrap_or_default(),
                    r.get::<String>("d").unwrap_or_default(),
                ))
            })
            .collect();
        Ok(Self { kinds, topics })
    }

    pub fn kind_names(&self) -> Vec<&str> {
        self.kinds.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn topic_names(&self) -> Vec<&str> {
        self.topics.iter().map(|(n, _, _)| n.as_str()).collect()
    }
}

/// Tags a caller passed. `None` = not passed (keep what is stored); a kind of
/// `""` or `topics: []` clears.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Requested {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub topics: Option<Vec<String>>,
}

impl Requested {
    pub fn given(&self) -> bool {
        self.kind.is_some() || self.topics.is_some()
    }
}

/// Check requested tags against the lists. The error names what is allowed.
pub fn check(req: &Requested, vocab: &Vocab) -> Result<(), String> {
    if let Some(k) = req.kind.as_deref().filter(|k| !k.is_empty())
        && !vocab.kind_names().contains(&k)
    {
        return Err(format!(
            "kind '{k}' is not an API kind. Kinds: {}. Nothing was saved.",
            vocab.kind_names().join(", ")
        ));
    }
    if let Some(topics) = &req.topics {
        let known = vocab.topic_names();
        if let Some(t) = topics.iter().find(|t| !known.contains(&t.as_str())) {
            return Err(format!(
                "topic '{t}' is not a topic. Topics: {}. An API may serve none (`topics: []`). Nothing was saved.",
                known.join(", ")
            ));
        }
        if req.kind.as_deref() == Some(SEARCH) && !topics.is_empty() {
            return Err(
                "an API of kind `search` serves no topic (a search engine answers any query); \
                 pass `topics: []`. Nothing was saved."
                    .into(),
            );
        }
    }
    Ok(())
}

/// The API's stored tags: `(kind, topics, tags_by)`.
pub async fn stored(
    neo4j: &Neo4jClient,
    name: &str,
) -> Option<(Option<String>, Vec<String>, Option<String>)> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:ApiContext {name: $name}) OPTIONAL MATCH (c)-[:SERVES]->(t:Topic) \
                 RETURN c.api_kind AS kind, c.tags_by AS by, collect(t.name) AS topics",
            )
            .param("name", name),
        )
        .await
        .ok()?;
    let r = rows.first()?;
    Some((
        r.get::<String>("kind").ok(),
        r.get::<Vec<String>>("topics").unwrap_or_default(),
        r.get::<String>("by").ok(),
    ))
}

/// Write tags. `topics: Some` replaces the SERVES set; `kind: Some("")` clears it.
pub async fn store(
    neo4j: &Neo4jClient,
    name: &str,
    kind: Option<&str>,
    topics: Option<&[String]>,
    by: &str,
    detail: Option<&Value>,
) -> Result<(), String> {
    let err = |e: agent_brain_repository::RepositoryError| format!("storing tags failed: {e}");
    let q = neo4rs::query(
        "MATCH (c:ApiContext {name: $name}) \
         SET c.tags_by = $by, c.tagged_at = datetime(), \
             c.api_kind = CASE WHEN $set_kind THEN (CASE WHEN $kind = '' THEN null ELSE $kind END) ELSE c.api_kind END, \
             c.tag_scores = CASE WHEN $detail = '' THEN c.tag_scores ELSE $detail END",
    )
    .param("name", name)
    .param("by", by)
    .param("set_kind", kind.is_some())
    .param("kind", kind.unwrap_or_default())
    .param("detail", detail.map(Value::to_string).unwrap_or_default());
    neo4j.run(q).await.map_err(err)?;
    let Some(topics) = topics else {
        return Ok(());
    };
    neo4j
        .run(
            neo4rs::query("MATCH (:ApiContext {name: $name})-[s:SERVES]->(:Topic) DELETE s")
                .param("name", name),
        )
        .await
        .map_err(err)?;
    neo4j
        .run(
            neo4rs::query(
                "MATCH (c:ApiContext {name: $name}) UNWIND $topics AS t \
                 MATCH (topic:Topic {name: t}) MERGE (c)-[:SERVES]->(topic)",
            )
            .param("name", name)
            .param("topics", topics.to_vec()),
        )
        .await
        .map_err(err)
}

/// The tagger model: `API_TAGGER_MODEL` (default `nimble`; `off` = none).
fn tagger_model() -> Option<String> {
    let m = std::env::var("API_TAGGER_MODEL").unwrap_or_else(|_| "nimble".into());
    let m = m.trim();
    (!m.is_empty() && !m.eq_ignore_ascii_case("off")).then(|| m.to_string())
}

/// The text nimble reads: name, description, up to 15 endpoint summaries
/// (the Phase 0 measurement's shape).
pub fn api_text(name: &str, description: &str, endpoints: &[String]) -> String {
    let mut lines = vec![format!("API: {name}"), description.trim().to_string()];
    if !endpoints.is_empty() {
        lines.push("Endpoints:".into());
        lines.extend(endpoints.iter().take(15).map(|e| format!("- {e}")));
    }
    lines.join("\n")
}

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A `/v1/systemone` body, written by hand: criteria keep their order (serde_json
/// maps sort keys — the alias judge's lesson, HBI §11.10).
pub fn kind_body(model: &str, text: &str, vocab: &Vocab) -> String {
    let criteria = vocab
        .kinds
        .iter()
        .map(|(n, d)| format!("{}:{}", q(n), q(d)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"model\":{},\"keep_alive\":\"60s\",\"state\":{{\"api\":{}}},\"questions\":{{\"q\":{{\"type\":\"choice\",\"instructions\":{},\"criteria\":{{{criteria}}}}}}}}}",
        q(model),
        q(text),
        q("What kind of data does this API provide? Pick the one kind that fits it best.")
    )
}

pub fn topic_body(model: &str, text: &str, label: &str, description: &str) -> String {
    let instructions = format!(
        "Is this API's data mainly about this topic, so that a workflow watching the topic would read it?\nTopic: {label}. {description}"
    );
    format!(
        "{{\"model\":{},\"keep_alive\":\"60s\",\"state\":{{\"api\":{}}},\"questions\":{{\"q\":{{\"type\":\"noul\",\"instructions\":{}}}}}}}",
        q(model),
        q(text),
        q(&instructions)
    )
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default()
    })
}

async fn ask(body: String) -> Result<Value, String> {
    let base =
        std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let v: Value = http()
        .post(format!("{}/v1/systemone", base.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(v["answers"]["q"].clone())
}

/// What nimble proposes: a kind and topics at >= 0.7, with every score.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub kind: Option<String>,
    pub topics: Vec<String>,
    pub scores: Value,
}

/// From nimble's answers, the tags that clear the bar. Kind first: a `search`
/// API, or one whose kind nimble cannot tell, gets no topics.
pub fn decide(kind: &str, p_kind: f64, topic_scores: &[(String, f64)]) -> Proposal {
    let sure_kind = (p_kind >= SURE).then(|| kind.to_string());
    let topics = match sure_kind.as_deref() {
        Some(SEARCH) | None => vec![],
        Some(_) => topic_scores
            .iter()
            .filter(|(_, p)| *p >= SURE)
            .map(|(t, _)| t.clone())
            .collect(),
    };
    let scores = json!({
        "kind": {kind: p_kind},
        "topics": topic_scores.iter().map(|(t, p)| (t.clone(), json!(p))).collect::<serde_json::Map<_, _>>(),
    });
    Proposal {
        kind: sure_kind,
        topics,
        scores,
    }
}

/// Ask nimble. `Err` says why there is no proposal (model off, unreachable).
pub async fn propose(text: &str, vocab: &Vocab) -> Result<Proposal, String> {
    let Some(model) = tagger_model() else {
        return Err("API_TAGGER_MODEL is off".into());
    };
    if vocab.kinds.is_empty() {
        return Err("no API kinds in the graph (apis/_kinds.yaml not seeded)".into());
    }
    let a = ask(kind_body(&model, text, vocab)).await?;
    let kind = a["choice"].as_str().unwrap_or_default().to_string();
    if !vocab.kind_names().contains(&kind.as_str()) {
        return Err(format!("{model} chose '{kind}', not a kind"));
    }
    let p_kind = a["probabilities"][&kind].as_f64().unwrap_or(0.0);
    let mut topic_scores = Vec::new();
    if p_kind >= SURE && kind != SEARCH {
        for (name, label, d) in &vocab.topics {
            match ask(topic_body(&model, text, label, d)).await {
                Ok(t) => topic_scores.push((name.clone(), t["noul"].as_f64().unwrap_or(0.0))),
                Err(e) => warn!(topic = %name, error = %e, "API tagger: topic question failed"),
            }
        }
    }
    Ok(decide(&kind, p_kind, &topic_scores))
}

/// Tag an API after it is saved, and say what was done (the result's `tags`).
/// Passed tags win; else an untagged API is offered to nimble; else the
/// stored tags stay. Never fails the save it follows.
pub async fn apply(
    neo4j: &Neo4jClient,
    name: &str,
    req: &Requested,
    vocab: &Vocab,
    text: &str,
) -> Value {
    if req.given() {
        let topics = req.topics.as_deref();
        return match store(neo4j, name, req.kind.as_deref(), topics, "caller", None).await {
            Ok(()) => describe(neo4j, name, "set from this call").await,
            Err(e) => json!({ "error": e }),
        };
    }
    if let Some((Some(_), _, _)) = stored(neo4j, name).await {
        return describe(
            neo4j,
            name,
            "kept: already tagged; pass kind/topics to change",
        )
        .await;
    }
    match propose(text, vocab).await {
        Ok(p) if p.kind.is_none() => json!({
            "kind": null, "topics": [], "tags_by": null,
            "note": "untagged: nimble could not tell its kind (below 0.7); pass `kind` (and `topics`) to tag it",
            "scores": p.scores,
        }),
        Ok(p) => match store(
            neo4j,
            name,
            p.kind.as_deref(),
            Some(&p.topics),
            "nimble",
            Some(&p.scores),
        )
        .await
        {
            Ok(()) => {
                let mut d = describe(
                    neo4j,
                    name,
                    "proposed by nimble (>= 0.7); pass kind/topics to correct",
                )
                .await;
                d["scores"] = p.scores;
                d
            }
            Err(e) => json!({ "error": e }),
        },
        Err(why) => json!({
            "kind": null, "topics": [], "tags_by": null,
            "note": format!("untagged: {why}; pass `kind` (and `topics`) to tag it"),
        }),
    }
}

async fn describe(neo4j: &Neo4jClient, name: &str, how: &str) -> Value {
    match stored(neo4j, name).await {
        Some((kind, topics, by)) => {
            json!({ "kind": kind, "topics": topics, "tags_by": by, "note": how })
        }
        None => json!({ "error": format!("ApiContext '{name}' not found after the save") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocab {
        Vocab {
            kinds: vec![
                ("news-feed".into(), "Dated stories".into()),
                ("search".into(), "A search engine".into()),
                ("forecast".into(), "Weather".into()),
            ],
            topics: vec![
                ("ai-ml".into(), "AI".into(), "models".into()),
                ("cybersecurity".into(), "Security".into(), "vulns".into()),
            ],
        }
    }

    #[test]
    fn check_refuses_names_off_the_lists_and_topics_on_a_search_engine() {
        let v = vocab();
        let ok = Requested {
            kind: Some("forecast".into()),
            topics: Some(vec![]),
        };
        assert_eq!(check(&ok, &v), Ok(()));
        let bad_kind = Requested {
            kind: Some("weather".into()),
            topics: None,
        };
        assert!(
            check(&bad_kind, &v)
                .unwrap_err()
                .contains("news-feed, search, forecast")
        );
        let bad_topic = Requested {
            kind: None,
            topics: Some(vec!["weather".into()]),
        };
        assert!(
            check(&bad_topic, &v)
                .unwrap_err()
                .contains("ai-ml, cybersecurity")
        );
        let search = Requested {
            kind: Some("search".into()),
            topics: Some(vec!["ai-ml".into()]),
        };
        assert!(check(&search, &v).is_err());
        let clear = Requested {
            kind: Some(String::new()),
            topics: None,
        };
        assert_eq!(check(&clear, &v), Ok(()));
    }

    #[test]
    fn decide_keeps_only_sure_tags_and_asks_kind_first() {
        let scores = vec![
            ("ai-ml".to_string(), 0.99),
            ("cybersecurity".to_string(), 0.5),
        ];
        let p = decide("news-feed", 0.95, &scores);
        assert_eq!(p.kind.as_deref(), Some("news-feed"));
        assert_eq!(p.topics, vec!["ai-ml".to_string()]);
        // Phase 0: one-line search-engine descriptions drew false topic "yes".
        assert!(decide("search", 1.0, &scores).topics.is_empty());
        let unsure = decide("forecast", 0.6, &scores);
        assert_eq!(unsure.kind, None);
        assert!(unsure.topics.is_empty());
    }

    #[test]
    fn the_kind_body_keeps_the_kinds_in_list_order() {
        let body = kind_body("nimble", "API: x", &vocab());
        let v: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["questions"]["q"]["type"], "choice");
        let order: Vec<usize> = ["news-feed", "search", "forecast"]
            .iter()
            .map(|k| body.find(&format!("\"{k}\":")).unwrap())
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "criteria reordered: {body}"
        );
        let t: Value =
            serde_json::from_str(&topic_body("nimble", "API: x", "AI", "models")).unwrap();
        assert_eq!(t["questions"]["q"]["type"], "noul");
    }

    #[test]
    fn api_text_caps_the_endpoints() {
        let eps: Vec<String> = (0..20).map(|i| format!("GET /e{i}")).collect();
        let t = api_text("x", "An API", &eps);
        assert!(t.contains("GET /e14") && !t.contains("GET /e15"));
    }
}
