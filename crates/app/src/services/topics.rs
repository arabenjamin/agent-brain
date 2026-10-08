//! Topics: the domains the brain works on or keeps track of, and which notes
//! belong to each (`HBI_REDESIGN_PLAN.md` §3).
//!
//! - **Declared, not discovered.** `topics/*.yaml` (`TOPICS_DIR`) names each
//!   topic and describes it; `init-db` syncs them to `(:Topic)` nodes.
//!   "Domain" already means a web hostname in SourceLists and claims, hence Topic.
//! - **The local model assigns them.** Each note is classified against the
//!   topic descriptions (main topic first, a second only when it is as
//!   central), giving `(:Note)-[:IN_TOPIC {rank, method: 'llm', assigned_at}]->(:Topic)`.
//!   Embedding similarity to topic centroids was measured first and was not
//!   good enough (F1 0.61 against hand labels, against 0.83 on a held-out set
//!   for the classifier; see `engineering-history.md` § Topics). A side
//!   benefit: notes with no embedding (most episodic records) get topics too.
//! - **Every note is accounted for.** A classified note carries
//!   `topics_version` (a hash of the topic definitions). A note with that
//!   version and no `IN_TOPIC` was assessed and fits none; a note without it is
//!   still waiting. Editing a topic changes the version, so every note is
//!   classified again.
//! - **Assignment runs when the brain is idle** ([`spawn_assigner`]): no job
//!   queued or running, no GPU lease. It never competes with real work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::repository::Neo4jClient;
use crate::services::LlmProvider;

/// Text sent to the classifier per note.
const CLASSIFY_CHARS: usize = 1500;
/// At most this many topics per note (the prompt asks for one, two when equal).
const MAX_TOPICS_PER_NOTE: usize = 2;
/// A note whose classification failed this many times is left alone.
pub const MAX_FAILURES: i64 = 3;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TopicDef {
    pub name: String,
    pub label: String,
    /// What belongs here — this text *is* the classifier's definition.
    pub description: String,
    /// ScheduledTasks that watch this topic (by name).
    #[serde(default)]
    pub schedules: Vec<String>,
    /// SourceLists its searches use.
    #[serde(default)]
    pub source_lists: Vec<String>,
}

/// A note the classifier skips: every field given must match.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkipRule {
    #[serde(default)]
    pub note_type: Option<String>,
    #[serde(default)]
    pub source_context: Option<String>,
}

/// `topics/_scope.yaml`: which notes are classified at all.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub skip: Vec<SkipRule>,
}

#[derive(Debug, Default)]
pub struct Topics {
    pub defs: Vec<TopicDef>,
    pub scope: Scope,
    /// Changes whenever a name, a description or the scope changes.
    pub version: String,
}

impl Topics {
    pub fn from_defs(defs: Vec<TopicDef>) -> Self {
        Self::with_scope(defs, Scope::default())
    }

    pub fn with_scope(mut defs: Vec<TopicDef>, scope: Scope) -> Self {
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs.dedup_by(|a, b| a.name == b.name);
        let mut h = Fnv::default();
        for d in &defs {
            h.write(d.name.as_bytes());
            h.write(b"\x1f");
            h.write(d.description.trim().as_bytes());
            h.write(b"\x1e");
        }
        for r in &scope.skip {
            h.write(b"skip\x1f");
            h.write(r.note_type.as_deref().unwrap_or("").as_bytes());
            h.write(b"\x1f");
            h.write(r.source_context.as_deref().unwrap_or("").as_bytes());
            h.write(b"\x1e");
        }
        let version = if defs.is_empty() {
            String::new()
        } else {
            format!("{:016x}", h.0)
        };
        Self {
            defs,
            scope,
            version,
        }
    }

