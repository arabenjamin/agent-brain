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

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};
use serde::Deserialize;
use tracing::{info, warn};

mod read_rules;
mod service;
mod skill;
mod store;
mod transcribe;

pub struct Media;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The bundle's folder (`read-rules.yaml` lives there); the source tree's in tests.
pub(crate) fn bundle_root() -> PathBuf {
    ROOT.get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")))
}

/// A group of watched sources (`sources-media/*.yaml`).
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceGroupFile {
    name: String,
    #[serde(default)]
    description: String,
    sources: Vec<SourceEntry>,
}

#[cfg_attr(test, derive(schemars::JsonSchema))]
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
    /// The source's tier (SOURCE_QUALITY_PLAN.md § 3a). Seeded on create only;
    /// a later change is the owner's, through `manage_media_source` retype.
    #[serde(default)]
    source_type: Option<String>,
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
    if let Some(t) = g
        .sources
        .iter()
        .filter_map(|s| s.source_type.as_deref())
        .find(|t| !store::SOURCE_TYPES.contains(t))
    {
        return Err(format!(
            "source_type '{t}' is not one of {}",
            store::SOURCE_TYPES.join(", ")
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
        if let Some(r) = ctx.root() {
            let _ = ROOT.set(r.to_path_buf());
        }
        let svc = Arc::new(
            service::MediaService::from_env()
                .with_reader(ctx.reader.clone())
                .with_telemetry(ctx),
        );
        if let Some(s) = skill::MediaSkill::new(svc, ctx) {
            vec![Arc::new(s) as Arc<dyn Skill>]
        } else {
            warn!("media: the host gave no graph or LLM; no tools");
            vec![]
        }
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        let graph = ctx
            .graph
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no graph"))?;
        // Constraints and indexes: schemas/graph/schema.cypher.
        agent_brain_plugin_api::graph::apply_schema(
            graph.as_ref(),
            agent_brain_plugin_api::protocol::graph_schema!(plugin),
        )
        .await?;
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
                        s.source_type.as_deref(),
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

    #[test]
    fn check_seed_takes_known_source_types_only() {
        let ok = "name: x\nsources:\n  - kind: article_rss\n    ref: https://a/rss\n    source_type: primary\n";
        assert!(parse_group(ok).is_ok());
        let bad = "name: x\nsources:\n  - kind: article_rss\n    ref: https://a/rss\n    source_type: gospel\n";
        assert!(parse_group(bad).unwrap_err().contains("gospel"));
    }
}

/// The JSON Schema of `sources-media`, generated from the Rust type that parses it
/// and committed under the bundle's `schemas/seeds/` (CODEBASE_SPLIT_PLAN
/// Phase 1). `SCHEMA_REGENERATE=1` rewrites it after a type change.
#[cfg(test)]
mod seed_schemas {
    use agent_brain_plugin_api::protocol::seed_schema_check::{
        SeedKind, check, repo_root, seed_files,
    };

    #[test]
    fn seed_schemas_match_their_types_and_the_shipped_files() {
        let root = repo_root(env!("CARGO_MANIFEST_DIR"));
        let kinds = vec![SeedKind {
            kind: "sources-media".into(),
            schema: serde_json::to_value(schemars::schema_for!(super::SourceGroupFile)).unwrap(),
            schema_file: "plugins/media/schemas/seeds/sources-media.json".into(),
            seeds: seed_files(&root, "sources-media", None),
            none_shipped: false,
        }];
        check(&root, &kinds).unwrap_or_else(|e| panic!("{e}"));
    }
}
