//! Topics: the domains the brain works on or keeps track of, and which notes
//! belong to each (`HBI_REDESIGN_PLAN.md` §3).
//!
//! - **Declared, not discovered.** the core bundle's `topics/*.yaml` names each
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
//! - **The YAML seeds; the graph owns.** `init-db` mirrors the files into
//!   `(:Topic)` nodes, but a topic edited in the HBI is `managed_by: runtime`
//!   and the sync leaves it alone (the rule schedules follow). A seeded topic
//!   deleted in the HBI leaves a `(:RetiredTopic)` so the sync does not bring
//!   it back. [`Topics::global`] is the graph's set, reloaded after every edit,
//!   so a change takes effect without a restart.
//! - **The user's word is final** (HBI plan Phase 15). Topics set on a note in
//!   the HBI are `method: 'manual'` and the note is `topics_locked`: the
//!   classifier never picks it, and [`reload`] stamps it with every new
//!   version so it never counts as waiting. `(:Entity)-[:NOT_IN_TOPIC]->(:Topic)`
//!   keeps an entity out of a topic's map and entity lists, however many of
//!   the topic's notes mention it.
//! - **Assignment runs when the brain is idle** ([`spawn_assigner`]): no job
//!   queued or running, no GPU lease. It never competes with real work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use agent_brain_llm::LlmProvider;
use agent_brain_repository::Neo4jClient;

/// Text sent to the classifier per note.
const CLASSIFY_CHARS: usize = 1500;
/// At most this many topics per note (the prompt asks for one, two when equal).
const MAX_TOPICS_PER_NOTE: usize = 2;
/// A note whose classification failed this many times is left alone.
pub const MAX_FAILURES: i64 = 3;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct TopicDef {
    pub name: String,
    pub label: String,
    /// What belongs here — this text *is* the classifier's definition.
    pub description: String,
    /// SourceLists its searches use.
    #[serde(default)]
    pub source_lists: Vec<String>,
    /// Who puts notes in it: the classifier (default), or only `members`
    /// rules and the user.
    #[serde(default)]
    pub assign: Assign,
    /// `assign: rules`: a note matching any rule joins the topic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberRule>,
    /// A page about the topic, set by the user (HBI plan Phase 18). Most
    /// topics are the user's own groupings with no Wikipedia article, so
    /// there is no automatic lookup. Not part of the version: editing it
    /// classifies nothing again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Other names people use for it ("Opensource Intelligence Agency" for
    /// `nia`). `lookup` resolves them to the name. Not part of the version:
    /// they classify nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The plugin bundle whose `topics/` declares it (`None` = the core).
    /// Set by the loader, never read from the file; not part of the version.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// How notes get into a topic.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Assign {
    /// The local model decides from the description.
    #[default]
    Classifier,
    /// Only `members` rules and the user. The topic is left out of the
    /// classifier's prompt. For a topic about the user (their career): the
    /// model can't tell whose career a note is about, and a small topic in its
    /// prompt drew notes that fit nothing else (HBI plan Phase 15).
    Rules,
}

impl Assign {
    pub fn as_str(self) -> &'static str {
        match self {
            Assign::Classifier => "classifier",
            Assign::Rules => "rules",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "classifier" => Some(Assign::Classifier),
            "rules" => Some(Assign::Rules),
            _ => None,
        }
    }
}

/// A note joins a rules topic when every field the rule gives matches:
/// `note_type` and `source_context` exactly, `content_contains` as a
/// case-insensitive substring of the note's first 1,500 characters.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemberRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_contains: Option<String>,
}

impl MemberRule {
    fn is_empty(&self) -> bool {
        [
            &self.note_type,
            &self.source_context,
            &self.content_contains,
        ]
        .iter()
        .all(|f| f.as_deref().is_none_or(|v| v.trim().is_empty()))
    }

    pub fn matches(&self, note: &PendingNote) -> bool {
        let eq = |want: &Option<String>, have: &Option<String>| {
            want.as_deref()
                .is_none_or(|w| have.as_deref() == Some(w.trim()))
        };
        !self.is_empty()
            && eq(&self.note_type, &note.note_type)
            && eq(&self.source_context, &note.source_context)
            && self.content_contains.as_deref().is_none_or(|c| {
                note.content
                    .to_lowercase()
                    .contains(&c.trim().to_lowercase())
            })
    }
}

/// A note waiting to be classified: what the classifier and the rules read.
#[derive(Debug, Clone, Default)]
pub struct PendingNote {
    pub id: String,
    pub content: String,
    pub note_type: Option<String>,
    pub source_context: Option<String>,
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
    /// Changes whenever a name, a description or the scope changes. Dormant
    /// topics (their plugin is off) count too, so turning a plugin off or on
    /// does not send every note back to the classifier.
    pub version: String,
}

impl Topics {
    pub fn from_defs(defs: Vec<TopicDef>) -> Self {
        Self::with_scope(defs, Scope::default())
    }

    pub fn with_scope(defs: Vec<TopicDef>, scope: Scope) -> Self {
        Self::with_dormant(defs, scope, &[])
    }

