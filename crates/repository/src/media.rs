//! Media graph layer — `:Media` (dedup + provenance ledger) and
//! `:MediaSource` (the channel/feed watchlist) node CRUD.
//!
//! `:Media.id` is the platform video/episode id and the dedup key: it is
//! checked before every ingest and used to filter already-seen items out of
//! `list_channel_videos`. `:MediaSource` mirrors the `:SourceList` ownership
//! model — YAML seeds ON CREATE (`managed_by = 'yaml'`), the graph owns it
//! afterwards, and runtime edits set `managed_by = 'runtime'`.

use chrono::Utc;
use neo4rs::query;

use crate::client::Neo4jClient;
use crate::error::Result;

/// A row in the `:Media` dedup/provenance ledger.
#[derive(Debug, Clone)]
pub struct MediaRecord {
    pub id: String,
    pub url: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub published_at: String,
    pub duration_secs: i64,
    pub transcript_source: String,
    /// Name of the `:MediaSource` that surfaced this item (empty for ad-hoc).
    pub source_media_name: String,
}

/// A watchlist entry: a YouTube channel/playlist or a podcast RSS feed.
#[derive(Debug, Clone)]
pub struct MediaSourceRecord {
    pub name: String,
    /// `youtube_channel` | `youtube_playlist` | `podcast_rss`
    pub kind: String,
    /// channel_id / playlist_id / rss_url (stored as the `ref` property).
    pub reference: String,
    pub description: String,
    pub active: bool,
    /// `yaml` (owned by sources-media/*.yaml) | `runtime` (created via tool/API).
    pub managed_by: String,
}

impl Neo4jClient {
    // ========================================================================
    // :Media (dedup ledger)
    // ========================================================================

