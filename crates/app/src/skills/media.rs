//! Media Skill — watch/summarize videos and manage the channel watchlist.
//!
//! Tools:
//! - `ingest_media` — fetch transcript, map-reduce summarize, upsert the
//!   `:Media` dedup node, and (optionally) store a linked summary note.
//! - `fetch_transcript` — raw transcript text only (Q&A / RAG grounding).
//! - `list_channel_videos` — recent un-ingested items from a channel/playlist.
//! - `poll_media_sources` — autonomous watch: fan out new videos into
//!   `"watch video: <url>"` Tasks (gated by `MEDIA_WATCH_ENABLED`).
//! - `manage_media_source` — runtime CRUD for the `:MediaSource` watchlist.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::{MediaRecord, Neo4jClient, TelemetryClient};
use crate::services::MediaService;
use crate::services::traits::{KnowledgeStore, LlmProvider};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

pub struct MediaSkill {
    svc: Arc<MediaService>,
    llm: Arc<dyn LlmProvider>,
    neo4j: Neo4jClient,
    /// Used only by the `store: true` path (direct calls); chains store via
    /// their own `store_note` step.
    knowledge: Option<Arc<dyn KnowledgeStore>>,
    telemetry: Option<TelemetryClient>,
}

impl MediaSkill {
    pub fn new(
        svc: Arc<MediaService>,
        llm: Arc<dyn LlmProvider>,
        neo4j: Neo4jClient,
        knowledge: Option<Arc<dyn KnowledgeStore>>,
        telemetry: Option<TelemetryClient>,
    ) -> Self {
        Self {
            svc,
            llm,
            neo4j,
            knowledge,
            telemetry,
        }
    }

    // ========================================================================
    // Tool definitions
    // ========================================================================

