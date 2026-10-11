//! Developer notes in the graph (CORE_AND_PLUGINS_PLAN.md §5).
//!
//! `(:DevNote {id, kind, subsystem, title, content, paths, source, date,
//! embedding})` holds the brain's engineering history, subsystem rules and
//! plan decisions, so a coding agent queries the few notes it needs instead of
//! loading a book of markdown.
//!
//! - **A separate label, never `:Note`.** Dev history must not surface in the
//!   brain's retrieval about the world, so it has its own vector and fulltext
//!   indexes (`dev_note_embeddings`, `dev_note_fulltext`) and nothing that
//!   reads `:Note` sees it.
//! - **The graph is the authority; git keeps a seed.** `devdocs/devnotes.jsonl`
//!   (no embeddings) seeds an empty graph, create-only, and is rewritten after
//!   every `write` so git always has a current copy. No agent loads the file.
//! - **Embeddings are backfilled**, so a note written while the GPU is leased
//!   (or the embedder is down) is still found by BM25 and gains its vector at
//!   the next start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::knowledge::sanitize_lucene_query;
use crate::traits::LlmProvider;
use agent_brain_repository::Neo4jClient;

/// `rule` (still true; needed to change code safely), `history` (why, an
/// incident, a measurement), `decision` (a finished plan), `plan` (designed,
/// not built).
pub const KINDS: &[&str] = &["rule", "history", "decision", "plan"];

/// bge-m3 fails on long dense text (~5.8K chars); embed a prefix, search the rest by BM25.
const EMBED_CHARS: usize = 4000;
const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DevNote {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub subsystem: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub date: String,
}

