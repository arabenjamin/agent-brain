//! SnapshotService — compressed knowledge graph backup and restore.
//!
//! Snapshots are gzip-compressed JSON files (`*.json.gz`) containing all
//! Note, Task, Entity, and Procedure nodes plus their inter-relationships.
//! Embeddings are excluded — use `backfill_endpoint_embeddings` after restore.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::repository::Neo4jClient;

/// Current snapshot schema version — increment when struct fields change.
pub const SCHEMA_VERSION: u32 = 1;

// ─── Snapshot data structs ────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct NoteRecord {
    pub id: String,
    pub content: String,
    pub note_type: String,
    pub created_at: String,
    pub last_accessed_at: String,
    pub access_count: i64,
    pub source_context: Option<String>,
    pub event_at: Option<String>,
    pub next_review_at: String,
    pub review_interval_days: i64,
}

#[derive(Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub goal: String,
    pub context: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
pub struct EntityRecord {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
pub struct ProcedureRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub steps_json: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
pub struct RelationshipRecord {
    pub rel_type: String,
    pub from_id: String,
    pub to_id: String,
    pub props: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
pub struct KnowledgeSnapshot {
    pub exported_at: String,
    pub schema_version: u32,
    pub notes: Vec<NoteRecord>,
    pub tasks: Vec<TaskRecord>,
    pub entities: Vec<EntityRecord>,
    pub procedures: Vec<ProcedureRecord>,
    pub relationships: Vec<RelationshipRecord>,
}

/// Lightweight metadata about a snapshot file (for listing).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SnapshotMeta {
    pub file_name: String,
    pub file_path: String,
    pub exported_at: String,
    pub schema_version: u32,
    pub note_count: usize,
    pub task_count: usize,
    pub entity_count: usize,
    pub procedure_count: usize,
    pub relationship_count: usize,
    pub size_bytes: u64,
}

/// Stats returned by `restore_snapshot`.
#[derive(Debug, Serialize)]
pub struct RestoreStats {
    pub notes_restored: usize,
    pub tasks_restored: usize,
    pub entities_restored: usize,
    pub procedures_restored: usize,
    pub relationships_restored: usize,
    pub dry_run: bool,
}

// ─── Service ──────────────────────────────────────────────────────────────────

pub struct SnapshotService {
    neo4j: Neo4jClient,
    snapshot_dir: PathBuf,
}

impl SnapshotService {
    pub fn new(neo4j: Neo4jClient, snapshot_dir: PathBuf) -> Self {
        Self {
            neo4j,
            snapshot_dir,
        }
    }

    /// Take a snapshot of the current knowledge graph.
    ///
    /// Returns the path to the created file and its metadata.
    pub async fn take_snapshot(&self, label: Option<&str>) -> Result<(PathBuf, SnapshotMeta)> {
        tokio::fs::create_dir_all(&self.snapshot_dir)
            .await
            .context("Failed to create snapshot directory")?;

        let now = Utc::now();
        let ts = now.format("%Y%m%d_%H%M%S").to_string();
        let file_name = match label {
            Some(l) => format!("snapshot_{}_{}.json.gz", l, ts),
            None => format!("snapshot_{}.json.gz", ts),
        };
        let file_path = self.snapshot_dir.join(&file_name);

        // Fetch all node types in parallel.
        let (notes_res, tasks_res, entities_res, procedures_res, rels_res) = tokio::join!(
            self.fetch_notes(),
            self.fetch_tasks(),
            self.fetch_entities(),
            self.fetch_procedures(),
            self.fetch_relationships(),
        );

        let notes = notes_res.context("Failed to fetch notes")?;
        let tasks = tasks_res.context("Failed to fetch tasks")?;
        let entities = entities_res.context("Failed to fetch entities")?;
        let procedures = procedures_res.context("Failed to fetch procedures")?;
        let relationships = rels_res.context("Failed to fetch relationships")?;

        let snapshot = KnowledgeSnapshot {
            exported_at: now.to_rfc3339(),
            schema_version: SCHEMA_VERSION,
            notes,
            tasks,
            entities,
            procedures,
            relationships,
        };

        let json_bytes = serde_json::to_vec(&snapshot).context("Failed to serialize snapshot")?;

        // Gzip compress and write.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&json_bytes)
            .context("Failed to compress snapshot")?;
        let compressed = encoder.finish().context("Failed to finalize gzip stream")?;

        let size_bytes = compressed.len() as u64;
        tokio::fs::write(&file_path, &compressed)
            .await
            .context("Failed to write snapshot file")?;

        let meta = SnapshotMeta {
            file_name: file_name.clone(),
            file_path: file_path.to_string_lossy().to_string(),
            exported_at: snapshot.exported_at.clone(),
            schema_version: snapshot.schema_version,
            note_count: snapshot.notes.len(),
            task_count: snapshot.tasks.len(),
            entity_count: snapshot.entities.len(),
            procedure_count: snapshot.procedures.len(),
            relationship_count: snapshot.relationships.len(),
            size_bytes,
        };

        info!(
            file = %file_name,
            notes = meta.note_count,
            tasks = meta.task_count,
            entities = meta.entity_count,
            procedures = meta.procedure_count,
            relationships = meta.relationship_count,
            size_bytes,
            "Knowledge snapshot taken"
        );

        Ok((file_path, meta))
    }