    /// `defs` are offered to the classifier; `dormant` (topics of plugins
    /// that are off) only count toward the version.
    pub fn with_dormant(mut defs: Vec<TopicDef>, scope: Scope, dormant: &[TopicDef]) -> Self {
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs.dedup_by(|a, b| a.name == b.name);
        let mut all: Vec<&TopicDef> = defs.iter().chain(dormant).collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all.dedup_by(|a, b| a.name == b.name);
        let mut h = Fnv::default();
        for d in all {
            h.write(d.name.as_bytes());
            h.write(b"\x1f");
            h.write(d.description.trim().as_bytes());
            // Only a rules topic adds to the hash, so classifier topics keep
            // the version they had before `assign` existed.
            if d.assign == Assign::Rules {
                h.write(b"\x1frules");
                for r in &d.members {
                    for f in [&r.note_type, &r.source_context, &r.content_contains] {
                        h.write(b"\x1f");
                        h.write(f.as_deref().unwrap_or("").as_bytes());
                    }
                }
            }
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

    /// The current topics: the graph's set once [`reload`] has run, the
    /// YAML files until then. Cheap to call; take it once per unit of work.
    pub fn global() -> Arc<Topics> {
        cell().read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_global(t: Topics) -> Arc<Topics> {
        let t = Arc::new(t);
        *cell().write().unwrap_or_else(|e| e.into_inner()) = t.clone();
        t
    }

    /// The topic named `name`, if declared.
    pub fn get(&self, name: &str) -> Option<&TopicDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// The topics the classifier may pick (not the `assign: rules` ones).
    fn classifier_defs(&self) -> impl Iterator<Item = &TopicDef> {
        self.defs.iter().filter(|d| d.assign == Assign::Classifier)
    }

    /// The rules topics a note matches, in name order.
    pub fn rule_topics(&self, note: &PendingNote) -> Vec<String> {
        self.defs
            .iter()
            .filter(|d| d.assign == Assign::Rules && d.members.iter().any(|r| r.matches(note)))
            .map(|d| d.name.clone())
            .collect()
    }

    /// The classifier prompt. Kept terse: it runs on the local model.
    pub fn prompt(&self, text: &str) -> String {
        let list: String = self
            .classifier_defs()
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
        let known: HashSet<&str> = self.classifier_defs().map(|d| d.name.as_str()).collect();
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

fn cell() -> &'static RwLock<Arc<Topics>> {
    static T: OnceLock<RwLock<Arc<Topics>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(Arc::new(Topics::load_bundles())))
}

/// The core bundle's `topics/` (its `_scope.yaml` is the only scope).
pub fn topics_dir() -> PathBuf {
    crate::bundles::Plugins::global().core_dir("topics")
}

/// The loaded bundles that are not the core: whose topics may be offered.
fn loaded_plugins() -> Vec<String> {
    crate::bundles::Plugins::global()
        .loaded()
        .map(|b| b.name.clone())
        .filter(|n| n != crate::bundles::CORE)
        .collect()
}

impl Topics {
    /// The core's topics and scope, then each loaded plugin's `topics/`
    /// (CORE_AND_PLUGINS_PLAN Phase 4: one topic per file; a plugin's
    /// `_scope.yaml` is ignored). A name already declared is skipped with a
    /// warning: a plugin cannot replace a core topic.
    pub fn load_bundles() -> Self {
        let core = Self::load_dir(&topics_dir());
        let mut defs = core.defs;
        for (plugin, dir) in crate::bundles::Plugins::global().dirs("topics") {
            if plugin == crate::bundles::CORE {
                continue;
            }
            for mut d in Self::load_dir(&dir).defs {
                if defs.iter().any(|e| e.name == d.name) {
                    warn!(%plugin, topic = %d.name, "topics: a plugin declares a topic that already exists — skipped");
                    continue;
                }
                d.plugin = Some(plugin.clone());
                defs.push(d);
            }
        }
        Self::with_scope(defs, core.scope)
    }
}

/// FNV-1a: a version hash that is stable across Rust releases (unlike `DefaultHasher`).
pub struct Fnv(pub(crate) u64);
impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}
impl Fnv {
    pub fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Mirror the YAML into `(:Topic)` nodes, then [`reload`]. A topic edited in
/// the HBI (`managed_by: 'runtime'`) is left as it is, and a seeded topic the
/// user deleted (`(:RetiredTopic)`) is not recreated. A YAML-owned topic no
/// longer declared is removed with its `IN_TOPIC` edges. Run by `init-db`.
pub async fn sync(neo4j: &Neo4jClient, topics: &Topics) -> Result<usize> {
    let rows: Vec<neo4rs::BoltType> = topics.defs.iter().map(def_row).collect();
    neo4j
        .run(
            neo4rs::query(
                "UNWIND $rows AS r \
                 OPTIONAL MATCH (gone:RetiredTopic {name: r.name}) \
                 WITH r WHERE gone IS NULL \
                 MERGE (t:Topic {name: r.name}) \
                 ON CREATE SET t.created_at = datetime(), t.managed_by = 'yaml' \
                 WITH t, r WHERE coalesce(t.managed_by, 'yaml') <> 'runtime' \
                 REMOVE t.schedules \
                 SET t.label = r.label, t.description = r.description, \
                     t.source_lists = r.source_lists, t.assign = r.assign, \
                     t.members = r.members, t.reference = r.reference, t.aliases = r.aliases, t.managed_by = 'yaml', \
                     t.plugin = r.plugin, t.synced_at = datetime()",
            )
            .param("rows", rows),
        )
        .await?;
    let names: Vec<String> = topics.defs.iter().map(|d| d.name.clone()).collect();
    let gone = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Topic) WHERE NOT t.name IN $names \
                   AND coalesce(t.managed_by, 'yaml') <> 'runtime' \
                   AND (coalesce(t.plugin, '') = '' OR t.plugin IN $loaded) \
                 WITH t, t.name AS name DETACH DELETE t RETURN collect(name) AS gone",
            )
            .param("names", names)
            // A topic whose plugin is off stays, dormant, with its notes:
            // it comes back with the plugin.
            .param("loaded", loaded_plugins()),
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
    Ok(reload(neo4j).await?.defs.len())
}

fn def_row(d: &TopicDef) -> neo4rs::BoltType {
    let mut m = std::collections::HashMap::new();
    m.insert("name".to_string(), neo4rs::BoltType::from(d.name.clone()));
    m.insert("label".to_string(), d.label.trim().to_string().into());
    m.insert(
        "description".to_string(),
        d.description.trim().to_string().into(),
    );
    m.insert("source_lists".to_string(), d.source_lists.clone().into());
    m.insert("assign".to_string(), d.assign.as_str().into());
    m.insert(
        "members".to_string(),
        serde_json::to_string(&d.members)
            .unwrap_or_else(|_| "[]".into())
            .into(),
    );
    m.insert(
        "reference".to_string(),
        d.reference
            .as_deref()
            .map(|r| r.trim().to_string().into())
            .unwrap_or(neo4rs::BoltType::Null(neo4rs::BoltNull)),
    );
    m.insert(
        "aliases".to_string(),
        d.aliases
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect::<Vec<_>>()
            .into(),
    );
    m.insert(
        "plugin".to_string(),
        d.plugin
            .clone()
            .map(Into::into)
            .unwrap_or(neo4rs::BoltType::Null(neo4rs::BoltNull)),
    );
    m.into()
}

/// Read the graph's topics into [`Topics::global`] (the scope still comes
/// from `topics/_scope.yaml`), and stamp each topic with the version. A graph
/// with no topics keeps the YAML set, so a fresh database still classifies.
pub async fn reload(neo4j: &Neo4jClient) -> Result<Arc<Topics>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (t:Topic) \
             RETURN t.name AS name, coalesce(t.label, t.name) AS label, \
                    coalesce(t.description, '') AS description, \
                    coalesce(t.source_lists, []) AS source_lists, \
                    coalesce(t.assign, 'classifier') AS assign, \
                    coalesce(t.members, '[]') AS members, t.reference AS reference, \
                    coalesce(t.aliases, []) AS aliases, t.plugin AS plugin",
        ))
        .await?;
    let defs: Vec<TopicDef> = rows
        .iter()
        .filter_map(|r| {
            Some(TopicDef {
                name: r.get::<String>("name").ok()?,
                label: r.get::<String>("label").unwrap_or_default(),
                description: r.get::<String>("description").unwrap_or_default(),
                source_lists: r.get::<Vec<String>>("source_lists").unwrap_or_default(),
                assign: Assign::parse(&r.get::<String>("assign").unwrap_or_default())
                    .unwrap_or_default(),
                members: serde_json::from_str(&r.get::<String>("members").unwrap_or_default())
                    .unwrap_or_default(),
                reference: r.get::<String>("reference").ok().filter(|s| !s.is_empty()),
                aliases: r.get::<Vec<String>>("aliases").unwrap_or_default(),
                plugin: r.get::<String>("plugin").ok().filter(|s| !s.is_empty()),
            })
        })
        .filter(|d| !d.description.trim().is_empty())
        .collect();
    // A topic whose plugin is off stays in the graph with its notes, out of
    // the classifier's prompt but in the version.
    let loaded = loaded_plugins();
    let (defs, dormant): (Vec<TopicDef>, Vec<TopicDef>) = defs
        .into_iter()
        .partition(|d| d.plugin.as_ref().is_none_or(|p| loaded.contains(p)));
    if defs.is_empty() {
        return Ok(Topics::global());
    }
    let scope = Topics::load_dir(&topics_dir()).scope;
    let topics = Topics::set_global(Topics::with_dormant(defs, scope, &dormant));
    neo4j
        .run(
            neo4rs::query("MATCH (t:Topic) SET t.version = $v").param("v", topics.version.as_str()),
        )
        .await?;
    // A rules topic holds no classifier edges: ones written before the topic
    // became `assign: rules` go now, not when each note is next classified.
    neo4j
        .run(neo4rs::query(
            "MATCH (:Note)-[r:IN_TOPIC {method: 'llm'}]->(:Topic {assign: 'rules'}) DELETE r",
        ))
        .await?;
    // Rules need no model: apply them now, so a rules topic is whole without
    // waiting hours for the classifier to reach old notes. Mirrors
    // `MemberRule::matches`; the classifier rewrites these edges consistently
    // when it reaches each note.
    let rules: Vec<neo4rs::BoltType> = topics
        .defs
        .iter()
        .filter(|d| d.assign == Assign::Rules)
        .flat_map(|d| d.members.iter().map(move |r| (d, r)))
        .filter(|(_, r)| !r.is_empty())
        .map(|(d, r)| {
            let mut m = std::collections::HashMap::new();
            let f = |v: &Option<String>| v.as_deref().unwrap_or("").trim().to_string();
            m.insert("topic".to_string(), neo4rs::BoltType::from(d.name.clone()));
            m.insert("note_type".to_string(), f(&r.note_type).into());
            m.insert("source_context".to_string(), f(&r.source_context).into());
            m.insert(
                "contains".to_string(),
                f(&r.content_contains).to_lowercase().into(),
            );
            m.into()
        })
        .collect();
    if !rules.is_empty() {
        neo4j
            .run(
                neo4rs::query(&format!(
                    "UNWIND $rules AS r \
                     MATCH (t:Topic {{name: r.topic}}) \
                     MATCH (n:Note) \
                     WHERE NOT (n)-[:PART_OF]->() AND NOT coalesce(n.topics_locked, false) \
                       AND NOT {SKIPPED_CYPHER} \
                       AND (r.note_type = '' OR n.note_type = r.note_type) \
                       AND (r.source_context = '' OR n.source_context = r.source_context) \
                       AND (r.contains = '' OR toLower(substring(coalesce(n.content, ''), 0, {CLASSIFY_CHARS})) CONTAINS r.contains) \
                     MERGE (n)-[e:IN_TOPIC]->(t) \
                     ON CREATE SET e.rank = 1, e.method = 'rule', e.assigned_at = datetime()"
                ))
                .param("rules", rules)
                .param("skip", topics.skip_param()),
            )
            .await?;
    }
    // A note the user set keeps its topics under every version.
    neo4j
        .run(
            neo4rs::query("MATCH (n:Note {topics_locked: true}) SET n.topics_version = $v")
                .param("v", topics.version.as_str()),
        )
        .await?;
    Ok(topics)
}