    /// The skip rules as Cypher parameters (`''` means "any").
    pub fn skip_param(&self) -> Vec<neo4rs::BoltType> {
        self.scope
            .skip
            .iter()
            .map(|r| {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "note_type".to_string(),
                    neo4rs::BoltType::from(r.note_type.clone().unwrap_or_default()),
                );
                m.insert(
                    "source_context".to_string(),
                    r.source_context.clone().unwrap_or_default().into(),
                );
                m.into()
            })
            .collect()
    }

    /// Every `*.yaml` in `dir`, one topic per file; `_scope.yaml` (and any
    /// `_*.yaml`) holds settings. A missing dir means no topics.
    pub fn load_dir(dir: &Path) -> Self {
        let mut defs = Vec::new();
        let mut scope = Scope::default();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for p in entries.flatten().map(|e| e.path()) {
                    if p.extension().and_then(|e| e.to_str()) != Some("yaml") {
                        continue;
                    }
                    if p.file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.starts_with('_'))
                    {
                        if p.file_name().and_then(|f| f.to_str()) == Some("_scope.yaml") {
                            match std::fs::read_to_string(&p)
                                .map_err(anyhow::Error::from)
                                .and_then(|t| serde_yaml::from_str::<Scope>(&t).map_err(Into::into))
                            {
                                Ok(s) => scope = s,
                                Err(e) => {
                                    warn!(path = %p.display(), error = %e, "topics: scope file skipped")
                                }
                            }
                        }
                        continue;
                    }
                    match std::fs::read_to_string(&p)
                        .map_err(anyhow::Error::from)
                        .and_then(|t| serde_yaml::from_str::<TopicDef>(&t).map_err(Into::into))
                    {
                        Ok(d) if !d.name.trim().is_empty() && !d.description.trim().is_empty() => {
                            defs.push(d)
                        }
                        Ok(_) => {
                            warn!(path = %p.display(), "topics: a topic needs a name and a description")
                        }
                        Err(e) => warn!(path = %p.display(), error = %e, "topics: file skipped"),
                    }
                }
            }
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "topics: no topics dir — notes get no topics")
            }
        }
        Self::with_scope(defs, scope)
    }

    pub fn global() -> &'static Topics {
        static T: OnceLock<Topics> = OnceLock::new();
        T.get_or_init(|| Topics::load_dir(&topics_dir()))
    }

    /// The classifier prompt. Kept terse: it runs on the local model.
    pub fn prompt(&self, text: &str) -> String {
        let list: String = self
            .defs
            .iter()
            .map(|d| format!("- {}: {}\n", d.name, d.description.trim()))
            .collect();
        let text: String = text.chars().take(CLASSIFY_CHARS).collect();
        format!(
            "Which of these topics is the text substantially about?\n{list}\
             Rules:\n\
             - First the ONE topic the text is mainly about. Add a second only if it is just as central. A passing mention does not count.\n\
             - If no topic fits (entertainment, sports, lifestyle, history, crime stories), the list is empty.\n\
             - Output ONLY JSON: {{\"topics\":[\"topic-name\"]}}\n\nTEXT:\n{text}"
        )
    }

    /// Known topic names from the model's answer, in its order, deduplicated.
    pub fn parse(&self, v: &Value) -> Vec<String> {
        let known: HashSet<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
        let mut out: Vec<String> = Vec::new();
        for t in v
            .get("topics")
            .and_then(|t| t.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(s) = t.as_str().map(|s| s.trim().to_lowercase())
                && known.contains(s.as_str())
                && !out.contains(&s)
            {
                out.push(s);
            }
        }
        out.truncate(MAX_TOPICS_PER_NOTE);
        out
    }
}

pub fn topics_dir() -> PathBuf {
    std::env::var("TOPICS_DIR")
        .unwrap_or_else(|_| "topics".into())
        .into()
}

