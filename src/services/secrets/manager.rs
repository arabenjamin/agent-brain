//! Credential manager for API authentication.
//!
//! This module provides the `CredentialManager` which orchestrates
//! credential storage, retrieval, and injection into HTTP requests.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::models::{ApiCredential, CredentialType, InjectLocation};
use crate::repository::Neo4jClient;
use crate::services::context::ContextStore;
use crate::services::http::RequestBuilder;

use super::error::{Result, SecretError};
use super::provider::BoxedSecretProvider;

/// Configuration for the credential manager.
#[derive(Debug, Clone, Default)]
pub struct CredentialManagerConfig {
    /// Whether to cache resolved credentials in memory.
    pub cache_credentials: bool,
    /// Cache TTL in seconds (default: 300).
    pub cache_ttl_secs: u64,
}

impl CredentialManagerConfig {
    /// Enable or disable credential caching.
    pub fn with_caching(mut self, enabled: bool) -> Self {
        self.cache_credentials = enabled;
        self
    }

    /// Set the cache TTL in seconds.
    pub fn with_cache_ttl(mut self, secs: u64) -> Self {
        self.cache_ttl_secs = secs;
        self
    }
}

/// Manages API credentials and injects them into HTTP requests.
///
/// The credential manager serves as the central point for:
/// - Storing credential configurations (metadata about how to authenticate)
/// - Retrieving actual secret values from a secret provider
/// - Automatically injecting credentials into HTTP requests based on URL matching
///
/// # Example
///
/// ```ignore
/// let manager = CredentialManager::new()
///     .with_secret_provider(Box::new(local_provider))
///     .with_context_store(context_store);
///
/// // Configure a credential
/// let cred = ApiCredential::new(
///     "OpenWeatherMap",
///     CredentialType::ApiKey,
///     InjectLocation::Query,
///     "appid",
///     "openweathermap/api_key",
/// );
/// manager.configure_credential(cred).await?;
///
/// // Auto-inject credentials when making requests
/// let builder = manager.inject_credentials_for_url(
///     "https://api.openweathermap.org/data/2.5/weather",
///     RequestBuilder::new(),
/// ).await?;
/// ```
pub struct CredentialManager {
    /// Secret provider for retrieving actual secret values.
    secret_provider: Option<BoxedSecretProvider>,
    /// Neo4j client for persistent credential metadata storage.
    neo4j: Option<Neo4jClient>,
    /// Context store for URL-to-API matching.
    context_store: Option<ContextStore>,
    /// In-memory credential metadata cache.
    credentials: Arc<RwLock<HashMap<String, ApiCredential>>>,
    /// Configuration.
    config: CredentialManagerConfig,
}

impl CredentialManager {
    /// Create a new credential manager.
    pub fn new() -> Self {
        Self {
            secret_provider: None,
            neo4j: None,
            context_store: None,
            credentials: Arc::new(RwLock::new(HashMap::new())),
            config: CredentialManagerConfig::default(),
        }
    }

    /// Set the secret provider.
    pub fn with_secret_provider(mut self, provider: BoxedSecretProvider) -> Self {
        self.secret_provider = Some(provider);
        self
    }

    /// Set the Neo4j client for persistent credential metadata.
    pub fn with_neo4j(mut self, client: Neo4jClient) -> Self {
        self.neo4j = Some(client);
        self
    }

    /// Set the context store for URL-to-API matching.
    pub fn with_context_store(mut self, store: ContextStore) -> Self {
        self.context_store = Some(store);
        self
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: CredentialManagerConfig) -> Self {
        self.config = config;
        self
    }

    /// Normalize API name for consistent lookup.
    fn normalize_name(name: &str) -> String {
        name.to_lowercase().replace([' ', '_'], "-")
    }