/// Longest description accepted: the definitions share one local-model prompt.
pub const MAX_DESCRIPTION: usize = 500;
/// The classifier prompt (every definition plus the rules) may not grow past
/// this; it runs on the local model. See CLAUDE.md "a local small-model
/// prompt is a size budget".
pub const PROMPT_BUDGET: usize = 3600;

/// Is `def` acceptable as a new (`create`) or changed topic among `current`?
/// Checked before anything is written; the message says what to change.
pub fn check_def(def: &TopicDef, current: &Topics, create: bool) -> Result<(), String> {
    let name = def.name.as_str();
    let ok_name = (2..=40).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok_name {
        return Err(format!(
            "name '{name}' must be 2–40 lowercase letters, digits and hyphens, starting with a letter or digit"
        ));
    }
    if name == "map" || name == "new" {
        return Err(format!("'{name}' is reserved (the HBI uses it in links)"));
    }
    match (create, current.get(name).is_some()) {
        (true, true) => return Err(format!("a topic named '{name}' already exists")),
        (false, false) => return Err(format!("no topic named '{name}'")),
        _ => {}
    }
    if def.label.trim().is_empty() || def.label.trim().chars().count() > 60 {
        return Err("label must be 1–60 characters".into());
    }
    let len = def.description.trim().chars().count();
    if len == 0 {
        return Err("description is required: it is the definition the classifier reads".into());
    }
    if len > MAX_DESCRIPTION {
        return Err(format!(
            "description is {len} characters; keep it under {MAX_DESCRIPTION} (every definition shares one local-model prompt)"
        ));
    }
    if def.members.iter().any(MemberRule::is_empty) {
        return Err(
            "a member rule needs at least one of note_type, source_context, content_contains"
                .into(),
        );
    }
    if let Some(r) = def.reference.as_deref() {
        let r = r.trim();
        if !(r.starts_with("https://") || r.starts_with("http://"))
            || r.len() > 500
            || r.contains(char::is_whitespace)
        {
            return Err("reference must be one http(s) URL of at most 500 characters".into());
        }
    }
    if def.aliases.len() > 8
        || def
            .aliases
            .iter()
            .any(|a| a.trim().is_empty() || a.trim().chars().count() > 60)
    {
        return Err("aliases: at most 8, each 1–60 characters".into());
    }
    let taken = |a: &str| {
        current.defs.iter().filter(|d| d.name != def.name).find(|d| {
            d.name.eq_ignore_ascii_case(a)
                || d.label.trim().eq_ignore_ascii_case(a)
                || d.aliases.iter().any(|x| x.trim().eq_ignore_ascii_case(a))
        })
    };
    if let Some((a, d)) = def.aliases.iter().find_map(|a| taken(a.trim()).map(|d| (a, d))) {
        return Err(format!(
            "alias '{}' already names topic '{}'",
            a.trim(),
            d.name
        ));
    }
    if def.assign == Assign::Classifier && !def.members.is_empty() {
        return Err(
            "member rules apply only with assign: rules (the classifier ignores them)".into(),
        );
    }
    let mut defs: Vec<TopicDef> = current
        .defs
        .iter()
        .filter(|d| d.name != name)
        .cloned()
        .collect();
    defs.push(def.clone());
    let prompt = Topics::with_scope(defs, Scope::default()).prompt("").len();
    if prompt > PROMPT_BUDGET {
        return Err(format!(
            "with this definition the classifier prompt is {prompt} characters, over its {PROMPT_BUDGET} budget; shorten this or another topic's description"
        ));
    }
    Ok(())
}