/// FNV-1a: a version hash that is stable across Rust releases (unlike `DefaultHasher`).
struct Fnv(u64);
impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}
impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Mirror the YAML into `(:Topic)` nodes; a topic no longer declared is
/// removed with its `IN_TOPIC` edges. Run by `init-db`.
pub async fn sync(neo4j: &Neo4jClient, topics: &Topics) -> Result<usize> {
    let rows: Vec<neo4rs::BoltType> = topics
        .defs
        .iter()
        .map(|d| {
            let mut m = std::collections::HashMap::new();
            m.insert("name".to_string(), neo4rs::BoltType::from(d.name.clone()));
            m.insert("label".to_string(), d.label.clone().into());
            m.insert(
                "description".to_string(),
                d.description.trim().to_string().into(),
            );
            m.insert("schedules".to_string(), d.schedules.clone().into());
            m.insert("source_lists".to_string(), d.source_lists.clone().into());
            m.into()
        })
        .collect();
    neo4j
        .run(
            neo4rs::query(
                "UNWIND $rows AS r \
                 MERGE (t:Topic {name: r.name}) \
                 ON CREATE SET t.created_at = datetime() \
                 SET t.label = r.label, t.description = r.description, \
                     t.schedules = r.schedules, t.source_lists = r.source_lists, \
                     t.version = $version, t.synced_at = datetime()",
            )
            .param("rows", rows)
            .param("version", topics.version.as_str()),
        )
        .await?;
    let names: Vec<String> = topics.defs.iter().map(|d| d.name.clone()).collect();
    let gone = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Topic) WHERE NOT t.name IN $names \
                 WITH t, t.name AS name DETACH DELETE t RETURN collect(name) AS gone",
            )
            .param("names", names),
        )
        .await?
        .first()
        .and_then(|r| r.get::<Vec<String>>("gone").ok())
        .unwrap_or_default();
    if !gone.is_empty() {
        info!(removed = ?gone, "Topics no longer declared were removed");
    }
    // A note the scope now skips keeps no topic from before.
    neo4j
        .run(
            neo4rs::query(&format!(
                "MATCH (n:Note)-[r:IN_TOPIC]->() WHERE {SKIPPED_CYPHER} DELETE r"
            ))
            .param("skip", topics.skip_param()),
        )
        .await?;
    Ok(topics.defs.len())
}

/// Classify one note and write its `IN_TOPIC` edges. `Ok(None)` when the
/// model's answer could not be used (counted as a failure on the note).
pub async fn classify_note(
    neo4j: &Neo4jClient,
    llm: &dyn LlmProvider,
    topics: &Topics,
    note_id: &str,
    content: &str,
) -> Result<Option<Vec<String>>> {
    let picked = match llm
        .generate_json(&topics.prompt(content), None, &["topics"], 1)
        .await
    {
        Ok(v) => topics.parse(&v),
        Err(e) => {
            debug!(note_id, error = %e, "topic classification failed");
            neo4j
                .run(
                    neo4rs::query(
                        "MATCH (n:Note {id: $id}) SET n.topics_failed = coalesce(n.topics_failed, 0) + 1",
                    )
                    .param("id", note_id),
                )
                .await?;
            return Ok(None);
        }
    };
    let ranked: Vec<neo4rs::BoltType> = picked
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut m = std::collections::HashMap::new();
            m.insert("name".to_string(), neo4rs::BoltType::from(t.clone()));
            m.insert("rank".to_string(), ((i + 1) as i64).into());
            m.into()
        })
        .collect();
    neo4j
        .run(
            neo4rs::query(
                "MATCH (n:Note {id: $id}) \
                 OPTIONAL MATCH (n)-[old:IN_TOPIC]->() DELETE old \
                 WITH DISTINCT n \
                 SET n.topics_version = $version, n.topics_at = datetime() REMOVE n.topics_failed \
                 WITH n \
                 UNWIND $ranked AS r \
                 MATCH (t:Topic {name: r.name}) \
                 MERGE (n)-[e:IN_TOPIC]->(t) \
                 SET e.rank = r.rank, e.method = 'llm', e.assigned_at = datetime()",
            )
            .param("id", note_id)
            .param("version", topics.version.as_str())
            .param("ranked", ranked),
        )
        .await?;
    Ok(Some(picked))
}