    /// Configure a credential for an API.
    ///
    /// This stores the credential metadata (not the actual secret value).
    /// The secret value should be stored separately in the secret provider.
    pub async fn configure_credential(&self, credential: ApiCredential) -> Result<()> {
        let key = Self::normalize_name(&credential.api_name);

        // Store in Neo4j if available
        if let Some(neo4j) = &self.neo4j {
            neo4j.create_api_credential(&credential).await?;
        }

        // Store in memory cache
        {
            let mut creds = self.credentials.write().await;
            creds.insert(key.clone(), credential.clone());
        }

        info!(api = %credential.api_name, "Configured API credential");
        Ok(())
    }

    /// Configure a credential and store the secret value.
    ///
    /// This is a convenience method that stores both the credential metadata
    /// and the actual secret value in one call.
    pub async fn configure_credential_with_secret(
        &self,
        credential: ApiCredential,
        secret_value: &str,
    ) -> Result<()> {
        // Store the secret value first
        if let Some(provider) = &self.secret_provider {
            provider
                .set_secret(&credential.secret_ref, secret_value)
                .await?;
        } else {
            return Err(SecretError::ProviderNotConfigured);
        }

        // Then store the credential metadata
        self.configure_credential(credential).await
    }

    /// Get credential metadata for an API.
    pub async fn get_credential(&self, api_name: &str) -> Result<ApiCredential> {
        let key = Self::normalize_name(api_name);

        // Check memory first
        {
            let creds = self.credentials.read().await;
            if let Some(cred) = creds.get(&key) {
                return Ok(cred.clone());
            }
        }

        // Try loading from Neo4j
        if let Some(neo4j) = &self.neo4j
            && let Ok(cred) = neo4j.get_api_credential(api_name).await
        {
            // Cache it
            let mut creds = self.credentials.write().await;
            creds.insert(key, cred.clone());
            return Ok(cred);
        }

        Err(SecretError::CredentialNotFound(api_name.to_string()))
    }

    /// List all configured credentials.
    pub async fn list_credentials(&self) -> Result<Vec<ApiCredential>> {
        // Return from Neo4j if available (authoritative source)
        if let Some(neo4j) = &self.neo4j {
            return neo4j.list_api_credentials().await.map_err(Into::into);
        }

        // Fall back to memory cache
        let creds = self.credentials.read().await;
        Ok(creds.values().cloned().collect())
    }

    /// Delete a credential configuration.
    pub async fn delete_credential(&self, api_name: &str) -> Result<()> {
        let key = Self::normalize_name(api_name);

        // Get the credential to find its secret_ref
        let credential = self.get_credential(api_name).await?;

        // Delete the secret from provider
        if let Some(provider) = &self.secret_provider
            && let Err(e) = provider.delete_secret(&credential.secret_ref).await
        {
            warn!(
                api = %api_name,
                secret_ref = %credential.secret_ref,
                error = %e,
                "Failed to delete secret from provider"
            );
        }

        // Delete from Neo4j if available
        if let Some(neo4j) = &self.neo4j {
            neo4j.delete_api_credential(api_name).await?;
        }

        // Remove from memory
        {
            let mut creds = self.credentials.write().await;
            creds.remove(&key);
        }

        info!(api = %api_name, "Deleted API credential");
        Ok(())
    }

    /// Detect which API a URL belongs to based on base URL matching.
    ///
    /// Returns the API name if a match is found.
    pub async fn detect_api_from_url(&self, url: &str) -> Option<String> {
        let context_store = self.context_store.as_ref()?;
        let contexts = context_store.get_all().await;

        for ctx in contexts {
            if let Some(base_url) = &ctx.base_url {
                // Normalize URLs for comparison
                let normalized_base = base_url.trim_end_matches('/');
                if url.starts_with(normalized_base) {
                    debug!(
                        url = %url,
                        api = %ctx.name,
                        base_url = %normalized_base,
                        "Matched URL to API"
                    );
                    return Some(ctx.name.clone());
                }
            }
        }

        None
    }

    /// Check if credentials are configured for an API.
    pub async fn has_credentials(&self, api_name: &str) -> bool {
        self.get_credential(api_name).await.is_ok()
    }