    /// Restore a knowledge graph snapshot from a file.
    ///
    /// Uses MERGE semantics — safe to run on a non-empty graph.
    /// When `dry_run` is true, returns counts without writing anything.
    pub async fn restore_snapshot(&self, path: &Path, dry_run: bool) -> Result<RestoreStats> {
        let compressed = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read snapshot file: {}", path.display()))?;

        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut json_bytes = Vec::new();
        decoder
            .read_to_end(&mut json_bytes)
            .context("Failed to decompress snapshot")?;

        let snapshot: KnowledgeSnapshot =
            serde_json::from_slice(&json_bytes).context("Failed to deserialize snapshot")?;

        if dry_run {
            return Ok(RestoreStats {
                notes_restored: snapshot.notes.len(),
                tasks_restored: snapshot.tasks.len(),
                entities_restored: snapshot.entities.len(),
                procedures_restored: snapshot.procedures.len(),
                relationships_restored: snapshot.relationships.len(),
                dry_run: true,
            });
        }

        // Restore nodes.
        let notes_restored = self.restore_notes(&snapshot.notes).await?;
        let tasks_restored = self.restore_tasks(&snapshot.tasks).await?;
        let entities_restored = self.restore_entities(&snapshot.entities).await?;
        let procedures_restored = self.restore_procedures(&snapshot.procedures).await?;
        let relationships_restored = self.restore_relationships(&snapshot.relationships).await?;

        info!(
            notes_restored,
            tasks_restored,
            entities_restored,
            procedures_restored,
            relationships_restored,
            "Knowledge snapshot restored"
        );

        Ok(RestoreStats {
            notes_restored,
            tasks_restored,
            entities_restored,
            procedures_restored,
            relationships_restored,
            dry_run: false,
        })
    }

