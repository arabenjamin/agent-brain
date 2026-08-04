//! MediaSource seeder — reads `sources-media/*.yaml` and seeds `(:MediaSource)`
//! watchlist nodes **ON CREATE only** (`managed_by = 'yaml'`), mirroring the
//! SourceList seeder. The graph owns each node after first creation, so runtime
//! edits via `manage_media_source` / `neo4j_query` persist across restarts.
//! Delete a node to re-seed it from YAML. Missing directory is non-fatal.

use std::path::Path;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::repository::Neo4jClient;

/// One YAML file = a named group of related channels/playlists/feeds.
#[derive(Debug, Deserialize)]
struct MediaSourceGroupFile {
    /// Group label; used as the default name prefix for each entry.
    name: String,
    #[serde(default)]
    description: String,
    sources: Vec<MediaSourceEntry>,
}

#[derive(Debug, Deserialize)]
struct MediaSourceEntry {
    /// Optional explicit node name. Defaults to `"{group}-{ref}"`.
    #[serde(default)]
    name: Option<String>,
    /// `youtube_channel` | `youtube_playlist` | `podcast_rss`
    kind: String,
    /// channel_id / playlist_id / rss_url.
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_true() -> bool {
    true
}

/// Seed every `*.yaml` group in `dir`. Returns the number of nodes newly created.
pub async fn seed_media_sources_from_dir(neo4j: &Neo4jClient, dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "seed_media_sources: cannot read directory (non-fatal)");
            return 0;
        }
    };

    let mut created = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_media_sources: cannot read file");
                continue;
            }
        };
        let group: MediaSourceGroupFile = match serde_yaml::from_str(&text) {
            Ok(g) => g,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_media_sources: YAML parse error");
                continue;
            }
        };

        for src in &group.sources {
            let name = src
                .name
                .clone()
                .unwrap_or_else(|| format!("{}-{}", group.name, src.reference));
            let description = src
                .description
                .clone()
                .unwrap_or_else(|| group.description.clone());
            match neo4j
                .seed_media_source_if_absent(
                    &name,
                    &src.kind,
                    &src.reference,
                    &description,
                    src.active,
                )
                .await
            {
                Ok(true) => {
                    info!(name = %name, kind = %src.kind, "Seeded MediaSource (graph-owned from now on)");
                    created += 1;
                }
                Ok(false) => debug!(name = %name, "MediaSource already exists — graph copy kept"),
                Err(e) => warn!(name = %name, error = %e, "seed_media_sources: seed failed"),
            }
        }
    }
    created
}
