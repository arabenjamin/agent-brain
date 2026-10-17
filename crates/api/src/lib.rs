//! The HBI's REST API: the shared handler plumbing (`rest`) and one module per
//! workspace (work, search, topics, related entities, items, the Feed,
//! system health). Split out of the app crate in CORE_AND_PLUGINS_PLAN 6f; the
//! app re-exports each module at its old `clients::` path and mounts the
//! routes in `mcp/http_transport.rs`.

pub mod entities;
pub mod feed;
pub mod items;
pub mod programs;
pub mod related;
pub mod rest;
pub mod search;
pub mod system;
pub mod topics;
pub mod work;

pub use rest::RestAdapter;