/// Create or update a topic from the HBI. It becomes `managed_by: 'runtime'`,
/// so its YAML file (if any) no longer applies. A changed description changes
/// the version, and every note is classified again on idle time.
pub async fn save(
    neo4j: &Neo4jClient,
    def: &TopicDef,
    create: bool,
) -> Result<Result<Arc<Topics>, String>> {
    let current = Topics::global();
    if let Err(e) = check_def(def, &current, create) {
        return Ok(Err(e));
    }
    neo4j
        .run(
            neo4rs::query(
                "OPTIONAL MATCH (gone:RetiredTopic {name: $r.name}) DETACH DELETE gone \
                 WITH 1 AS _ \
                 MERGE (t:Topic {name: $r.name}) \
                 ON CREATE SET t.created_at = datetime() \
                 SET t.label = $r.label, t.description = $r.description, \
                     t.source_lists = $r.source_lists, t.assign = $r.assign, \
                     t.members = $r.members, t.reference = $r.reference, t.aliases = $r.aliases, t.managed_by = 'runtime', \
                     t.updated_at = datetime()",
            )
            .param("r", def_row(def)),
        )
        .await?;
    Ok(Ok(reload(neo4j).await?))
}

/// What deleting a topic did, or why it was refused.
#[derive(Debug, Serialize)]
pub struct Deleted {
    pub name: String,
    /// Notes that were in it; they are classified again among the rest.
    pub notes: i64,
    /// A YAML file declares it: a `(:RetiredTopic)` keeps the sync from
    /// bringing it back.
    pub retired_seed: bool,
}

