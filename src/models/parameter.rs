use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where the parameter is located in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParameterLocation {
    Query,
    Path,
    Body,
    Header,
}

impl std::fmt::Display for ParameterLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParameterLocation::Query => write!(f, "query"),
            ParameterLocation::Path => write!(f, "path"),
            ParameterLocation::Body => write!(f, "body"),
            ParameterLocation::Header => write!(f, "header"),
        }
    }
}

/// Input required for an endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub id: Uuid,
    pub name: String,
    pub location: ParameterLocation,
    pub required: bool,
    pub param_type: Option<String>,
    pub description: Option<String>,
}

impl Parameter {
    pub fn new(name: impl Into<String>, location: ParameterLocation, required: bool) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            location,
            required,
            param_type: None,
            description: None,
        }
    }

    pub fn with_type(mut self, param_type: impl Into<String>) -> Self {
        self.param_type = Some(param_type.into());
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}