    /// True if a `:Media` node with this id already exists (dedup gate).
    pub async fn media_exists(&self, id: &str) -> Result<bool> {
        let rows = self
            .execute(query("MATCH (m:Media {id: $id}) RETURN count(m) AS c").param("id", id))
            .await?;
        let c = rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<i64>("c").ok())
            .unwrap_or(0);
        Ok(c > 0)
    }

    /// Upsert a `:Media` node (MERGE on id). `ingested_at` is stamped ON CREATE
    /// only so re-ingest of the same id preserves the original timestamp.
    pub async fn upsert_media(&self, m: &MediaRecord) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.run(
            query(
                "MERGE (m:Media {id: $id}) \
                 ON CREATE SET m.ingested_at = $now \
                 SET m.url = $url, m.title = $title, m.channel = $channel, \
                     m.channel_id = $channel_id, m.published_at = $published_at, \
                     m.duration_secs = $duration_secs, m.transcript_source = $transcript_source, \
                     m.source_media_name = $source_media_name",
            )
            .param("id", m.id.as_str())
            .param("now", now.as_str())
            .param("url", m.url.as_str())
            .param("title", m.title.as_str())
            .param("channel", m.channel.as_str())
            .param("channel_id", m.channel_id.as_str())
            .param("published_at", m.published_at.as_str())
            .param("duration_secs", m.duration_secs)
            .param("transcript_source", m.transcript_source.as_str())
            .param("source_media_name", m.source_media_name.as_str()),
        )
        .await
    }

    /// True if an *open* Task (`created`/`in_progress`) already targets this
    /// goal — used by `poll_media_sources` to avoid enqueuing the same video
    /// twice before its first ingest creates the `:Media` node.
    pub async fn open_task_exists_for_goal(&self, goal: &str) -> Result<bool> {
        let rows = self
            .execute(
                query(
                    "MATCH (t:Task) WHERE t.goal = $goal \
                     AND t.status IN ['created', 'in_progress'] RETURN count(t) AS c",
                )
                .param("goal", goal),
            )
            .await?;
        let c = rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<i64>("c").ok())
            .unwrap_or(0);
        Ok(c > 0)
    }

    /// Create `(:Note)-[:SUMMARIZES]->(:Media)` (best-effort; both must exist).
    pub async fn link_note_summarizes_media(&self, note_id: &str, media_id: &str) -> Result<()> {
        self.run(
            query(
                "MATCH (n:Note {id: $note_id}), (m:Media {id: $media_id}) \
                 MERGE (n)-[:SUMMARIZES]->(m)",
            )
            .param("note_id", note_id)
            .param("media_id", media_id),
        )
        .await
    }

    /// Create `(:Media)-[:FROM_SOURCE]->(:MediaSource)` (best-effort).
    pub async fn link_media_from_source(&self, media_id: &str, source_name: &str) -> Result<()> {
        self.run(
            query(
                "MATCH (m:Media {id: $media_id}), (s:MediaSource {name: $source_name}) \
                 MERGE (m)-[:FROM_SOURCE]->(s)",
            )
            .param("media_id", media_id)
            .param("source_name", source_name),
        )
        .await
    }

    // ========================================================================
    // :MediaSource (watchlist)
    // ========================================================================

    /// Seed a `:MediaSource` if absent (MERGE on name, ON CREATE only) with
    /// `managed_by = 'yaml'`. Returns true when newly created. The graph owns
    /// the node afterwards, so runtime edits persist across restarts.
    pub async fn seed_media_source_if_absent(
        &self,
        name: &str,
        kind: &str,
        reference: &str,
        description: &str,
        active: bool,
    ) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .execute(
                query(
                    "MERGE (s:MediaSource {name: $name}) \
                     ON CREATE SET s.kind = $kind, s.ref = $ref, s.description = $description, \
                                   s.active = $active, s.managed_by = 'yaml', \
                                   s.created_at = $now, s.updated_at = $now \
                     RETURN s.created_at = $now AS created",
                )
                .param("name", name)
                .param("kind", kind)
                .param("ref", reference)
                .param("description", description)
                .param("active", active)
                .param("now", now.as_str()),
            )
            .await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<bool>("created").ok())
            .unwrap_or(false))
    }

    /// Upsert a `:MediaSource` at runtime (via tool/API). Sets
    /// `managed_by = 'runtime'`, detaching it from any YAML file of the same name.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_media_source_runtime(
        &self,
        name: &str,
        kind: &str,
        reference: &str,
        description: &str,
        active: bool,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.run(
            query(
                "MERGE (s:MediaSource {name: $name}) \
                 ON CREATE SET s.created_at = $now \
                 SET s.kind = $kind, s.ref = $ref, s.description = $description, \
                     s.active = $active, s.managed_by = 'runtime', s.updated_at = $now",
            )
            .param("name", name)
            .param("kind", kind)
            .param("ref", reference)
            .param("description", description)
            .param("active", active)
            .param("now", now.as_str()),
        )
        .await
    }

    /// Delete a `:MediaSource` by name.
    pub async fn delete_media_source(&self, name: &str) -> Result<()> {
        self.run(query("MATCH (s:MediaSource {name: $name}) DETACH DELETE s").param("name", name))
            .await
    }

    /// List watchlist entries, optionally only the active ones.
    pub async fn list_media_sources(&self, active_only: bool) -> Result<Vec<MediaSourceRecord>> {
        let cypher = if active_only {
            "MATCH (s:MediaSource) WHERE s.active = true \
             RETURN s.name AS name, s.kind AS kind, s.ref AS ref, \
                    s.description AS description, s.active AS active, s.managed_by AS managed_by \
             ORDER BY s.name"
        } else {
            "MATCH (s:MediaSource) \
             RETURN s.name AS name, s.kind AS kind, s.ref AS ref, \
                    s.description AS description, s.active AS active, s.managed_by AS managed_by \
             ORDER BY s.name"
        };
        let rows = self.execute(query(cypher)).await?;
        Ok(rows.into_iter().map(row_to_media_source).collect())
    }

    /// Fetch a single `:MediaSource` by name.
    pub async fn get_media_source(&self, name: &str) -> Result<Option<MediaSourceRecord>> {
        let rows = self
            .execute(
                query(
                    "MATCH (s:MediaSource {name: $name}) \
                     RETURN s.name AS name, s.kind AS kind, s.ref AS ref, \
                            s.description AS description, s.active AS active, \
                            s.managed_by AS managed_by",
                )
                .param("name", name),
            )
            .await?;
        Ok(rows.into_iter().next().map(row_to_media_source))
    }
}

fn row_to_media_source(row: neo4rs::Row) -> MediaSourceRecord {
    MediaSourceRecord {
        name: row.get::<String>("name").unwrap_or_default(),
        kind: row.get::<String>("kind").unwrap_or_default(),
        reference: row.get::<String>("ref").unwrap_or_default(),
        description: row.get::<String>("description").unwrap_or_default(),
        active: row.get::<bool>("active").unwrap_or(false),
        managed_by: row
            .get::<String>("managed_by")
            .unwrap_or_else(|_| "runtime".into()),
    }
}