    /// Inject credentials into a request builder for a specific API.
    pub async fn inject_credentials(
        &self,
        api_name: &str,
        mut builder: RequestBuilder,
    ) -> Result<RequestBuilder> {
        let credential = self.get_credential(api_name).await?;

        if !credential.active {
            warn!(api = %api_name, "Credential is inactive, skipping injection");
            return Ok(builder);
        }

        // Get the actual secret value
        let secret_value = match &self.secret_provider {
            Some(provider) => provider.get_secret(&credential.secret_ref).await?,
            None => return Err(SecretError::ProviderNotConfigured),
        };

        // Format the value based on credential type
        let formatted_value = self.format_credential_value(&credential, &secret_value)?;

        // Inject based on location
        match credential.inject_location {
            InjectLocation::Header => {
                builder = builder.header(&credential.inject_key, formatted_value);
            }
            InjectLocation::Query => {
                builder = builder.query_param(&credential.inject_key, formatted_value);
            }
        }

        debug!(
            api = %api_name,
            location = %credential.inject_location,
            key = %credential.inject_key,
            "Injected credential into request"
        );

        Ok(builder)
    }

    /// Store a secret value directly in the secret provider.
    ///
    /// This is useful for storing secret values when you already have the secret_ref.
    pub async fn store_secret(&self, secret_ref: &str, value: &str) -> Result<()> {
        let provider = self
            .secret_provider
            .as_ref()
            .ok_or(SecretError::ProviderNotConfigured)?;
        provider.set_secret(secret_ref, value).await
    }

    /// Delete a secret value from the secret provider.
    ///
    /// This is useful for deleting secret values when you have the secret_ref.
    pub async fn delete_secret(&self, secret_ref: &str) -> Result<()> {
        let provider = self
            .secret_provider
            .as_ref()
            .ok_or(SecretError::ProviderNotConfigured)?;
        provider.delete_secret(secret_ref).await
    }

    /// Inject credentials into a request builder based on URL auto-detection.
    ///
    /// If the URL matches a known API with configured credentials, they will be injected.
    /// If no match is found or no credentials are configured, the builder is returned unchanged.
    pub async fn inject_credentials_for_url(
        &self,
        url: &str,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder> {
        // Try to detect which API this URL belongs to
        if let Some(api_name) = self.detect_api_from_url(url).await {
            // Check if we have credentials for this API
            if self.has_credentials(&api_name).await {
                return self.inject_credentials(&api_name, builder).await;
            }
        }

        // No match or no credentials - return unchanged
        Ok(builder)
    }

    /// Format the credential value based on its type.
    fn format_credential_value(
        &self,
        credential: &ApiCredential,
        secret_value: &str,
    ) -> Result<String> {
        match credential.credential_type {
            CredentialType::ApiKey => Ok(secret_value.to_string()),
            CredentialType::Bearer => Ok(format!("Bearer {}", secret_value)),
            CredentialType::Basic => {
                // Expect secret_value to be "username:password"
                let encoded = base64::engine::general_purpose::STANDARD.encode(secret_value);
                Ok(format!("Basic {}", encoded))
            }
            CredentialType::OAuth2ClientCredentials => {
                // For OAuth2, the secret is the access token
                // Token refresh should be handled separately
                Ok(format!("Bearer {}", secret_value))
            }
        }
    }
}

impl Default for CredentialManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::secrets::{LocalSecretConfig, LocalSecretProvider};
    use tempfile::tempdir;

    async fn create_test_manager() -> (CredentialManager, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        let config = LocalSecretConfig::new(&file_path).with_encryption_key("test-key");
        let provider = LocalSecretProvider::new(config).unwrap();

        let manager = CredentialManager::new().with_secret_provider(Box::new(provider));

        (manager, dir)
    }

