use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A data object definition from the OpenAPI spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub id: Uuid,
    pub name: String,
    pub json_structure: serde_json::Value,
}

impl Schema {
    pub fn new(name: impl Into<String>, json_structure: serde_json::Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            json_structure,
        }
    }
}
