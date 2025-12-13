use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A high-level grouping of API endpoints (e.g., "Users", "Payments").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: Uuid,
    pub name: String,
    pub description: String,
}

impl Resource {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: description.into(),
        }
    }
}