    fn ingest_media_def() -> ToolDefinition {
        ToolDefinition {
            name: "ingest_media".to_string(),
            description:
                "Watch a video: fetch its transcript (captions), summarize it (map-reduce), \
                 record a :Media dedup node, and return a structured summary with key concepts. \
                 `url` may be a bare URL or a goal like 'watch video: <url>'. Set store=true to \
                 also persist a linked summary note (default false — chains store separately)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Video URL, or text containing one"},
                    "source_context": {"type": "string", "description": "Optional source_context tag for the stored note"},
                    "store": {"type": "boolean", "description": "Also store a linked summary note (default false)"},
                    "force": {"type": "boolean", "description": "Re-summarize even if already ingested (default false)"},
                    "source_media_name": {"type": "string", "description": "Internal: name of the MediaSource that surfaced this video"}
                },
                "required": ["url"]
            }),
        }
    }

    fn fetch_transcript_def() -> ToolDefinition {
        ToolDefinition {
            name: "fetch_transcript".to_string(),
            description:
                "Fetch the raw transcript text of a video (captions only, no summarization). \
                 Useful for direct Q&A or grounding."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Video URL, or text containing one"}
                },
                "required": ["url"]
            }),
        }
    }

    fn list_channel_videos_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_channel_videos".to_string(),
            description:
                "List recent, not-yet-ingested videos from a YouTube channel or playlist via its \
                 free RSS feed. Pass `source` (a MediaSource name, or a channel_id/playlist_id) or \
                 explicit `kind`+`ref`."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "MediaSource name, or a channel_id/playlist_id"},
                    "kind": {"type": "string", "enum": ["youtube_channel", "youtube_playlist"], "description": "Explicit source kind"},
                    "ref": {"type": "string", "description": "Explicit channel_id / playlist_id"},
                    "limit": {"type": "integer", "description": "Max items to return (default 15)"}
                }
            }),
        }
    }

    fn poll_media_sources_def() -> ToolDefinition {
        ToolDefinition {
            name: "poll_media_sources".to_string(),
            description:
                "Autonomous watch: check every active :MediaSource for new videos and create a \
                 'watch video: <url>' Task for each. No-op unless MEDIA_WATCH_ENABLED is set."
                    .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn manage_media_source_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_media_source".to_string(),
            description:
                "Manage the :MediaSource watchlist. action=list|upsert|delete. Upsert marks the \
                 node runtime-owned (managed_by=runtime). Reserved names are graph-owned after seed."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "upsert", "delete"], "description": "Operation (default list)"},
                    "name": {"type": "string", "description": "Watchlist entry name"},
                    "kind": {"type": "string", "enum": ["youtube_channel", "youtube_playlist", "podcast_rss"]},
                    "ref": {"type": "string", "description": "channel_id / playlist_id / rss_url"},
                    "description": {"type": "string"},
                    "active": {"type": "boolean", "description": "Whether the watch loop polls it (default true)"}
                }
            }),
        }
    }

    // ========================================================================
    // Handlers
    // ========================================================================

    async fn handle_ingest_media(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: IngestInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let url = MediaService::extract_first_url(&input.url)
            .unwrap_or_else(|| input.url.trim().to_string());

        let (meta, transcript, source) = match self.svc.fetch_transcript(&url).await {
            Ok(t) => t,
            Err(e) => {
                if let Some(ref t) = self.telemetry {
                    let _ =
                        t.log_knowledge_gap(&url, Some("ingest_media"), "transcript_unavailable");
                }
                return ToolCallResult::error(format!("Could not fetch transcript: {e}"));
            }
        };

        let max = self.svc.max_duration_secs();
        if max > 0 && meta.duration_secs > max {
            return ToolCallResult::error(format!(
                "Video is {}s long, exceeding MEDIA_MAX_DURATION_SECS={}",
                meta.duration_secs, max
            ));
        }

        // Dedup: skip the expensive summarize if already ingested.
        if !input.force.unwrap_or(false) && self.neo4j.media_exists(&meta.id).await.unwrap_or(false)
        {
            return ToolCallResult::success_json(json!({
                "already_ingested": true,
                "video_id": meta.id,
                "title": meta.title,
                "url": meta.url,
                "message": "Already ingested; pass force=true to re-summarize."
            }));
        }

        let (summary, key_concepts) = match self.svc.summarize(&meta, &transcript, &self.llm).await
        {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Summarization failed: {e}")),
        };

        // Record the dedup/provenance node.
        let record = MediaRecord {
            id: meta.id.clone(),
            url: meta.url.clone(),
            title: meta.title.clone(),
            channel: meta.channel.clone(),
            channel_id: meta.channel_id.clone(),
            published_at: meta.published_at.clone(),
            duration_secs: meta.duration_secs,
            transcript_source: source.clone(),
            source_media_name: input.source_media_name.clone().unwrap_or_default(),
        };
        if let Err(e) = self.neo4j.upsert_media(&record).await {
            warn!(error = %e, "ingest_media: failed to upsert :Media node");
        }
        if let Some(ref sm) = input.source_media_name {
            let _ = self.neo4j.link_media_from_source(&meta.id, sm).await;
        }

        let mut out = json!({
            "video_id": meta.id,
            "title": meta.title,
            "channel": meta.channel,
            "channel_id": meta.channel_id,
            "url": meta.url,
            "published_at": meta.published_at,
            "duration_secs": meta.duration_secs,
            "transcript_source": source,
            "transcript_len": transcript.len(),
            "summary": summary,
            "key_concepts": key_concepts,
        });

        // Optional direct-store path (chains use their own store_note step).
        if input.store.unwrap_or(false)
            && let Some(ref ks) = self.knowledge
        {
            let concept_lines = key_concepts
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n");
            let content = format!(
                "# {}\n\nSource: {} ({})\nURL: {}\n\n{}\n\n## Key concepts\n{}",
                meta.title, meta.channel, meta.published_at, meta.url, summary, concept_lines
            );
            let sc = input.source_context.as_deref().unwrap_or("video_learning");
            match ks
                .store_note(&content, Some("semantic"), Some(sc), None, None)
                .await
            {
                Ok((note_id, _)) => {
                    let _ = self
                        .neo4j
                        .link_note_summarizes_media(&note_id, &meta.id)
                        .await;
                    out["note_id"] = json!(note_id);
                    out["stored"] = json!(true);
                }
                Err(e) => warn!(error = %e, "ingest_media: store_note failed"),
            }
        }

        ToolCallResult::success_json(out)
    }

    async fn handle_fetch_transcript(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: UrlInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let url = MediaService::extract_first_url(&input.url)
            .unwrap_or_else(|| input.url.trim().to_string());
        match self.svc.fetch_transcript(&url).await {
            Ok((meta, transcript, source)) => ToolCallResult::success_json(json!({
                "video_id": meta.id,
                "title": meta.title,
                "url": meta.url,
                "transcript_source": source,
                "length": transcript.len(),
                "transcript": transcript,
            })),
            Err(e) => ToolCallResult::error(format!("Could not fetch transcript: {e}")),
        }
    }

    async fn handle_list_channel_videos(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ListInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };

        let (kind, reference) = match self.resolve_source(&input).await {
            Ok(kr) => kr,
            Err(msg) => return ToolCallResult::error(msg),
        };

        let items = match self.svc.list_feed_videos(&kind, &reference).await {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(format!("Feed listing failed: {e}")),
        };

        let limit = input.limit.unwrap_or(15);
        let mut fresh = Vec::new();
        for item in items {
            if self
                .neo4j
                .media_exists(&item.video_id)
                .await
                .unwrap_or(false)
            {
                continue;
            }
            fresh.push(item);
            if fresh.len() >= limit {
                break;
            }
        }
        ToolCallResult::success_json(fresh)
    }

    async fn handle_poll_media_sources(&self) -> ToolCallResult {
        if !watch_enabled() {
            return ToolCallResult::success_json(json!({
                "enabled": false,
                "message": "Media watch disabled (set MEDIA_WATCH_ENABLED=true to enable)."
            }));
        }
        let sources = match self.neo4j.list_media_sources(true).await {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Could not list media sources: {e}")),
        };

        let mut polled = 0usize;
        let mut created = 0usize;
        let mut goals = Vec::new();
        for ms in sources {
            polled += 1;
            let items = match self.svc.list_feed_videos(&ms.kind, &ms.reference).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(source = %ms.name, error = %e, "poll_media_sources: feed failed");
                    continue;
                }
            };
            for item in items {
                let goal = format!("watch video: {}", item.url);
                if self
                    .neo4j
                    .media_exists(&item.video_id)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                if self
                    .neo4j
                    .open_task_exists_for_goal(&goal)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                let ctx = format!(
                    "Auto-discovered from MediaSource '{}' ({}). Title: {}. Published: {}.",
                    ms.name, ms.kind, item.title, item.published_at
                );
                match self.neo4j.create_task(&goal, Some(&ctx), None).await {
                    Ok(_) => {
                        created += 1;
                        goals.push(goal);
                    }
                    Err(e) => warn!(error = %e, "poll_media_sources: create_task failed"),
                }
            }
        }
        info!(polled, created, "poll_media_sources complete");
        ToolCallResult::success_json(json!({
            "enabled": true,
            "sources_polled": polled,
            "tasks_created": created,
            "goals": goals,
        }))
    }

    async fn handle_manage_media_source(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ManageInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let action = input.action.as_deref().unwrap_or("list");
        match action {
            "list" => match self.neo4j.list_media_sources(false).await {
                Ok(sources) => {
                    let arr: Vec<Value> = sources
                        .iter()
                        .map(|s| {
                            json!({
                                "name": s.name,
                                "kind": s.kind,
                                "ref": s.reference,
                                "description": s.description,
                                "active": s.active,
                                "managed_by": s.managed_by,
                            })
                        })
                        .collect();
                    ToolCallResult::success_json(arr)
                }
                Err(e) => ToolCallResult::error(format!("list failed: {e}")),
            },
            "delete" => {
                let Some(name) = input.name.as_deref() else {
                    return ToolCallResult::error("delete requires 'name'");
                };
                match self.neo4j.delete_media_source(name).await {
                    Ok(()) => ToolCallResult::success_json(json!({"deleted": name})),
                    Err(e) => ToolCallResult::error(format!("delete failed: {e}")),
                }
            }
            "upsert" => {
                let (Some(name), Some(kind), Some(reference)) = (
                    input.name.as_deref(),
                    input.kind.as_deref(),
                    input.reference.as_deref(),
                ) else {
                    return ToolCallResult::error("upsert requires 'name', 'kind', and 'ref'");
                };
                let description = input.description.as_deref().unwrap_or_default();
                let active = input.active.unwrap_or(true);
                match self
                    .neo4j
                    .upsert_media_source_runtime(name, kind, reference, description, active)
                    .await
                {
                    Ok(()) => ToolCallResult::success_json(json!({
                        "upserted": name, "kind": kind, "ref": reference,
                        "active": active, "managed_by": "runtime"
                    })),
                    Err(e) => ToolCallResult::error(format!("upsert failed: {e}")),
                }
            }
            other => ToolCallResult::error(format!("unknown action '{other}'")),
        }
    }

    /// Resolve `(kind, ref)` from a MediaSource name, explicit kind+ref, or a
    /// bare channel/playlist id (inferred from its prefix).
    async fn resolve_source(&self, input: &ListInput) -> Result<(String, String), String> {
        if let (Some(kind), Some(reference)) = (&input.kind, &input.reference) {
            return Ok((kind.clone(), reference.clone()));
        }
        let Some(source) = &input.source else {
            return Err("provide 'source' (a MediaSource name or channel/playlist id) or explicit 'kind'+'ref'".into());
        };
        if let Ok(Some(ms)) = self.neo4j.get_media_source(source).await {
            return Ok((ms.kind, ms.reference));
        }
        match infer_kind(source) {
            Some(kind) => Ok((kind.to_string(), source.clone())),
            None => Err(format!(
                "could not resolve '{source}' — not a known MediaSource name and not a \
                 recognizable channel/playlist id"
            )),
        }
    }
}

