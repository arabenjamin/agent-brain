//! `(:Revision)` storage: the history of what the brain edits about itself
//! (OUTSIDE_REVIEW_PLAN Phase 2). A revision records one definition of a
//! workflow, agent profile or program, who wrote it and why.
//!
//! Append-only. Neo4j Community has no trigger to refuse an update, so the
//! guarantee is that this file holds the only Cypher that creates a revision,
//! nothing sets or deletes one (a test scans the workspace for it), and
//! `prev_hash` chains the hashes so an edit made some other way shows up.

use neo4rs::{BoltNull, BoltType, Query, query};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Neo4jClient, RepositoryError};

/// What a revision is a revision of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionKind {
    Workflow,
    Profile,
    Program,
}

impl RevisionKind {
    pub const ALL: [RevisionKind; 3] = [Self::Workflow, Self::Profile, Self::Program];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Profile => "profile",
            Self::Program => "program",
        }
    }

    /// The label of the node a revision hangs off.
    pub fn label(self) -> &'static str {
        match self {
            Self::Workflow => "Workflow",
            Self::Profile => "ContextProfile",
            Self::Program => "Program",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One stored revision.
#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    pub kind: String,
    pub name: String,
    pub version: i64,
    /// Canonical JSON (see [`canonical`]); empty on a `delete` revision.
    pub definition: String,
    pub content_hash: String,
    pub prev_hash: Option<String>,
    pub author: String,
    pub reason: Option<String>,
    /// The version the writer read; absent on a create or a seed.
    pub based_on: Option<i64>,
    /// `save`, `revert`, `seed`, `baseline`, `delete` or `outside`.
    pub op: String,
    pub at: Option<String>,
}

/// A revision to write.
pub struct NewRevision<'a> {
    pub kind: RevisionKind,
    pub name: &'a str,
    pub definition: &'a str,
    pub author: &'a str,
    pub reason: Option<&'a str>,
    pub based_on: Option<i64>,
    pub op: &'a str,
}

/// How an append ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Appended {
    /// Written, at this version.
    Written(i64),
    /// Nothing written: the owner is no longer at the version the writer read.
    /// It is at this one now.
    Moved(i64),
}

/// A definition's stable text: keys sorted at every depth, so two definitions
/// that say the same thing hash the same whatever order they were built in.
pub fn canonical(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(v).to_string()
}