/// Delete a topic. Refused while a schedule declares it (the error names
/// them), since every schedule's topic must exist.
pub async fn delete(neo4j: &Neo4jClient, name: &str) -> Result<Result<Deleted, String>> {
    if Topics::global().get(name).is_none() {
        return Ok(Err(format!("no topic named '{name}'")));
    }
    let schedules: Vec<String> = neo4j
        .execute(
            neo4rs::query("MATCH (s:Workflow {topic: $name}) RETURN s.name AS n ORDER BY n")
                .param("name", name),
        )
        .await?
        .iter()
        .filter_map(|r| r.get::<String>("n").ok())
        .collect();
    if !schedules.is_empty() {
        return Ok(Err(format!(
            "{} schedule(s) declare this topic; give them another topic first: {}",
            schedules.len(),
            schedules.join(", ")
        )));
    }
    let retired_seed = Topics::load_bundles().get(name).is_some();
    let notes = neo4j
        .execute(
            neo4rs::query(
                // Its notes are marked waiting: if the same definitions come
                // back (a reset), the version repeats, and a note still stamped
                // with it would look classified without this topic. A note the
                // user locked to it goes back to the classifier too.
                "MATCH (t:Topic {name: $name}) \
                 OPTIONAL MATCH (n:Note)-[:IN_TOPIC]->(t) \
                 WITH t, collect(n) AS ns \
                 FOREACH (n IN ns | REMOVE n.topics_version, n.topics_locked) \
                 WITH t, size(ns) AS notes \
                 DETACH DELETE t RETURN notes",
            )
            .param("name", name),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("notes").ok())
        .unwrap_or(0);
    if retired_seed {
        neo4j
            .run(
                neo4rs::query("MERGE (g:RetiredTopic {name: $name}) SET g.retired_at = datetime()")
                    .param("name", name),
            )
            .await?;
    }
    reload(neo4j).await?;
    Ok(Ok(Deleted {
        name: name.to_string(),
        notes,
        retired_seed,
    }))
}

/// Put a topic back to its YAML definition (undoing HBI edits or a delete).
pub async fn reset(neo4j: &Neo4jClient, name: &str) -> Result<Result<Arc<Topics>, String>> {
    let Some(seed) = Topics::load_bundles().get(name).cloned() else {
        return Ok(Err(format!("no topics/*.yaml file declares '{name}'")));
    };
    neo4j
        .run(
            neo4rs::query(
                "OPTIONAL MATCH (gone:RetiredTopic {name: $r.name}) DETACH DELETE gone \
                 WITH 1 AS _ \
                 MERGE (t:Topic {name: $r.name}) \
                 ON CREATE SET t.created_at = datetime() \
                 SET t.label = $r.label, t.description = $r.description, \
                     t.source_lists = $r.source_lists, t.assign = $r.assign, \
                     t.members = $r.members, t.reference = $r.reference, t.managed_by = 'yaml', \
                     t.plugin = $r.plugin, t.synced_at = datetime() REMOVE t.updated_at",
            )
            .param("r", def_row(&seed)),
        )
        .await?;
    Ok(Ok(reload(neo4j).await?))
}

