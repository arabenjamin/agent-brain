//! What the knowledge layer stands on: plugin bundles (where seeds are found,
//! and what each plugin's code contributed), the core's architecture files,
//! logging and its ring buffer, the clock, entity resolution, topics, and
//! graph snapshots. Split out of the
//! app crate in CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module at
//! its old `services::` path.

pub mod architecture;
pub mod bundles;
pub mod clock;
pub mod entity_resolution;
pub mod logging;
pub mod snapshot;
pub mod topics;