/// `{CODEBASE_DIR}/devdocs/devnotes.jsonl` (the repo is bind-mounted read-write).
pub fn seed_file() -> PathBuf {
    let root = std::env::var("CODEBASE_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(root).join("devdocs").join("devnotes.jsonl")
}

pub struct DevNotes {
    neo4j: Neo4jClient,
    llm: Arc<dyn LlmProvider>,
}

impl DevNotes {
    pub fn new(neo4j: Neo4jClient, llm: Arc<dyn LlmProvider>) -> Self {
        Self { neo4j, llm }
    }

    async fn embed(&self, n: &DevNote) -> Option<Vec<f32>> {
        let text: String = format!("{}\n{}", n.title, n.content)
            .chars()
            .take(EMBED_CHARS)
            .collect();
        match self.llm.embed(&text).await {
            Ok(v) if !v.is_empty() => Some(v),
            Ok(_) => None,
            Err(e) => {
                warn!(id = %n.id, error = %e, "dev note not embedded (BM25 still finds it)");
                None
            }
        }
    }

    /// MERGE by id. `overwrite: false` is the seed path: an existing note wins.
    async fn upsert(&self, n: &DevNote, overwrite: bool) -> Result<()> {
        let set = "SET d.kind = $kind, d.subsystem = $subsystem, d.title = $title, \
                   d.content = $content, d.paths = $paths, d.source = $source, d.date = $date";
        let cypher = if overwrite {
            format!("MERGE (d:DevNote {{id: $id}}) {set}")
        } else {
            format!("MERGE (d:DevNote {{id: $id}}) ON CREATE {set}")
        };
        self.neo4j
            .run(
                neo4rs::query(&cypher)
                    .param("id", n.id.as_str())
                    .param("kind", n.kind.as_str())
                    .param("subsystem", n.subsystem.as_str())
                    .param("title", n.title.as_str())
                    .param("content", n.content.as_str())
                    .param("paths", n.paths.clone())
                    .param("source", n.source.as_str())
                    .param("date", n.date.as_str()),
            )
            .await?;
        Ok(())
    }

    async fn set_embedding(&self, id: &str, v: Vec<f32>) -> Result<()> {
        self.neo4j
            .run(
                neo4rs::query("MATCH (d:DevNote {id: $id}) SET d.embedding = $v")
                    .param("id", id)
                    .param("v", v),
            )
            .await?;
        Ok(())
    }

    /// Seed an EMPTY graph from the jsonl file, then embed any note without a
    /// vector. Returns (seeded, embedded). A missing file seeds nothing.
    pub async fn seed_and_backfill(&self, file: &Path) -> Result<(usize, usize)> {
        let existing = self.count().await?;
        let mut seeded = 0;
        if existing == 0 && file.exists() {
            for n in read_jsonl(file)? {
                self.upsert(&n, false).await?;
                seeded += 1;
            }
            info!(seeded, file = %file.display(), "dev notes seeded from git");
        }
        let rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (d:DevNote) WHERE d.embedding IS NULL \
                 RETURN d.id AS id, d.title AS title, d.content AS content",
            ))
            .await?;
        let mut embedded = 0;
        for row in rows {
            let n = DevNote {
                id: row.get("id").unwrap_or_default(),
                title: row.get("title").unwrap_or_default(),
                content: row.get("content").unwrap_or_default(),
                kind: String::new(),
                subsystem: String::new(),
                paths: vec![],
                source: String::new(),
                date: String::new(),
            };
            let Some(v) = self.embed(&n).await else {
                break; // embedder down or leased: the rest waits for the next start
            };
            self.set_embedding(&n.id, v).await?;
            embedded += 1;
        }
        Ok((seeded, embedded))
    }

    pub async fn count(&self) -> Result<i64> {
        let rows = self
            .neo4j
            .execute(neo4rs::query("MATCH (d:DevNote) RETURN count(d) AS n"))
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0))
    }

    /// Write a note (new or replacing one with the same id), embed it, and
    /// re-export the seed file. Returns the id.
    pub async fn write(&self, mut n: DevNote) -> Result<String> {
        anyhow::ensure!(
            KINDS.contains(&n.kind.as_str()),
            "kind must be one of {KINDS:?}"
        );
        anyhow::ensure!(
            !n.title.trim().is_empty() && !n.content.trim().is_empty(),
            "title and content are required"
        );
        if n.id.trim().is_empty() {
            n.id = format!("{}/{}", slug(&n.subsystem), slug(&n.title));
        }
        if n.date.is_empty() {
            n.date = agent_brain_graph::clock::today();
        }
        // A rewrite keeps where the note came from: the former doc name in
        // `source` is what a cited `HBI_REDESIGN_PLAN.md §9` is searched by.
        if let Some(old) = self.get(&n.id).await?
            && !old.source.is_empty()
        {
            n.source = old.source;
        }
        self.upsert(&n, true).await?;
        if let Some(v) = self.embed(&n).await {
            self.set_embedding(&n.id, v).await?;
        }
        self.export(&seed_file()).await?;
        Ok(n.id)
    }

    pub async fn get(&self, id: &str) -> Result<Option<DevNote>> {
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(
                    "MATCH (d:DevNote {id: $id}) RETURN d.id AS id, d.kind AS kind, \
                     d.subsystem AS subsystem, d.title AS title, d.content AS content, \
                     d.paths AS paths, d.source AS source, d.date AS date",
                )
                .param("id", id),
            )
            .await?;
        Ok(rows.first().map(row_to_note))
    }

    /// Hybrid search (vector + BM25, reciprocal-rank merged), then filtered by
    /// `kind` and by substrings: `subsystem` (also matching the note's source,
    /// so a former doc's name like `HBI_REDESIGN_PLAN` finds its notes) and `path`. Returns (hits, matched): the
    /// hits capped at `limit`, and how many matched before the cap.
    pub async fn search(
        &self,
        query: &str,
        kind: Option<&str>,
        subsystem: Option<&str>,
        path: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Value>, usize)> {
        // Filters apply after the merge, so a filtered search draws a wider pool:
        // otherwise the note it wants can rank outside the unfiltered top 60.
        let filtered = [kind, subsystem, path].iter().any(|f| f.is_some());
        let pool: i64 = if filtered { 400 } else { 60 };
        let mut scores: HashMap<String, f64> = HashMap::new();
        if let Some(v) = self
            .embed(&DevNote {
                id: String::new(),
                kind: String::new(),
                subsystem: String::new(),
                title: String::new(),
                content: query.to_string(),
                paths: vec![],
                source: String::new(),
                date: String::new(),
            })
            .await
        {
            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(
                        "CALL db.index.vector.queryNodes('dev_note_embeddings', $k, $v) \
                         YIELD node RETURN node.id AS id",
                    )
                    .param("k", pool)
                    .param("v", v),
                )
                .await
                .unwrap_or_default();
            for (rank, r) in rows.iter().enumerate() {
                if let Ok(id) = r.get::<String>("id") {
                    *scores.entry(id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
                }
            }
        }
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(
                    "CALL db.index.fulltext.queryNodes('dev_note_fulltext', $q, {limit: $k}) \
                     YIELD node RETURN node.id AS id",
                )
                .param("q", sanitize_lucene_query(query))
                .param("k", pool),
            )
            .await
            .unwrap_or_default();
        for (rank, r) in rows.iter().enumerate() {
            if let Ok(id) = r.get::<String>("id") {
                *scores.entry(id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }
        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(
                    "UNWIND $ids AS id MATCH (d:DevNote {id: id}) \
                     WHERE ($kind = '' OR d.kind = $kind) \
                       AND ($sub = '' OR toLower(d.subsystem) CONTAINS toLower($sub) \
                            OR toLower(d.source) CONTAINS toLower($sub)) \
                       AND ($path = '' OR any(p IN d.paths WHERE p CONTAINS $path) \
                            OR d.content CONTAINS $path) \
                     RETURN d.id AS id, d.kind AS kind, d.subsystem AS subsystem, \
                            d.title AS title, d.content AS content, d.paths AS paths, \
                            d.source AS source, d.date AS date",
                )
                .param("ids", ids)
                .param("kind", kind.unwrap_or(""))
                .param("sub", subsystem.unwrap_or(""))
                .param("path", path.unwrap_or("")),
            )
            .await?;
        let by_id: HashMap<String, DevNote> = rows
            .iter()
            .map(row_to_note)
            .map(|n| (n.id.clone(), n))
            .collect();
        let matched: Vec<&DevNote> = ranked.iter().filter_map(|(id, _)| by_id.get(id)).collect();
        let total = matched.len();
        let hits = matched
            .into_iter()
            .take(limit)
            .map(|n| {
                json!({
                    "id": n.id, "kind": n.kind, "subsystem": n.subsystem, "title": n.title,
                    "paths": n.paths, "source": n.source, "date": n.date,
                    "snippet": n.content.chars().take(300).collect::<String>(),
                })
            })
            .collect();
        Ok((hits, total))
    }

    /// Every note, ordered by id, without embeddings — the git seed.
    pub async fn export(&self, file: &Path) -> Result<usize> {
        let rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (d:DevNote) RETURN d.id AS id, d.kind AS kind, d.subsystem AS subsystem, \
                 d.title AS title, d.content AS content, d.paths AS paths, d.source AS source, \
                 d.date AS date ORDER BY d.id",
            ))
            .await?;
        let mut out = String::new();
        for r in &rows {
            out.push_str(&serde_json::to_string(&row_to_note(r))?);
            out.push('\n');
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("jsonl.tmp");
        std::fs::write(&tmp, out).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, file)?;
        Ok(rows.len())
    }
}