/// The topics the user gives a note, checked: known names, no repeats, at
/// most [`MAX_TOPICS_PER_NOTE`]. Empty means "fits no topic".
pub fn check_note_topics(names: &[String], topics: &Topics) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for n in names.iter().map(|n| n.trim()) {
        if topics.get(n).is_none() {
            return Err(format!(
                "no topic named '{n}'; topics are: {}",
                topics
                    .defs
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    }
    if out.len() > MAX_TOPICS_PER_NOTE {
        return Err(format!(
            "at most {MAX_TOPICS_PER_NOTE} topics a note (the main one first)"
        ));
    }
    Ok(out)
}

/// Set a note's topics by hand (main first) and lock them: the classifier
/// leaves the note alone from now on. `Ok(Err)` is a refusal to show the user.
pub async fn set_note_topics(
    neo4j: &Neo4jClient,
    note_id: &str,
    names: &[String],
) -> Result<Result<Vec<String>, String>> {
    let topics = Topics::global();
    let picked = match check_note_topics(names, &topics) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
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
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note {id: $id}) WHERE NOT (n)-[:PART_OF]->() \
                 OPTIONAL MATCH (n)-[old:IN_TOPIC]->() DELETE old \
                 WITH DISTINCT n \
                 SET n.topics_locked = true, n.topics_version = $version, \
                     n.topics_at = datetime() REMOVE n.topics_failed \
                 WITH n \
                 CALL (n) { \
                   UNWIND $ranked AS r \
                   MATCH (t:Topic {name: r.name}) \
                   MERGE (n)-[e:IN_TOPIC]->(t) \
                   SET e.rank = r.rank, e.method = 'manual', e.assigned_at = datetime() \
                   RETURN count(*) AS c \
                 } \
                 RETURN n.id AS id",
            )
            .param("id", note_id)
            .param("version", topics.version.as_str())
            .param("ranked", ranked),
        )
        .await?;
    if rows.is_empty() {
        return Ok(Err(format!(
            "no note '{note_id}', or it is a chunk (a chunk's topics are its parent's)"
        )));
    }
    Ok(Ok(picked))
}

/// Hand a note back to the classifier: it is unlocked and waits to be
/// classified again; its current topics stay until then.
pub async fn unlock_note(neo4j: &Neo4jClient, note_id: &str) -> Result<bool> {
    Ok(!neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note {id: $id}) REMOVE n.topics_locked, n.topics_version RETURN n.id AS id",
            )
            .param("id", note_id),
        )
        .await?
        .is_empty())
}

/// Keep an entity out of a topic (`hide`) or let it back in. Returns false
/// when the entity or the topic does not exist.
pub async fn hide_entity(
    neo4j: &Neo4jClient,
    entity_id: &str,
    topic: &str,
    hide: bool,
) -> Result<bool> {
    let cypher = if hide {
        "MATCH (e:Entity {id: $id}), (t:Topic {name: $topic}) \
         MERGE (e)-[h:NOT_IN_TOPIC]->(t) ON CREATE SET h.at = datetime() RETURN e.id AS id"
    } else {
        "MATCH (e:Entity {id: $id}), (t:Topic {name: $topic}) \
         OPTIONAL MATCH (e)-[h:NOT_IN_TOPIC]->(t) DELETE h RETURN e.id AS id"
    };
    Ok(!neo4j
        .execute(
            neo4rs::query(cypher)
                .param("id", entity_id)
                .param("topic", topic),
        )
        .await?
        .is_empty())
}

/// Classify one note and write its `IN_TOPIC` edges. `Ok(None)` when the
/// model's answer could not be used (counted as a failure on the note).
pub async fn classify_note(
    neo4j: &Neo4jClient,
    llm: &dyn LlmProvider,
    topics: &Topics,
    note: &PendingNote,
) -> Result<Option<Vec<String>>> {
    let note_id = note.id.as_str();
    // A rules topic it matches comes first; the classifier adds its own.
    let mut picked = topics.rule_topics(note);
    let classified = match llm
        .generate_json(&topics.prompt(&note.content), None, &["topics"], 1)
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
    for t in classified {
        if !picked.contains(&t) {
            picked.push(t);
        }
    }
    picked.truncate(MAX_TOPICS_PER_NOTE);
    let rules = topics.rule_topics(note);
    let ranked: Vec<neo4rs::BoltType> = picked
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut m = std::collections::HashMap::new();
            m.insert("name".to_string(), neo4rs::BoltType::from(t.clone()));
            m.insert("rank".to_string(), ((i + 1) as i64).into());
            m.insert(
                "method".to_string(),
                if rules.contains(t) { "rule" } else { "llm" }.into(),
            );
            m.into()
        })
        .collect();
    neo4j
        .run(
            neo4rs::query(
                // Edges to a dormant topic (its plugin is off) are not the
                // classifier's to judge: they stay for when it comes back.
                "MATCH (n:Note {id: $id}) \
                 OPTIONAL MATCH (n)-[old:IN_TOPIC]->(ot:Topic) \
                   WHERE coalesce(ot.plugin, '') = '' OR ot.plugin IN $loaded \
                 DELETE old \
                 WITH DISTINCT n \
                 SET n.topics_version = $version, n.topics_at = datetime() REMOVE n.topics_failed \
                 WITH n \
                 UNWIND $ranked AS r \
                 MATCH (t:Topic {name: r.name}) \
                 MERGE (n)-[e:IN_TOPIC]->(t) \
                 SET e.rank = r.rank, e.method = r.method, e.assigned_at = datetime()",
            )
            .param("id", note_id)
            .param("version", topics.version.as_str())
            .param("loaded", loaded_plugins())
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
/// is its parent's text; skipped notes excluded), newest first. A claim is
/// classified with its source's title: alone, a one-sentence claim about
/// Paragon's spyware was filed under agent-brain.
async fn pending_batch(neo4j: &Neo4jClient, topics: &Topics, n: i64) -> Result<Vec<PendingNote>> {
    let rows = neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (n:Note) \
                 WHERE coalesce(n.topics_version, '') <> $version \
                   AND coalesce(n.topics_failed, 0) < $max_failures \
                   AND NOT (n)-[:PART_OF]->() \
                   AND NOT coalesce(n.topics_locked, false) \
                   AND NOT {SKIPPED_CYPHER} \
                 RETURN n.id AS id, substring(coalesce(n.content, '') + \
                   CASE WHEN n.note_type = 'claim' AND n.source_title IS NOT NULL \
                     THEN '\\n(Source: ' + n.source_title + ')' ELSE '' END, 0, $chars) AS content, \
                   n.note_type AS note_type, n.source_context AS source_context \
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
            Some(PendingNote {
                id: r.get::<String>("id").ok()?,
                content: r.get::<String>("content").unwrap_or_default(),
                note_type: r.get::<String>("note_type").ok(),
                source_context: r.get::<String>("source_context").ok(),
            })
        })
        .collect())
}

