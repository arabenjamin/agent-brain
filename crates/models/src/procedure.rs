//! Procedure model — represents a stored multi-step workflow.

use serde::{Deserialize, Serialize};

/// A stored procedural memory: a named, reusable sequence of tool steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Procedure {
    /// Unique identifier.
    pub id: String,

    /// Short human-readable name for the procedure.
    pub name: String,

    /// Longer description of when and why to use this procedure.
    pub description: String,

    /// Ordered list of steps. Each step is a JSON object with at minimum
    /// `tool` (string) and `purpose` (string); `args` is optional.
    pub steps: Vec<serde_json::Value>,

    /// ISO-8601 creation timestamp.
    pub created_at: String,
}
