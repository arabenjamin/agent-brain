//! The plugin host: the compiled plugin crates and what they are given
//! (`plugin_host`), the bundles' ON-CREATE seeds (`plugins`), `manage_plugin`
//! and the declared `tools/` of a bundle, and the self-model sync that records
//! what the brain is made of. Split out of the app crate in
//! CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module at its old path.

pub mod bundle_tools;
pub mod plugin_host;
pub mod plugin_manager;
pub mod plugins;
pub mod self_model;
pub mod views;
