//! Media graph layer — `:Media` (dedup + provenance ledger) and
//! `:MediaSource` (the channel/feed watchlist), over the host's graph.
//!
//! `:Media.id` is the platform video/episode id and the dedup key: it is
//! checked before every ingest. `:MediaSource` mirrors the `:SourceList`
//! ownership model — seeded ON CREATE from `sources-media/*.yaml`
//! (`managed_by = 'yaml'`), the graph owns it afterwards, and runtime edits
//! set `managed_by = 'runtime'`. Moved from the core's repository crate in
//! CORE_AND_PLUGINS_PLAN Phase 4.

use std::sync::Arc;

use agent_brain_plugin_api::{Graph, Value};
use anyhow::Result;
use chrono::Utc;
use serde_json::json;

/// Constraints and indexes the pack needs (run by `Plugin::start`).
pub const SCHEMA: &[&str] = &[
    "CREATE CONSTRAINT media_source_name IF NOT EXISTS FOR (m:MediaSource) REQUIRE m.name IS UNIQUE",
    "CREATE CONSTRAINT media_id IF NOT EXISTS FOR (m:Media) REQUIRE m.id IS UNIQUE",
    // poll_media_sources scans active watchlist entries every tick.
    "CREATE INDEX media_source_active IF NOT EXISTS FOR (m:MediaSource) ON (m.active)",
    "CREATE INDEX media_channel_idx IF NOT EXISTS FOR (m:Media) ON (m.channel_id)",
    "CREATE INDEX media_ingested_idx IF NOT EXISTS FOR (m:Media) ON (m.ingested_at)",
];

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
    /// `youtube_channel` | `youtube_playlist` | `podcast_rss` | `article_rss`
    pub kind: String,
    /// channel_id / playlist_id / rss_url (stored as the `ref` property).
    pub reference: String,
    pub description: String,
    pub active: bool,
    /// `yaml` (owned by sources-media/*.yaml) | `runtime` (created via tool/API).
    pub managed_by: String,
}

#[derive(Clone)]
pub struct Store {
    graph: Arc<dyn Graph>,
}

fn count(rows: &[Value]) -> i64 {
    rows.first().and_then(|r| r["c"].as_i64()).unwrap_or(0)
}