    #[tokio::test]
    async fn test_credential_manager_configure_and_get() {
        let (manager, _dir) = create_test_manager().await;

        // Store the secret first
        if let Some(provider) = &manager.secret_provider {
            provider
                .set_secret("test/api_key", "secret123")
                .await
                .unwrap();
        }

        // Configure credential
        let cred = ApiCredential::new(
            "TestAPI",
            CredentialType::ApiKey,
            InjectLocation::Header,
            "X-API-Key",
            "test/api_key",
        );

        manager.configure_credential(cred).await.unwrap();

        // Retrieve it
        let retrieved = manager.get_credential("TestAPI").await.unwrap();
        assert_eq!(retrieved.api_name, "TestAPI");
        assert_eq!(retrieved.credential_type, CredentialType::ApiKey);
    }

    #[tokio::test]
    async fn test_credential_manager_normalize_name() {
        let (manager, _dir) = create_test_manager().await;

        let cred = ApiCredential::new(
            "OpenWeatherMap API",
            CredentialType::ApiKey,
            InjectLocation::Query,
            "appid",
            "owm/key",
        );

        manager.configure_credential(cred).await.unwrap();

        // Should find with different cases/formats
        assert!(manager.get_credential("OpenWeatherMap API").await.is_ok());
        assert!(manager.get_credential("openweathermap-api").await.is_ok());
        assert!(manager.get_credential("openweathermap_api").await.is_ok());
    }

    #[tokio::test]
    async fn test_credential_manager_list() {
        let (manager, _dir) = create_test_manager().await;

        let cred1 = ApiCredential::new(
            "API1",
            CredentialType::ApiKey,
            InjectLocation::Header,
            "X-Key",
            "api1/key",
        );
        let cred2 = ApiCredential::new(
            "API2",
            CredentialType::Bearer,
            InjectLocation::Header,
            "Authorization",
            "api2/token",
        );

        manager.configure_credential(cred1).await.unwrap();
        manager.configure_credential(cred2).await.unwrap();

        let list = manager.list_credentials().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_format_api_key() {
        let (manager, _dir) = create_test_manager().await;

        let cred = ApiCredential::new(
            "Test",
            CredentialType::ApiKey,
            InjectLocation::Header,
            "X-Key",
            "test/key",
        );

        let formatted = manager.format_credential_value(&cred, "mykey123").unwrap();
        assert_eq!(formatted, "mykey123");
    }

    #[tokio::test]
    async fn test_format_bearer() {
        let (manager, _dir) = create_test_manager().await;

        let cred = ApiCredential::new(
            "Test",
            CredentialType::Bearer,
            InjectLocation::Header,
            "Authorization",
            "test/token",
        );

        let formatted = manager.format_credential_value(&cred, "token123").unwrap();
        assert_eq!(formatted, "Bearer token123");
    }

    #[tokio::test]
    async fn test_format_basic() {
        let (manager, _dir) = create_test_manager().await;

        let cred = ApiCredential::new(
            "Test",
            CredentialType::Basic,
            InjectLocation::Header,
            "Authorization",
            "test/basic",
        );

        let formatted = manager.format_credential_value(&cred, "user:pass").unwrap();
        // "user:pass" base64 encoded
        assert_eq!(formatted, "Basic dXNlcjpwYXNz");
    }

    #[tokio::test]
    async fn test_configure_with_secret() {
        let (manager, _dir) = create_test_manager().await;

        let cred = ApiCredential::new(
            "TestAPI",
            CredentialType::ApiKey,
            InjectLocation::Header,
            "X-API-Key",
            "test/secret",
        );

        manager
            .configure_credential_with_secret(cred, "supersecret")
            .await
            .unwrap();

        // Verify secret was stored
        if let Some(provider) = &manager.secret_provider {
            let value = provider.get_secret("test/secret").await.unwrap();
            assert_eq!(value, "supersecret");
        }

        // Verify credential metadata was stored
        let retrieved = manager.get_credential("TestAPI").await.unwrap();
        assert_eq!(retrieved.secret_ref, "test/secret");
    }
}
