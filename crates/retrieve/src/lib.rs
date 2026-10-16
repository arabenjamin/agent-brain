//! Retrieval tools: web search (the engine ladder and its YAML engines),
//! `http_request` and `ingest_api`, the MCP client tools, and the graph and
//! telemetry query tools. Split out of the app crate in
//! CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module at its old
//! `skills::` path.

pub mod http;
pub mod mcp_client;
pub mod page_readers;
pub mod query;
pub mod read;
pub mod search;
pub mod search_engines;
pub mod search_quality;
pub mod search_query;
pub mod search_rotation;
pub mod serde_flex;

pub use http::HttpSkill;
pub use query::QuerySkill;
pub use search::SearchSkill;