/// Is the brain busy? Any job queued or running, or the GPU leased.
pub async fn busy(neo4j: &Neo4jClient) -> bool {
    if agent_brain_llm::gpu_lease::held().is_some() {
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
    tokio::spawn(async move {
        // The graph's topics (HBI edits included), not only the YAML files.
        if let Err(e) = reload(&neo4j).await {
            warn!(error = %e, "topic assigner: could not read topics from the graph; using topics/*.yaml");
        }
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
            // Re-read each batch: an edit in the HBI applies from the next one.
            let topics = Topics::global();
            if topics.defs.is_empty() {
                continue;
            }
            let topics = topics.as_ref();
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
            for (i, note) in batch.iter().enumerate() {
                if i > 0 && i % 5 == 0 && busy(&neo4j).await {
                    interrupted = true;
                    break;
                }
                match classify_note(&neo4j, llm.as_ref(), topics, note).await {
                    Ok(Some(_)) => done += 1,
                    Ok(None) => failed += 1,
                    Err(e) => {
                        warn!(note_id = %note.id, error = %e, "topic assigner: write failed");
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
                source_lists: vec![],
                ..Default::default()
            },
            TopicDef {
                name: "ai-ml".into(),
                label: "AI".into(),
                description: "AI as a field.".into(),
                source_lists: vec![],
                ..Default::default()
            },
            TopicDef {
                name: "geopolitics".into(),
                label: "Geopolitics".into(),
                description: "Countries.".into(),
                source_lists: vec![],
                ..Default::default()
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
        d[0].source_lists = vec!["x".into()];
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
    fn a_plugin_going_off_keeps_the_version_and_leaves_the_prompt() {
        let all = Topics::from_defs(vec![def("a", "Alpha."), def("b", "Beta.")]);
        let off = Topics::with_dormant(
            vec![def("a", "Alpha.")],
            Scope::default(),
            &[def("b", "Beta.")],
        );
        assert_eq!(off.version, all.version);
        assert_eq!(off.defs.len(), 1);
        assert!(!off.prompt("").contains("Beta."));
        // A dormant topic's definition still counts: editing it is a change.
        let edited = Topics::with_dormant(
            vec![def("a", "Alpha.")],
            Scope::default(),
            &[def("b", "Bet.")],
        );
        assert_ne!(edited.version, all.version);
    }

    fn def(name: &str, description: &str) -> TopicDef {
        TopicDef {
            name: name.into(),
            label: "A label".into(),
            description: description.into(),
            source_lists: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn check_def_names_what_to_change() {
        let t = defs();
        assert!(check_def(&def("my-job-search", "The user's own career."), &t, true).is_ok());
        assert!(check_def(&def("ai-ml", "Changed."), &t, false).is_ok());
        let err = |d: TopicDef, create: bool| check_def(&d, &t, create).unwrap_err();
        assert!(err(def("ai-ml", "x"), true).contains("already exists"));
        assert!(err(def("nope", "x"), false).contains("no topic"));
        assert!(err(def("Bad Name", "x"), true).contains("lowercase"));
        assert!(err(def("-x", "x"), true).contains("lowercase"));
        assert!(err(def("map", "x"), true).contains("reserved"));
        assert!(err(def("new", "x"), true).contains("reserved"));
        assert!(err(def("new-one", "  "), true).contains("required"));
        assert!(err(def("new-one", &"y".repeat(MAX_DESCRIPTION + 1)), true).contains("under"));
        let mut with_ref = def("new-one", "x");
        with_ref.reference = Some("https://en.wikipedia.org/wiki/Geopolitics".into());
        assert!(check_def(&with_ref, &t, true).is_ok());
        with_ref.reference = Some("not a url".into());
        assert!(
            check_def(&with_ref, &t, true)
                .unwrap_err()
                .contains("reference")
        );
        let mut no_label = def("new-one", "x");
        no_label.label = " ".into();
        assert!(
            check_def(&no_label, &t, true)
                .unwrap_err()
                .contains("label")
        );
        let mut nia = def("nia", "The user's own program.");
        nia.aliases = vec!["Opensource Intelligence Agency".into()];
        assert!(check_def(&nia, &t, true).is_ok());
        nia.aliases = vec!["AI".into()];
        assert!(check_def(&nia, &t, true).unwrap_err().contains("already names topic 'ai-ml'"));
        nia.aliases = vec![" ".into()];
        assert!(check_def(&nia, &t, true).unwrap_err().contains("aliases"));
    }

    fn rules_topic() -> TopicDef {
        TopicDef {
            name: "my-job-search".into(),
            label: "Career".into(),
            description: "The user's own career.".into(),
            assign: Assign::Rules,
            members: vec![
                MemberRule {
                    source_context: Some("resume".into()),
                    ..Default::default()
                },
                MemberRule {
                    source_context: Some("chat_session".into()),
                    content_contains: Some("[Session: JOBSEARCH-".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn note(ctx: Option<&str>, content: &str) -> PendingNote {
        PendingNote {
            id: "n".into(),
            content: content.into(),
            note_type: Some("episodic".into()),
            source_context: ctx.map(String::from),
        }
    }

    #[test]
    fn a_rules_topic_is_never_offered_to_the_classifier() {
        let mut d = defs().defs;
        d.push(rules_topic());
        let t = Topics::from_defs(d);
        assert!(
            !t.prompt("").contains("my-job-search"),
            "kept out of the prompt"
        );
        assert!(t.prompt("").contains("- ai-ml:"));
        assert!(
            t.parse(&json!({"topics": ["my-job-search", "ai-ml"]})) == vec!["ai-ml"],
            "the model can't pick it even if it names it"
        );
    }

    #[test]
    fn member_rules_match_every_field_they_give() {
        let mut d = defs().defs;
        d.push(rules_topic());
        let t = Topics::from_defs(d);
        assert_eq!(
            t.rule_topics(&note(Some("resume"), "anything")),
            vec!["my-job-search"]
        );
        assert_eq!(
            t.rule_topics(&note(
                Some("chat_session"),
                "Chat turn [session: jobsearch-2026-09-09] …"
            )),
            vec!["my-job-search"],
            "content_contains ignores case"
        );
        assert!(
            t.rule_topics(&note(
                Some("chat_session"),
                "Chat turn [session: abc] aidevboard"
            ))
            .is_empty(),
            "both fields must match"
        );
        assert!(
            t.rule_topics(&note(None, "[session: jobsearch-x]"))
                .is_empty()
        );
        assert!(
            !MemberRule::default().matches(&note(Some("resume"), "")),
            "an empty rule matches nothing"
        );
    }

    #[test]
    fn rules_count_toward_the_version_and_are_checked() {
        let base = Topics::from_defs(vec![rules_topic()]);
        let mut changed = rules_topic();
        changed.members.pop();
        assert_ne!(Topics::from_defs(vec![changed]).version, base.version);
        let t = defs();
        let mut bad = rules_topic();
        bad.members.push(MemberRule::default());
        assert!(
            check_def(&bad, &t, true)
                .unwrap_err()
                .contains("at least one")
        );
        let mut stray = rules_topic();
        stray.assign = Assign::Classifier;
        assert!(
            check_def(&stray, &t, true)
                .unwrap_err()
                .contains("only with assign: rules")
        );
        assert!(check_def(&rules_topic(), &t, true).is_ok());
    }

    #[test]
    fn note_topics_are_known_unique_and_at_most_two() {
        let t = defs();
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            check_note_topics(&v(&["ai-ml"]), &t).unwrap(),
            v(&["ai-ml"])
        );
        assert_eq!(
            check_note_topics(&v(&["geopolitics", "ai-ml", "geopolitics"]), &t).unwrap(),
            v(&["geopolitics", "ai-ml"]),
            "order kept, repeats dropped"
        );
        assert!(
            check_note_topics(&[], &t).unwrap().is_empty(),
            "none is a choice"
        );
        assert!(
            check_note_topics(&v(&["career"]), &t)
                .unwrap_err()
                .contains("no topic named")
        );
        assert!(
            check_note_topics(&v(&["ai-ml", "geopolitics", "us-politics"]), &t)
                .unwrap_err()
                .contains("at most 2")
        );
    }

    #[test]
    fn check_def_holds_the_prompt_budget() {
        // Fill the prompt near its budget with legal descriptions, then one more tips it.
        let mut many = Vec::new();
        for i in 0..6 {
            many.push(def(&format!("t{i}"), &"z".repeat(MAX_DESCRIPTION)));
        }
        let t = Topics::from_defs(many);
        let e = check_def(&def("t9", &"z".repeat(MAX_DESCRIPTION)), &t, true).unwrap_err();
        assert!(e.contains("budget"), "{e}");
        // Shortening an existing one is always allowed.
        assert!(check_def(&def("t0", "short"), &t, false).is_ok());
    }

    #[test]
    fn the_shipped_topics_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/topics");
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
