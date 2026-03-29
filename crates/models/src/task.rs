use serde::{Deserialize, Serialize};

/// Status of a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskStatus {
    #[default]
    Created,
    InProgress,
    Completed,
    Failed,
    Blocked,
}

/// A high-level task or goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier (UUID).
    pub id: String,
    /// Main objective.
    pub goal: String,
    /// Additional context.
    #[serde(default)]
    pub context: Option<String>,
    /// Current status.
    pub status: TaskStatus,
    /// When the task was created.
    pub created_at: String, // ISO 8601
    /// When the task was last updated.
    pub updated_at: String, // ISO 8601
}