    /// List all snapshot files in the snapshot directory, sorted newest-first.
    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>> {
        let mut dir = match tokio::fs::read_dir(&self.snapshot_dir).await {
            Ok(d) => d,
            Err(_) => return Ok(Vec::new()), // dir doesn't exist yet
        };

        let mut metas: Vec<SnapshotMeta> = Vec::new();

        while let Some(entry) = dir
            .next_entry()
            .await
            .context("Failed to read snapshot dir")?
        {
            let path = entry.path();
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            if !file_name.ends_with(".json.gz") {
                continue;
            }

            let size_bytes = entry.metadata().await.map(|m| m.len()).unwrap_or(0);

            // Read and decompress just enough to parse the header fields.
            match self.read_snapshot_meta(&path, &file_name, size_bytes).await {
                Ok(meta) => metas.push(meta),
                Err(e) => warn!("Skipping unreadable snapshot {}: {}", file_name, e),
            }
        }

        // Sort newest-first by exported_at string (RFC 3339 sorts lexicographically).
        metas.sort_by(|a, b| b.exported_at.cmp(&a.exported_at));

        Ok(metas)
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    async fn read_snapshot_meta(
        &self,
        path: &Path,
        file_name: &str,
        size_bytes: u64,
    ) -> Result<SnapshotMeta> {
        let compressed = tokio::fs::read(path).await?;
        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut json_bytes = Vec::new();
        decoder.read_to_end(&mut json_bytes)?;
        let snapshot: KnowledgeSnapshot = serde_json::from_slice(&json_bytes)?;

        Ok(SnapshotMeta {
            file_name: file_name.to_string(),
            file_path: path.to_string_lossy().to_string(),
            exported_at: snapshot.exported_at,
            schema_version: snapshot.schema_version,
            note_count: snapshot.notes.len(),
            task_count: snapshot.tasks.len(),
            entity_count: snapshot.entities.len(),
            procedure_count: snapshot.procedures.len(),
            relationship_count: snapshot.relationships.len(),
            size_bytes,
        })
    }

    // ─── Fetch helpers ────────────────────────────────────────────────────────

    async fn fetch_notes(&self) -> Result<Vec<NoteRecord>> {
        let q = neo4rs::query(
            "MATCH (n:Note) RETURN \
             n.id AS id, n.content AS content, n.note_type AS note_type, \
             toString(n.created_at) AS created_at, toString(n.last_accessed_at) AS last_accessed_at, \
             coalesce(n.access_count, 0) AS access_count, \
             n.source_context AS source_context, toString(n.event_at) AS event_at, \
             toString(n.next_review_at) AS next_review_at, \
             coalesce(n.review_interval_days, 1) AS review_interval_days",
        );
        let rows = self.neo4j.execute(q).await?;
        let mut notes = Vec::new();
        for row in rows {
            let id: String = row.get("id").unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            notes.push(NoteRecord {
                id,
                content: row.get("content").unwrap_or_default(),
                note_type: row
                    .get("note_type")
                    .unwrap_or_else(|_| "semantic".to_string()),
                created_at: row.get("created_at").unwrap_or_default(),
                last_accessed_at: row.get("last_accessed_at").unwrap_or_default(),
                access_count: row.get::<i64>("access_count").unwrap_or(0),
                source_context: row.get("source_context").ok(),
                event_at: row.get("event_at").ok(),
                next_review_at: row.get("next_review_at").unwrap_or_default(),
                review_interval_days: row.get::<i64>("review_interval_days").unwrap_or(1),
            });
        }
        Ok(notes)
    }

    async fn fetch_tasks(&self) -> Result<Vec<TaskRecord>> {
        let q = neo4rs::query(
            "MATCH (t:Task) RETURN \
             t.id AS id, t.goal AS goal, t.context AS context, \
             t.status AS status, toString(t.created_at) AS created_at",
        );
        let rows = self.neo4j.execute(q).await?;
        let mut tasks = Vec::new();
        for row in rows {
            let id: String = row.get("id").unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            tasks.push(TaskRecord {
                id,
                goal: row.get("goal").unwrap_or_default(),
                context: row.get("context").ok(),
                status: row.get("status").unwrap_or_else(|_| "created".to_string()),
                created_at: row.get("created_at").unwrap_or_default(),
            });
        }
        Ok(tasks)
    }

    async fn fetch_entities(&self) -> Result<Vec<EntityRecord>> {
        let q = neo4rs::query(
            "MATCH (e:Entity) RETURN \
             e.id AS id, e.name AS name, e.entity_type AS entity_type, \
             toString(e.created_at) AS created_at",
        );
        let rows = self.neo4j.execute(q).await?;
        let mut entities = Vec::new();
        for row in rows {
            let id: String = row.get("id").unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            entities.push(EntityRecord {
                id,
                name: row.get("name").unwrap_or_default(),
                entity_type: row
                    .get("entity_type")
                    .unwrap_or_else(|_| "unknown".to_string()),
                created_at: row.get("created_at").unwrap_or_default(),
            });
        }
        Ok(entities)
    }

    async fn fetch_procedures(&self) -> Result<Vec<ProcedureRecord>> {
        let q = neo4rs::query(
            "MATCH (p:Procedure) RETURN \
             p.id AS id, p.name AS name, p.description AS description, \
             p.steps AS steps_json, toString(p.created_at) AS created_at",
        );
        let rows = self.neo4j.execute(q).await?;
        let mut procedures = Vec::new();
        for row in rows {
            let id: String = row.get("id").unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            procedures.push(ProcedureRecord {
                id,
                name: row.get("name").unwrap_or_default(),
                description: row.get("description").unwrap_or_default(),
                steps_json: row.get("steps_json").unwrap_or_else(|_| "[]".to_string()),
                created_at: row.get("created_at").unwrap_or_default(),
            });
        }
        Ok(procedures)
    }

    async fn fetch_relationships(&self) -> Result<Vec<RelationshipRecord>> {
        let q = neo4rs::query(
            "MATCH (a)-[r]->(b) \
             WHERE (a:Note OR a:Task OR a:Entity OR a:Procedure) \
               AND (b:Note OR b:Task OR b:Entity OR b:Procedure) \
               AND a.id IS NOT NULL AND b.id IS NOT NULL \
             RETURN type(r) AS rel_type, a.id AS from_id, b.id AS to_id, properties(r) AS props",
        );
        let rows = self.neo4j.execute(q).await?;
        let mut rels = Vec::new();
        for row in rows {
            let from_id: String = row.get("from_id").unwrap_or_default();
            let to_id: String = row.get("to_id").unwrap_or_default();
            if from_id.is_empty() || to_id.is_empty() {
                continue;
            }
            let props: serde_json::Value = row
                .get("props")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            rels.push(RelationshipRecord {
                rel_type: row.get("rel_type").unwrap_or_default(),
                from_id,
                to_id,
                props,
            });
        }
        Ok(rels)
    }

    // ─── Restore helpers ──────────────────────────────────────────────────────

    async fn restore_notes(&self, notes: &[NoteRecord]) -> Result<usize> {
        let mut count = 0;
        for note in notes {
            let q = neo4rs::query(
                "MERGE (n:Note {id: $id}) \
                 ON CREATE SET n.content = $content, n.note_type = $note_type, \
                   n.access_count = $access_count, n.source_context = $source_context, \
                   n.review_interval_days = $review_interval_days",
            )
            .param("id", note.id.clone())
            .param("content", note.content.clone())
            .param("note_type", note.note_type.clone())
            .param("access_count", note.access_count)
            .param(
                "source_context",
                note.source_context.clone().unwrap_or_default(),
            )
            .param("review_interval_days", note.review_interval_days);

            if self.neo4j.run(q).await.is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn restore_tasks(&self, tasks: &[TaskRecord]) -> Result<usize> {
        let mut count = 0;
        for task in tasks {
            let q = neo4rs::query(
                "MERGE (t:Task {id: $id}) \
                 ON CREATE SET t.goal = $goal, t.context = $context, t.status = $status",
            )
            .param("id", task.id.clone())
            .param("goal", task.goal.clone())
            .param("context", task.context.clone().unwrap_or_default())
            .param("status", task.status.clone());

            if self.neo4j.run(q).await.is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn restore_entities(&self, entities: &[EntityRecord]) -> Result<usize> {
        let mut count = 0;
        for entity in entities {
            let q = neo4rs::query(
                "MERGE (e:Entity {name: $name}) \
                 ON CREATE SET e.id = $id, e.entity_type = $entity_type",
            )
            .param("name", entity.name.clone())
            .param("id", entity.id.clone())
            .param("entity_type", entity.entity_type.clone());

            if self.neo4j.run(q).await.is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn restore_procedures(&self, procedures: &[ProcedureRecord]) -> Result<usize> {
        let mut count = 0;
        for proc in procedures {
            let q = neo4rs::query(
                "MERGE (p:Procedure {id: $id}) \
                 ON CREATE SET p.name = $name, p.description = $description, p.steps = $steps_json",
            )
            .param("id", proc.id.clone())
            .param("name", proc.name.clone())
            .param("description", proc.description.clone())
            .param("steps_json", proc.steps_json.clone());

            if self.neo4j.run(q).await.is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn restore_relationships(&self, rels: &[RelationshipRecord]) -> Result<usize> {
        let mut count = 0;
        for rel in rels {
            // Dynamic relationship type requires format string.
            let cypher = format!(
                "MATCH (a {{id: $from_id}}), (b {{id: $to_id}}) \
                 MERGE (a)-[r:{}]->(b)",
                rel.rel_type
            );
            let q = neo4rs::query(&cypher)
                .param("from_id", rel.from_id.clone())
                .param("to_id", rel.to_id.clone());

            if self.neo4j.run(q).await.is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }
}