fn row_to_note(r: &neo4rs::Row) -> DevNote {
    DevNote {
        id: r.get("id").unwrap_or_default(),
        kind: r.get("kind").unwrap_or_default(),
        subsystem: r.get("subsystem").unwrap_or_default(),
        title: r.get("title").unwrap_or_default(),
        content: r.get("content").unwrap_or_default(),
        paths: r.get("paths").unwrap_or_default(),
        source: r.get("source").unwrap_or_default(),
        date: r.get("date").unwrap_or_default(),
    }
}

pub fn read_jsonl(file: &Path) -> Result<Vec<DevNote>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str::<DevNote>(l)
                .with_context(|| format!("{}:{}: bad dev note", file.display(), i + 1))
        })
        .collect()
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-');
    s.chars()
        .take(60)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_stable_and_short() {
        assert_eq!(
            slug("Evaluator Loop (Generator-Evaluator)"),
            "evaluator-loop-generator-evaluator"
        );
        assert_eq!(slug("`services/queue.rs`"), "services-queue-rs");
        assert!(slug(&"x".repeat(200)).len() <= 60);
    }

    #[test]
    fn jsonl_round_trips_and_names_a_bad_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("d.jsonl");
        let n = DevNote {
            id: "a/b".into(),
            kind: "rule".into(),
            subsystem: "queue".into(),
            title: "t".into(),
            content: "c".into(),
            paths: vec!["services/queue.rs".into()],
            source: "s".into(),
            date: "2026-10-10".into(),
        };
        std::fs::write(&f, format!("{}\n\n", serde_json::to_string(&n).unwrap())).unwrap();
        assert_eq!(read_jsonl(&f).unwrap(), vec![n]);
        std::fs::write(&f, "{not json}\n").unwrap();
        let err = format!("{:#}", read_jsonl(&f).unwrap_err());
        assert!(err.contains("d.jsonl:1"), "{err}");
    }
}
