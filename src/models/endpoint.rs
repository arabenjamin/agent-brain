use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// HTTP method for an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpMethod::Get => write!(f, "GET"),
            HttpMethod::Post => write!(f, "POST"),
            HttpMethod::Put => write!(f, "PUT"),
            HttpMethod::Patch => write!(f, "PATCH"),
            HttpMethod::Delete => write!(f, "DELETE"),
            HttpMethod::Head => write!(f, "HEAD"),
            HttpMethod::Options => write!(f, "OPTIONS"),
        }
    }
}

/// Verification status of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndpointStatus {
    #[default]
    Unknown,
    Verified,
    DocumentationInvalid,
    Broken,
}

/// A specific API path and method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: Uuid,
    pub path: String,
    pub method: HttpMethod,
    pub summary: String,
    pub operation_id: Option<String>,
    pub status: EndpointStatus,
    pub last_verified_status: Option<u16>,
    pub healed_by_ai: bool,
}

impl Endpoint {
    pub fn new(
        path: impl Into<String>,
        method: HttpMethod,
        summary: impl Into<String>,
        operation_id: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            path: path.into(),
            method,
            summary: summary.into(),
            operation_id,
            status: EndpointStatus::Unknown,
            last_verified_status: None,
            healed_by_ai: false,
        }
    }
}
