//! What the core knows about its own make-up (HBI_REDESIGN_PLAN.md §4): the
//! core bundle's `architecture/` files (`components.yaml`, the services the
//! brain depends on; `removed-tools.yaml`, the tombstones) and the router
//! source this binary was compiled from.
//!
//! The architecture *view* (inventory, layout, snapshots and their diffs,
//! `GET /api/plugins/developer/architecture`) is the `developer` plugin's
//! (CORE_AND_PLUGINS_PLAN Phase 4); it reads these through the host's
//! introspection capability.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing::warn;

pub fn architecture_dir() -> PathBuf {
    crate::services::plugins::Plugins::global().core_dir("architecture")
}

pub(crate) fn load_yaml<T: for<'de> Deserialize<'de> + Default>(file: &str) -> T {
    let p = architecture_dir().join(file);
    match std::fs::read_to_string(&p)
        .map_err(anyhow::Error::from)
        .and_then(|t| serde_yaml::from_str::<T>(&t).map_err(Into::into))
    {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %p.display(), error = %e, "architecture: file not loaded");
            T::default()
        }
    }
}

/// One service the brain depends on (`components.yaml`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Component {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub kind: String,
    /// `neo4j` | `duckdb` | `http`.
    pub probe: String,
    #[serde(default)]
    pub url_env: Option<String>,
    #[serde(default)]
    pub strip_suffix: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub purpose: String,
}

/// The shape of `components.yaml`.
#[derive(Debug, Default, Deserialize)]
pub struct ComponentsFile {
    #[serde(default)]
    pub components: Vec<Component>,
}

/// `components.yaml`, read once. Missing → no probes, and the health response
/// says the list is empty rather than claiming all is well.
pub fn components() -> &'static [Component] {
    static C: OnceLock<Vec<Component>> = OnceLock::new();
    C.get_or_init(|| load_yaml::<ComponentsFile>("components.yaml").components)
}

/// The HTTP transport's source, compiled in: its `.route(..)` calls are the
/// core's routes (the developer plugin parses them).
pub const ROUTER_SOURCE: &str = include_str!("../mcp/http_transport.rs");