/// Cypher: the note `n` is one the scope skips (`$skip` from [`Topics::skip_param`]).
/// Every side is coalesced: a note with no `source_context` must compare
/// false, not null — a null here drops the note from every coverage bucket.
pub const SKIPPED_CYPHER: &str = "any(r IN $skip WHERE (r.note_type = '' OR coalesce(n.note_type, '') = r.note_type) \
     AND (r.source_context = '' OR coalesce(n.source_context, '') = r.source_context))";

/// Notes still waiting for this topic version (top-level notes only: a chunk
/// is its parent's text; skipped notes excluded), newest first.
async fn pending_batch(
    neo4j: &Neo4jClient,
    topics: &Topics,
    n: i64,
) -> Result<Vec<(String, String)>> {
    let rows = neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (n:Note) \
                 WHERE coalesce(n.topics_version, '') <> $version \
                   AND coalesce(n.topics_failed, 0) < $max_failures \
                   AND NOT (n)-[:PART_OF]->() \
                   AND NOT {SKIPPED_CYPHER} \
                 RETURN n.id AS id, substring(coalesce(n.content, ''), 0, $chars) AS content \
                 ORDER BY n.created_at DESC LIMIT $n"
            ))
            .param("version", topics.version.as_str())
            .param("skip", topics.skip_param())
            .param("max_failures", MAX_FAILURES)
            .param("chars", CLASSIFY_CHARS as i64)
            .param("n", n),
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some((
                r.get::<String>("id").ok()?,
                r.get::<String>("content").unwrap_or_default(),
            ))
        })
        .collect())
}

/// Is the brain busy? Any job queued or running, or the GPU leased.
async fn busy(neo4j: &Neo4jClient) -> bool {
    if crate::services::gpu_lease::held().is_some() {
        return true;
    }
    neo4j
        .execute(neo4rs::query(
            "MATCH (j:AgentJob) WHERE j.status IN ['queued', 'running'] RETURN count(j) AS c",
        ))
        .await
        .ok()
        .and_then(|r| r.first().and_then(|r| r.get::<i64>("c").ok()))
        .map(|c| c > 0)
        .unwrap_or(true)
}