/// The sha-256 of a canonical definition, in hex.
pub fn content_hash(definition: &str) -> String {
    Sha256::digest(definition.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What is wrong with one object's history, oldest first: a version skipped, a
/// definition that no longer matches its hash, a broken `prev_hash` link. Empty
/// means nothing was edited outside [`Neo4jClient::append_revision`].
pub fn chain_problems(revisions: &[Revision]) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev: Option<&Revision> = None;
    for r in revisions {
        if content_hash(&r.definition) != r.content_hash {
            out.push(format!(
                "v{}: the definition does not match its hash",
                r.version
            ));
        }
        match prev {
            Some(p) => {
                if r.version != p.version + 1 {
                    out.push(format!("v{} follows v{}", r.version, p.version));
                }
                if r.prev_hash.as_deref() != Some(p.content_hash.as_str()) {
                    out.push(format!(
                        "v{}: prev_hash does not match v{}",
                        r.version, p.version
                    ));
                }
            }
            None if r.version != 1 || r.prev_hash.is_some() => {
                out.push(format!("the history starts at v{}", r.version));
            }
            None => {}
        }
        prev = Some(r);
    }
    out
}

fn row_to_revision(row: &neo4rs::Row) -> Revision {
    Revision {
        kind: row.get("kind").unwrap_or_default(),
        name: row.get("name").unwrap_or_default(),
        version: row.get("version").unwrap_or(0),
        definition: row.get("definition").unwrap_or_default(),
        content_hash: row.get("content_hash").unwrap_or_default(),
        prev_hash: row.get("prev_hash").ok(),
        author: row.get("author").unwrap_or_default(),
        reason: row.get("reason").ok(),
        based_on: row.get("based_on").ok(),
        op: row.get("op").unwrap_or_default(),
        at: row.get("at").ok(),
    }
}

const REVISION_FIELDS: &str = "r.kind AS kind, r.name AS name, r.version AS version, \
     r.definition AS definition, r.content_hash AS content_hash, r.prev_hash AS prev_hash, \
     r.author AS author, r.reason AS reason, r.based_on AS based_on, r.op AS op, \
     toString(r.at) AS at";

fn opt_str(v: Option<&str>) -> BoltType {
    v.map_or(BoltType::Null(BoltNull), |s| BoltType::from(s.to_string()))
}

fn opt_i64(v: Option<i64>) -> BoltType {
    v.map_or(BoltType::Null(BoltNull), BoltType::from)
}

impl Neo4jClient {
    /// One object's newest revision. By kind and name, not by edge: a deleted
    /// object's history stays, and a new one of that name continues it.
    pub async fn latest_revision(
        &self,
        kind: RevisionKind,
        name: &str,
    ) -> Result<Option<Revision>, RepositoryError> {
        Ok(self
            .revisions(kind, name, Some(1))
            .await?
            .into_iter()
            .next())
    }

    /// One object's revisions, newest first.
    pub async fn revisions(
        &self,
        kind: RevisionKind,
        name: &str,
        limit: Option<i64>,
    ) -> Result<Vec<Revision>, RepositoryError> {
        let cypher = format!(
            "MATCH (r:Revision {{kind: $kind, name: $name}}) RETURN {REVISION_FIELDS} \
             ORDER BY r.version DESC LIMIT $limit"
        );
        let rows = self
            .execute(
                query(&cypher)
                    .param("kind", kind.as_str())
                    .param("name", name)
                    .param("limit", limit.unwrap_or(10_000)),
            )
            .await?;
        Ok(rows.iter().map(row_to_revision).collect())
    }

    /// Every revision of a kind, by name then oldest first: the boot's chain check.
    pub async fn revisions_of_kind(
        &self,
        kind: RevisionKind,
    ) -> Result<Vec<Revision>, RepositoryError> {
        let cypher = format!(
            "MATCH (r:Revision {{kind: $kind}}) RETURN {REVISION_FIELDS} \
             ORDER BY r.name, r.version"
        );
        let rows = self
            .execute(query(&cypher).param("kind", kind.as_str()))
            .await?;
        Ok(rows.iter().map(row_to_revision).collect())
    }

    /// One revision by version.
    pub async fn revision(
        &self,
        kind: RevisionKind,
        name: &str,
        version: i64,
    ) -> Result<Option<Revision>, RepositoryError> {
        let cypher = format!(
            "MATCH (r:Revision {{kind: $kind, name: $name, version: $version}}) \
             RETURN {REVISION_FIELDS}"
        );
        let rows = self
            .execute(
                query(&cypher)
                    .param("kind", kind.as_str())
                    .param("name", name)
                    .param("version", version),
            )
            .await?;
        Ok(rows.first().map(row_to_revision))
    }

    /// The version an object is at, 0 when it has none yet (or does not exist).
    pub async fn live_version(
        &self,
        kind: RevisionKind,
        name: &str,
    ) -> Result<i64, RepositoryError> {
        let cypher = format!(
            "MATCH (o:{} {{name: $name}}) RETURN coalesce(o.revision, 0) AS v",
            kind.label()
        );
        let rows = self.execute(query(&cypher).param("name", name)).await?;
        Ok(rows
            .first()
            .and_then(|r| r.get::<i64>("v").ok())
            .unwrap_or(0))
    }

    /// The only way a revision is written. In one transaction: `before` (the
    /// write that creates the owner), the revision itself if the owner is still
    /// at `expected`, then `after` (the write that changes the owner). So a
    /// definition and its revision land together or not at all, and of two
    /// writers who read the same version one lands and the other is told.
    ///
    /// The owner's lock is taken before its version is read, so the second
    /// writer waits for the first to commit and then sees the moved version.
    /// `drift` is what [`Neo4jClient`]'s owner shows as its difference from its
    /// file. The `revision_uid` constraint is the backstop.
    pub async fn append_revision(
        &self,
        rev: &NewRevision<'_>,
        expected: i64,
        drift: Option<&str>,
        before: Vec<Query>,
        after: Vec<Query>,
    ) -> Result<Appended, RepositoryError> {
        let latest = self.latest_revision(rev.kind, rev.name).await?;
        let version = latest.as_ref().map_or(0, |r| r.version) + 1;
        let hash = content_hash(rev.definition);
        let cypher = format!(
            "MATCH (o:{label} {{name: $name}}) SET o._revision_lock = true \
             WITH o WHERE coalesce(o.revision, 0) = $expected \
             CREATE (o)-[:REVISION]->(r:Revision {{uid: $uid, kind: $kind, name: $name, \
                 version: $version, definition: $definition, content_hash: $hash, \
                 prev_hash: $prev, author: $author, reason: $reason, based_on: $based_on, \
                 op: $op, at: datetime()}}) \
             SET o.revision = $version, o.live_hash = $live_hash, o.drift = $drift \
             REMOVE o._revision_lock \
             RETURN r.version AS version",
            label = rev.kind.label()
        );
        let live_hash = if rev.op == "delete" {
            None
        } else {
            Some(&hash)
        };
        let insert = query(&cypher)
            .param("name", rev.name)
            .param("expected", expected)
            .param(
                "uid",
                format!("{}/{}/{version}", rev.kind.as_str(), rev.name),
            )
            .param("kind", rev.kind.as_str())
            .param("version", version)
            .param("definition", rev.definition)
            .param("hash", hash.as_str())
            .param(
                "prev",
                opt_str(latest.as_ref().map(|r| r.content_hash.as_str())),
            )
            .param("author", rev.author)
            .param("reason", opt_str(rev.reason))
            .param("based_on", opt_i64(rev.based_on))
            .param("op", rev.op)
            .param("live_hash", opt_str(live_hash.map(String::as_str)))
            .param("drift", opt_str(drift));

        let result: Result<bool, neo4rs::Error> = async {
            let mut txn = self.graph().start_txn().await?;
            for q in before {
                txn.run(q).await?;
            }
            let mut stream = txn.execute(insert).await?;
            let mut landed = false;
            while stream.next(txn.handle()).await?.is_some() {
                landed = true;
            }
            if !landed {
                txn.rollback().await?;
                return Ok(false);
            }
            for q in after {
                txn.run(q).await?;
            }
            txn.commit().await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => Ok(Appended::Written(version)),
            Ok(false) => Ok(Appended::Moved(
                self.live_version(rev.kind, rev.name).await?,
            )),
            // Two writers past the check at once: the constraint stopped one.
            Err(e) if e.to_string().contains("ConstraintValidation") => Ok(Appended::Moved(
                self.live_version(rev.kind, rev.name).await?,
            )),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rev(version: i64, definition: &str, prev: Option<&str>) -> Revision {
        Revision {
            kind: "workflow".into(),
            name: "w".into(),
            version,
            definition: definition.into(),
            content_hash: content_hash(definition),
            prev_hash: prev.map(content_hash),
            author: "a".into(),
            reason: None,
            based_on: None,
            op: "save".into(),
            at: None,
        }
    }

    #[test]
    fn the_same_definition_in_any_key_order_hashes_the_same() {
        let a = json!({"b": 1, "a": {"y": [{"q": 1, "p": 2}], "x": 2}});
        let b = json!({"a": {"x": 2, "y": [{"p": 2, "q": 1}]}, "b": 1});
        assert_eq!(canonical(&a), canonical(&b));
        assert_ne!(canonical(&a), canonical(&json!({"b": 2})));
    }

    #[test]
    fn a_chain_with_an_edit_in_it_says_where() {
        let good = vec![rev(1, "one", None), rev(2, "two", Some("one"))];
        assert!(chain_problems(&good).is_empty());

        let mut edited = good.clone();
        edited[0].definition = "changed".into();
        assert!(chain_problems(&edited)[0].contains("v1"));

        let mut relinked = good.clone();
        relinked[1].prev_hash = Some(content_hash("other"));
        assert!(chain_problems(&relinked)[0].contains("prev_hash"));

        let gap = vec![rev(1, "one", None), rev(3, "two", Some("one"))];
        assert!(chain_problems(&gap)[0].contains("follows"));

        assert!(chain_problems(&[rev(2, "x", Some("y"))])[0].contains("starts"));
    }

    /// Cypher that binds a revision and then writes to it, in `source`.
    fn revision_writes(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for pat in [":Revision", "REVISION]->("] {
            for (at, _) in source.match_indices(pat) {
                let var: String = if pat == ":Revision" {
                    source[..at]
                        .chars()
                        .rev()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect()
                } else {
                    source[at + pat.len()..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect()
                };
                if var.is_empty() {
                    continue;
                }
                let rest: String = source[at..].chars().take(800).collect();
                let squeezed = rest.split_whitespace().collect::<Vec<_>>().join(" ");
                for write in [
                    format!("SET {var}."),
                    format!("SET {var} "),
                    format!(", {var}."),
                    format!("REMOVE {var}."),
                    format!("DELETE {var}"),
                ] {
                    // `, r.x` is only a write inside a SET; a RETURN list is not.
                    if write.starts_with(", ") {
                        let Some(set) = squeezed.find(" SET ") else {
                            continue;
                        };
                        let after_set = &squeezed[set..];
                        let end = after_set.find(" RETURN ").unwrap_or(after_set.len());
                        if !after_set[..end].contains(&write) {
                            continue;
                        }
                    } else if !squeezed.contains(&write) {
                        continue;
                    }
                    found.push(squeezed.chars().take(160).collect());
                    break;
                }
            }
        }
        found
    }

    fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !matches!(name.as_str(), "target" | "node_modules" | "vendor") {
                    files_under(&p, out);
                }
            } else if name.ends_with(".rs") || name.ends_with(".yaml") || name.ends_with(".yml") {
                out.push(p);
            }
        }
    }

    #[test]
    fn the_scan_catches_a_write_to_a_revision() {
        for bad in [
            "MATCH (r:Revision {name: $n}) SET r.definition = $d",
            "MATCH (x:Revision) DETACH DELETE x",
            "MATCH (w:Workflow)-[:REVISION]->(rev) DELETE rev",
            "MATCH (r:Revision {name: $n}) \\\n   SET r.a = 1, r.definition = $d",
        ] {
            assert!(!revision_writes(bad).is_empty(), "missed: {bad}");
        }
        for fine in [
            "MATCH (r:Revision {kind: $k}) RETURN r.kind AS kind, r.name AS name",
            "CREATE (o)-[:REVISION]->(r:Revision {uid: $u}) SET o.revision = 1, o.drift = $d RETURN r.version",
        ] {
            assert!(revision_writes(fine).is_empty(), "flagged: {fine}");
        }
    }

    /// The append-only guarantee (see the module doc).
    #[test]
    fn nothing_in_the_workspace_writes_to_a_revision() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        files_under(&root.join("crates"), &mut files);
        files_under(&root.join("plugins"), &mut files);
        assert!(files.len() > 100, "the scan found only {}", files.len());
        let mut bad = Vec::new();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            // A file's own tests may spell out what must not happen.
            let code = text.split("#[cfg(test)]").next().unwrap_or(&text);
            for w in revision_writes(code) {
                bad.push(format!("{}: {w}", f.display()));
            }
        }
        assert!(
            bad.is_empty(),
            "Cypher writes to a :Revision:\n{}",
            bad.join("\n")
        );
    }
}
