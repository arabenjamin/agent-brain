//! Common reusable parameter types for skill tool inputs.

use serde::Deserialize;

pub fn default_limit_5() -> usize {
    5
}

pub fn default_graph_hops() -> usize {
    2
}

/// A single `limit` parameter with a default of 5.
#[derive(Deserialize)]
pub struct LimitParam {
    #[serde(default = "default_limit_5")]
    pub limit: usize,
}

/// Pagination parameters with `limit` (default 5) and `offset` (default 0).
#[derive(Deserialize)]
pub struct PaginationParam {
    #[serde(default = "default_limit_5")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

/// Search options shared across hybrid-search tool inputs.
#[derive(Deserialize)]
pub struct SearchOptions {
    #[serde(default = "default_limit_5")]
    pub limit: usize,
    #[serde(default = "default_graph_hops")]
    pub graph_hops: usize,
    #[serde(default)]
    pub entity_expansion: bool,
}