/// The background assigner: while the brain is idle, classify the notes that
/// are waiting, a few at a time, re-checking for real work between notes.
/// `TOPIC_ASSIGNER_ENABLED=false` turns it off.
pub fn spawn_assigner(neo4j: Neo4jClient, llm: Arc<dyn LlmProvider>) {
    if std::env::var("TOPIC_ASSIGNER_ENABLED")
        .map(|v| v.eq_ignore_ascii_case("false") || v == "0")
        .unwrap_or(false)
    {
        info!("Topic assigner disabled (TOPIC_ASSIGNER_ENABLED=false)");
        return;
    }
    let topics = Topics::global();
    if topics.defs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        const IDLE_POLL: Duration = Duration::from_secs(30);
        const BATCH: i64 = 25;
        // After a full batch with the brain still idle, go straight on; sleep
        // only when there is nothing to do or real work arrived.
        let mut keep_going = false;
        loop {
            if !keep_going {
                tokio::time::sleep(IDLE_POLL).await;
            }
            keep_going = false;
            if busy(&neo4j).await {
                continue;
            }
            let batch = match pending_batch(&neo4j, topics, BATCH).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "topic assigner: could not read pending notes");
                    continue;
                }
            };
            if batch.is_empty() {
                continue;
            }
            let (mut done, mut failed) = (0usize, 0usize);
            let mut interrupted = false;
            for (i, (id, content)) in batch.iter().enumerate() {
                if i > 0 && i % 5 == 0 && busy(&neo4j).await {
                    interrupted = true;
                    break;
                }
                match classify_note(&neo4j, llm.as_ref(), topics, id, content).await {
                    Ok(Some(_)) => done += 1,
                    Ok(None) => failed += 1,
                    Err(e) => {
                        warn!(note_id = %id, error = %e, "topic assigner: write failed");
                        failed += 1;
                    }
                }
            }
            debug!(done, failed, "topic assigner batch");
            keep_going = !interrupted && batch.len() as i64 == BATCH && done > 0;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defs() -> Topics {
        Topics::from_defs(vec![
            TopicDef {
                name: "us-politics".into(),
                label: "US politics".into(),
                description: "The US federal government.".into(),
                schedules: vec![],
                source_lists: vec![],
            },
            TopicDef {
                name: "ai-ml".into(),
                label: "AI".into(),
                description: "AI as a field.".into(),
                schedules: vec![],
                source_lists: vec![],
            },
            TopicDef {
                name: "geopolitics".into(),
                label: "Geopolitics".into(),
                description: "Countries.".into(),
                schedules: vec![],
                source_lists: vec![],
            },
        ])
    }

    #[test]
    fn parse_keeps_known_names_in_order_and_caps() {
        let t = defs();
        assert_eq!(
            t.parse(&json!({"topics": ["US-Politics", "made-up", "ai-ml", "geopolitics"]})),
            vec!["us-politics", "ai-ml"]
        );
        assert_eq!(
            t.parse(&json!({"topics": ["ai-ml", "ai-ml"]})),
            vec!["ai-ml"]
        );
        assert!(t.parse(&json!({"topics": []})).is_empty());
        // The nested echo the model produced once in calibration.
        assert!(t.parse(&json!({"topics": [{"topics": []}]})).is_empty());
        assert!(t.parse(&json!({"other": 1})).is_empty());
    }

    #[test]
    fn version_follows_names_and_descriptions_only() {
        let a = defs();
        let mut d = defs().defs;
        d[0].label = "Renamed label".into();
        d[0].schedules = vec!["x".into()];
        assert_eq!(
            Topics::from_defs(d.clone()).version,
            a.version,
            "labels and links don't re-classify"
        );
        d[0].description = "Changed.".into();
        assert_ne!(Topics::from_defs(d).version, a.version);
        let mut rev = defs().defs;
        rev.reverse();
        assert_eq!(
            Topics::from_defs(rev).version,
            a.version,
            "file order doesn't matter"
        );
        assert_eq!(Topics::from_defs(vec![]).version, "");
        let scoped = Topics::with_scope(
            defs().defs,
            Scope {
                skip: vec![SkipRule {
                    note_type: Some("episodic".into()),
                    source_context: None,
                }],
            },
        );
        assert_ne!(scoped.version, a.version, "a scope change re-classifies");
    }

    #[test]
    fn prompt_lists_every_topic_and_clips_the_text() {
        let t = defs();
        let long = "x".repeat(5000);
        let p = t.prompt(&long);
        assert!(p.contains("- ai-ml: AI as a field."));
        assert!(p.contains("- us-politics: The US federal government."));
        assert!(p.ends_with(&format!("TEXT:\n{}", "x".repeat(CLASSIFY_CHARS))));
    }

    #[test]
    fn the_shipped_topics_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../topics");
        let t = Topics::load_dir(&dir);
        assert!(t.defs.len() >= 5, "topics/*.yaml should load");
        assert!(
            t.defs.iter().all(|d| !d.name.starts_with('_')),
            "_scope.yaml is settings, not a topic"
        );
        assert!(!t.scope.skip.is_empty(), "_scope.yaml should load");
        assert!(t.defs.iter().all(|d| !d.label.is_empty()));
        // The prompt is a local-model budget: keep it near 2-3k chars.
        assert!(
            t.prompt("").len() < 3200,
            "prompt is {} chars",
            t.prompt("").len()
        );
    }
}