fn watch_enabled() -> bool {
    matches!(
        std::env::var("MEDIA_WATCH_ENABLED").as_deref(),
        Ok("true") | Ok("1") | Ok("yes")
    )
}

/// Infer a YouTube source kind from a bare id prefix.
fn infer_kind(reference: &str) -> Option<&'static str> {
    if reference.starts_with("UC") {
        Some("youtube_channel")
    } else if ["PL", "UU", "OL", "FL", "RD"]
        .iter()
        .any(|p| reference.starts_with(p))
    {
        Some("youtube_playlist")
    } else {
        None
    }
}

#[async_trait]
impl Skill for MediaSkill {
    fn name(&self) -> &str {
        "Media Learning"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::ingest_media_def(),
            Self::fetch_transcript_def(),
            Self::list_channel_videos_def(),
            Self::poll_media_sources_def(),
            Self::manage_media_source_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "ingest_media" => Some(self.handle_ingest_media(arguments).await),
            "fetch_transcript" => Some(self.handle_fetch_transcript(arguments).await),
            "list_channel_videos" => Some(self.handle_list_channel_videos(arguments).await),
            "poll_media_sources" => Some(self.handle_poll_media_sources().await),
            "manage_media_source" => Some(self.handle_manage_media_source(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input types
// ============================================================================

#[derive(Debug, Deserialize)]
struct IngestInput {
    url: String,
    #[serde(default)]
    source_context: Option<String>,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    force: Option<bool>,
    #[serde(default)]
    source_media_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UrlInput {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ListInput {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ManageInput {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    active: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_channel_and_playlist_kinds() {
        assert_eq!(infer_kind("UCabcdef"), Some("youtube_channel"));
        assert_eq!(infer_kind("PLabcdef"), Some("youtube_playlist"));
        assert_eq!(infer_kind("UUabcdef"), Some("youtube_playlist"));
        assert_eq!(infer_kind("randomthing"), None);
    }
}
