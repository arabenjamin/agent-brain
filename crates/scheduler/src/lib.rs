//! Autonomy: the scheduler tick (pending tasks become job chains, due
//! workflows start runs), items and their dispatch, and run repair. Split out
//! of the app crate in CORE_AND_PLUGINS_PLAN 6f; the app re-exports each
//! module at its old `services::` path.

pub mod item_dispatch;
pub mod items;
pub mod repair;
pub mod scheduler;

pub use scheduler::{SchedulerConfig, SchedulerService};
