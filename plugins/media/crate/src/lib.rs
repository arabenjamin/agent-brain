//! The `media` plugin: watch and read (CORE_AND_PLUGINS_PLAN Phase 4, pack 2).
//!
//! Videos, podcasts and articles become summaries with key concepts
//! (`ingest_media`, `ingest_article`: yt-dlp captions or Whisper, map-reduce
//! summarisation inside the tool); a watchlist of channels and feeds
//! (`:MediaSource`, `manage_media_source`, `poll_media_sources`,
//! `discover_media_sources`) feeds the `video-learning` / `article-learning`
//! workflows; `spawn_gap_tasks` turns their FOLLOW UP concepts into
//! `fill-knowledge-gap` runs.
//!
//! The pack owns the `sources-media/` seed kind: `start` seeds `:MediaSource`
//! from every loaded bundle's files, so another pack can add channels.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

mod service;
mod skill;
mod store;
mod transcribe;

pub struct Media;

/// A group of watched sources (`sources-media/*.yaml`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceGroupFile {
    name: String,
    #[serde(default)]
    description: String,
    sources: Vec<SourceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceEntry {
    #[serde(default)]
    name: Option<String>,
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "yes")]
    active: bool,
}

fn yes() -> bool {
    true
}

/// The `:MediaSource` kinds `poll_media_sources` knows how to read.
const SOURCE_KINDS: &[&str] = &[
    "youtube_channel",
    "youtube_playlist",
    "podcast_rss",
    "article_rss",
];

fn parse_group(text: &str) -> Result<SourceGroupFile, String> {
    let g: SourceGroupFile = serde_yaml::from_str(text).map_err(|e| format!("YAML: {e}"))?;
    if g.sources.is_empty() {
        return Err("a media source group needs at least one source".into());
    }
    if let Some(s) = g
        .sources
        .iter()
        .find(|s| !SOURCE_KINDS.contains(&s.kind.as_str()))
    {
        return Err(format!(
            "source kind '{}' is not one of {}",
            s.kind,
            SOURCE_KINDS.join(", ")
        ));
    }
    Ok(g)
}

#[async_trait]
impl Plugin for Media {
    fn name(&self) -> &'static str {
        "media"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        let svc = Arc::new(service::MediaService::from_env());
        match skill::MediaSkill::new(svc, ctx) {
            Some(s) => vec![Arc::new(s) as Arc<dyn Skill>],
            None => {
                warn!("media: the host gave no graph or LLM; no tools");
                vec![]
            }
        }
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        let graph = ctx
            .graph
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no graph"))?;
        for stmt in store::SCHEMA {
            graph.query(stmt, json!({})).await?;
        }
        let store = store::Store::new(graph);
        let mut created = 0usize;
        for (plugin, path) in ctx.files("sources-media") {
            let group = match std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read: {e}"))
                .and_then(|t| parse_group(&t))
            {
                Ok(g) => g,
                Err(e) => {
                    warn!(path = %path.display(), "sources-media: {e}");
                    continue;
                }
            };
            for s in &group.sources {
                let name = s
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", group.name, s.reference));
                let description = s
                    .description
                    .clone()
                    .unwrap_or_else(|| group.description.clone());
                match store
                    .seed_media_source_if_absent(
                        &name,
                        &s.kind,
                        &s.reference,
                        &description,
                        s.active,
                        &plugin,
                    )
                    .await
                {
                    Ok(true) => {
                        info!(%name, kind = %s.kind, %plugin, "Seeded MediaSource (graph-owned from now on)");
                        created += 1;
                    }
                    Ok(false) => {}
                    Err(e) => warn!(%name, error = %e, "sources-media: seed failed"),
                }
            }
        }
        if created > 0 {
            info!(created, "media: seeded new watchlist sources");
        }
        Ok(())
    }

    fn seed_kinds(&self) -> &'static [&'static str] {
        &["sources-media"]
    }

    fn check_seed(&self, kind: &str, text: &str) -> Result<(), String> {
        match kind {
            "sources-media" => parse_group(text).map(|_| ()),
            other => Err(format!("the media plugin does not read '{other}'")),
        }
    }

    fn retrieval_tools(&self) -> &'static [&'static str] {
        &["ingest_media"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_source_files_parse() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources-media");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap();
            parse_group(&text).unwrap_or_else(|err| panic!("{}: {err}", e.path().display()));
            n += 1;
        }
        assert!(n >= 3, "expected the shipped watchlists, found {n}");
    }

    #[test]
    fn check_seed_rejects_unknown_kinds() {
        let bad = "name: x\nsources:\n  - kind: tiktok\n    ref: abc\n";
        assert!(
            Media
                .check_seed("sources-media", bad)
                .unwrap_err()
                .contains("tiktok")
        );
        assert!(
            Media
                .check_seed("sources-media", "name: x\nsources: []\n")
                .is_err()
        );
    }
}