impl Store {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        Self { graph }
    }

    async fn q(&self, cypher: &str, params: Value) -> Result<Vec<Value>> {
        self.graph.query(cypher, params).await
    }

    // ========================================================================
    // :Media (dedup ledger)
    // ========================================================================

    /// True if a `:Media` node with this id already exists (dedup gate).
    pub async fn media_exists(&self, id: &str) -> Result<bool> {
        let rows = self
            .q(
                "MATCH (m:Media {id: $id}) RETURN count(m) AS c",
                json!({ "id": id }),
            )
            .await?;
        Ok(count(&rows) > 0)
    }

    /// What the brain wrote the last time it read this item: the newest
    /// video- or article-learning note naming `needle` (a video id, an
    /// article URL), so a repeat ingest can hand the run real content instead
    /// of only "already ingested" (a chat hand-off of something already read).
    /// `None` when no such note exists.
    pub async fn prior_summary(&self, needle: &str) -> Result<Option<(String, String)>> {
        let rows = self
            .q(
                "MATCH (n:Note {note_type: 'source_record'}) \
                 WHERE (n.content STARTS WITH '# Video learning' OR n.content STARTS WITH '# Article') \
                   AND n.content CONTAINS $needle \
                 RETURN n.id AS id, n.content AS content ORDER BY n.created_at DESC LIMIT 1",
                json!({ "needle": needle }),
            )
            .await?;
        Ok(rows.first().and_then(|r| {
            Some((
                r.get("id")?.as_str()?.to_string(),
                r.get("content")?.as_str()?.to_string(),
            ))
        }))
    }

    /// Upsert a `:Media` node (MERGE on id). `ingested_at` is stamped ON CREATE
    /// only so re-ingest of the same id preserves the original timestamp.
    pub async fn upsert_media(&self, m: &MediaRecord) -> Result<()> {
        self.q(
            "MERGE (m:Media {id: $id}) \
             ON CREATE SET m.ingested_at = datetime($now) \
             SET m.url = $url, m.title = $title, m.channel = $channel, \
                 m.channel_id = $channel_id, \
                 m.published_at = CASE WHEN $published_at = '' THEN null \
                                      ELSE datetime($published_at) END, \
                 m.duration_secs = $duration_secs, m.transcript_source = $transcript_source, \
                 m.source_media_name = $source_media_name",
            json!({
                "id": m.id,
                "now": Utc::now().to_rfc3339(),
                "url": m.url,
                "title": m.title,
                "channel": m.channel,
                "channel_id": m.channel_id,
                "published_at": m.published_at,
                "duration_secs": m.duration_secs,
                "transcript_source": m.transcript_source,
                "source_media_name": m.source_media_name,
            }),
        )
        .await?;
        Ok(())
    }

    /// True if an *open* Task (`created`/`in_progress`) already targets this
    /// goal — used by `poll_media_sources` to avoid enqueuing the same video
    /// twice before its first ingest creates the `:Media` node.
    pub async fn open_task_exists_for_goal(&self, goal: &str) -> Result<bool> {
        let rows = self
            .q(
                "MATCH (t:Task) WHERE t.goal = $goal \
                 AND t.status IN ['created', 'in_progress'] RETURN count(t) AS c",
                json!({ "goal": goal }),
            )
            .await?;
        Ok(count(&rows) > 0)
    }

    /// The topics some active program works on, as `(name, description)`:
    /// what a gap question must be about to be worth a gap-fill run.
    pub async fn program_topics(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .q(
                "MATCH (p:Program) WHERE p.status = 'active' \
                 UNWIND coalesce(p.topics, []) AS t \
                 MATCH (tp:Topic {name: t}) \
                 RETURN DISTINCT tp.name AS name, coalesce(tp.description, '') AS d \
                 ORDER BY name",
                json!({}),
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some((
                    r["name"].as_str()?.to_string(),
                    r["d"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect())
    }

    /// How many Tasks whose goal starts with `prefix` were created since
    /// `since` (RFC 3339). Backs the curiosity engine's daily cap.
    pub async fn count_tasks_with_goal_prefix_since(
        &self,
        prefix: &str,
        since: &str,
    ) -> Result<i64> {
        let rows = self
            .q(
                "MATCH (t:Task) WHERE t.goal STARTS WITH $prefix \
                 AND t.created_at >= datetime($since) RETURN count(t) AS c",
                json!({ "prefix": prefix, "since": since }),
            )
            .await?;
        Ok(count(&rows))
    }

    /// Link a video to the watchlist source whose `ref` is its channel id, and
    /// record that source's name on the video. The watch task the poll creates
    /// carries only the URL, so the source name never reached `ingest_media` —
    /// no video had a `FROM_SOURCE` edge before 2026-10-01 (0 of 1,089). The
    /// channel id is on every video and every YouTube `MediaSource.ref`.
    pub async fn link_media_by_channel(&self, media_id: &str) -> Result<()> {
        self.q(
            "MATCH (m:Media {id: $media_id}) WHERE coalesce(m.channel_id, '') <> '' \
             MATCH (s:MediaSource {ref: m.channel_id}) \
             MERGE (m)-[:FROM_SOURCE]->(s) \
             SET m.source_media_name = s.name",
            json!({ "media_id": media_id }),
        )
        .await?;
        Ok(())
    }

    /// Create `(:Note)-[:SUMMARIZES]->(:Media)` (best-effort; both must exist).
    pub async fn link_note_summarizes_media(&self, note_id: &str, media_id: &str) -> Result<()> {
        self.q(
            "MATCH (n:Note {id: $note_id}), (m:Media {id: $media_id}) \
             MERGE (n)-[:SUMMARIZES]->(m)",
            json!({ "note_id": note_id, "media_id": media_id }),
        )
        .await?;
        Ok(())
    }

    /// Create `(:Media)-[:FROM_SOURCE]->(:MediaSource)` (best-effort).
    pub async fn link_media_from_source(&self, media_id: &str, source_name: &str) -> Result<()> {
        self.q(
            "MATCH (m:Media {id: $media_id}), (s:MediaSource {name: $source_name}) \
             MERGE (m)-[:FROM_SOURCE]->(s)",
            json!({ "media_id": media_id, "source_name": source_name }),
        )
        .await?;
        Ok(())
    }

    // ========================================================================
    // :MediaSource (watchlist)
    // ========================================================================

    /// Seed a `:MediaSource` if absent (MERGE on name, ON CREATE only) with
    /// `managed_by = 'yaml'` and the bundle it came from. Returns true when
    /// newly created. The graph owns the node afterwards, so runtime edits
    /// persist across restarts.
    pub async fn seed_media_source_if_absent(
        &self,
        name: &str,
        kind: &str,
        reference: &str,
        description: &str,
        active: bool,
        plugin: &str,
    ) -> Result<bool> {
        let rows = self
            .q(
                "MERGE (s:MediaSource {name: $name}) \
                 ON CREATE SET s.kind = $kind, s.ref = $ref, s.description = $description, \
                               s.active = $active, s.managed_by = 'yaml', s.plugin = $plugin, \
                               s.created_at = datetime($now), s.updated_at = datetime($now), \
                               s.__new = true \
                 WITH s, coalesce(s.__new, false) AS created REMOVE s.__new RETURN created",
                json!({
                    "name": name, "kind": kind, "ref": reference,
                    "description": description, "active": active, "plugin": plugin,
                    "now": Utc::now().to_rfc3339(),
                }),
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r["created"].as_bool())
            .unwrap_or(false))
    }

    /// Upsert a `:MediaSource` at runtime (via tool/API). Sets
    /// `managed_by = 'runtime'`, detaching it from any YAML file of the same name.
    pub async fn upsert_media_source_runtime(
        &self,
        name: &str,
        kind: &str,
        reference: &str,
        description: &str,
        active: bool,
    ) -> Result<()> {
        self.q(
            "MERGE (s:MediaSource {name: $name}) \
             ON CREATE SET s.created_at = datetime($now) \
             SET s.kind = $kind, s.ref = $ref, s.description = $description, \
                 s.active = $active, s.managed_by = 'runtime', s.updated_at = datetime($now)",
            json!({
                "name": name, "kind": kind, "ref": reference,
                "description": description, "active": active,
                "now": Utc::now().to_rfc3339(),
            }),
        )
        .await?;
        Ok(())
    }

    /// Delete a `:MediaSource` by name.
    pub async fn delete_media_source(&self, name: &str) -> Result<()> {
        self.q(
            "MATCH (s:MediaSource {name: $name}) DETACH DELETE s",
            json!({ "name": name }),
        )
        .await?;
        Ok(())
    }

    /// List watchlist entries, optionally only the active ones.
    pub async fn list_media_sources(&self, active_only: bool) -> Result<Vec<MediaSourceRecord>> {
        let filter = if active_only {
            "WHERE s.active = true "
        } else {
            ""
        };
        let rows = self
            .q(
                &format!(
                    "MATCH (s:MediaSource) {filter}\
                     RETURN s.name AS name, s.kind AS kind, s.ref AS ref, \
                            s.description AS description, s.active AS active, \
                            s.managed_by AS managed_by \
                     ORDER BY s.name"
                ),
                json!({}),
            )
            .await?;
        Ok(rows.iter().map(row_to_media_source).collect())
    }
}

fn row_to_media_source(row: &Value) -> MediaSourceRecord {
    let s = |k: &str| row[k].as_str().unwrap_or_default().to_string();
    MediaSourceRecord {
        name: s("name"),
        kind: s("kind"),
        reference: s("ref"),
        description: s("description"),
        active: row["active"].as_bool().unwrap_or(false),
        managed_by: row["managed_by"].as_str().unwrap_or("runtime").to_string(),
    }
}
