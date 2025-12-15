//! API credential models for authentication management.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Type of credential for API authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    /// API Key authentication (e.g., X-API-Key header or query param)
    ApiKey,
    /// Bearer token authentication (Authorization: Bearer xxx)
    Bearer,
    /// Basic authentication (Authorization: Basic base64(user:pass))
    Basic,
    /// OAuth2 client credentials flow
    OAuth2ClientCredentials,
}

impl std::fmt::Display for CredentialType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialType::ApiKey => write!(f, "api_key"),
            CredentialType::Bearer => write!(f, "bearer"),
            CredentialType::Basic => write!(f, "basic"),
            CredentialType::OAuth2ClientCredentials => write!(f, "oauth2_client_credentials"),
        }
    }
}

impl std::str::FromStr for CredentialType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "api_key" | "apikey" => Ok(CredentialType::ApiKey),
            "bearer" => Ok(CredentialType::Bearer),
            "basic" => Ok(CredentialType::Basic),
            "oauth2_client_credentials" | "oauth2" => Ok(CredentialType::OAuth2ClientCredentials),
            _ => Err(format!("Unknown credential type: {}", s)),
        }
    }
}

/// Where to inject the credential in the HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectLocation {
    /// Inject as HTTP header
    Header,
    /// Inject as query parameter
    Query,
}

impl std::fmt::Display for InjectLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InjectLocation::Header => write!(f, "header"),
            InjectLocation::Query => write!(f, "query"),
        }
    }
}

impl std::str::FromStr for InjectLocation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "header" => Ok(InjectLocation::Header),
            "query" => Ok(InjectLocation::Query),
            _ => Err(format!("Unknown inject location: {}", s)),
        }
    }
}

/// Credential configuration for an API.
///
/// This struct stores metadata about how to authenticate with an API.
/// The actual secret value is stored separately in a SecretProvider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCredential {
    /// Unique identifier
    pub id: Uuid,
    /// API name this credential is for (normalized to lowercase)
    pub api_name: String,
    /// Type of credential
    pub credential_type: CredentialType,
    /// Where to inject the credential
    pub inject_location: InjectLocation,
    /// Key/header name for injection (e.g., "X-API-Key", "Authorization", "api_key")
    pub inject_key: String,
    /// Reference path in the secret provider (e.g., "openweathermap/api_key")
    pub secret_ref: String,
    /// Optional description
    pub description: Option<String>,
    /// Whether this credential is active
    pub active: bool,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last updated timestamp
    pub updated_at: DateTime<Utc>,
}

impl ApiCredential {
    /// Create a new API credential configuration.
    pub fn new(
        api_name: impl Into<String>,
        credential_type: CredentialType,
        inject_location: InjectLocation,
        inject_key: impl Into<String>,
        secret_ref: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            api_name: api_name.into(),
            credential_type,
            inject_location,
            inject_key: inject_key.into(),
            secret_ref: secret_ref.into(),
            description: None,
            active: true,
            created_at: now,
            updated_at: now,
        }
    }

    /// Add a description to this credential.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set the credential as inactive.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.updated_at = Utc::now();
    }

    /// Set the credential as active.
    pub fn activate(&mut self) {
        self.active = true;
        self.updated_at = Utc::now();
    }

    /// Update the timestamp.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_type_display() {
        assert_eq!(CredentialType::ApiKey.to_string(), "api_key");
        assert_eq!(CredentialType::Bearer.to_string(), "bearer");
        assert_eq!(CredentialType::Basic.to_string(), "basic");
        assert_eq!(
            CredentialType::OAuth2ClientCredentials.to_string(),
            "oauth2_client_credentials"
        );
    }

    #[test]
    fn test_credential_type_from_str() {
        assert_eq!(
            "api_key".parse::<CredentialType>().unwrap(),
            CredentialType::ApiKey
        );
        assert_eq!(
            "bearer".parse::<CredentialType>().unwrap(),
            CredentialType::Bearer
        );
        assert_eq!(
            "BASIC".parse::<CredentialType>().unwrap(),
            CredentialType::Basic
        );
        assert!("invalid".parse::<CredentialType>().is_err());
    }

    #[test]
    fn test_inject_location_display() {
        assert_eq!(InjectLocation::Header.to_string(), "header");
        assert_eq!(InjectLocation::Query.to_string(), "query");
    }

    #[test]
    fn test_inject_location_from_str() {
        assert_eq!(
            "header".parse::<InjectLocation>().unwrap(),
            InjectLocation::Header
        );
        assert_eq!(
            "QUERY".parse::<InjectLocation>().unwrap(),
            InjectLocation::Query
        );
        assert!("invalid".parse::<InjectLocation>().is_err());
    }

    #[test]
    fn test_api_credential_new() {
        let cred = ApiCredential::new(
            "OpenWeatherMap",
            CredentialType::ApiKey,
            InjectLocation::Query,
            "appid",
            "openweathermap/api_key",
        );

        assert_eq!(cred.api_name, "OpenWeatherMap");
        assert_eq!(cred.credential_type, CredentialType::ApiKey);
        assert_eq!(cred.inject_location, InjectLocation::Query);
        assert_eq!(cred.inject_key, "appid");
        assert_eq!(cred.secret_ref, "openweathermap/api_key");
        assert!(cred.active);
        assert!(cred.description.is_none());
    }

    #[test]
    fn test_api_credential_with_description() {
        let cred = ApiCredential::new(
            "Petstore",
            CredentialType::Bearer,
            InjectLocation::Header,
            "Authorization",
            "petstore/token",
        )
        .with_description("API token for Petstore");

        assert_eq!(cred.description, Some("API token for Petstore".to_string()));
    }

    #[test]
    fn test_api_credential_deactivate() {
        let mut cred = ApiCredential::new(
            "Test",
            CredentialType::ApiKey,
            InjectLocation::Header,
            "X-API-Key",
            "test/key",
        );

        assert!(cred.active);
        let original_updated = cred.updated_at;

        // Small delay to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(10));

        cred.deactivate();
        assert!(!cred.active);
        assert!(cred.updated_at > original_updated);
    }
}
