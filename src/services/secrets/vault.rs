//! HashiCorp Vault secret provider (KV v2 engine).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::error::{Result, SecretError};
use super::provider::SecretProvider;

/// Default request timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Configuration for HashiCorp Vault.
#[derive(Debug, Clone)]
pub struct VaultConfig {
    /// Vault server address (e.g., "https://vault.example.com:8200").
    pub address: String,
    /// Vault token for authentication.
    pub token: String,
    /// KV secrets engine mount path (default: "secret").
    pub mount_path: String,
    /// Request timeout.
    pub timeout: Duration,
    /// Optional namespace for Vault Enterprise.
    pub namespace: Option<String>,
}

impl VaultConfig {
    /// Create a new Vault configuration.
    pub fn new(address: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            token: token.into(),
            mount_path: "secret".to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            namespace: None,
        }
    }

    /// Set the KV mount path.
    pub fn with_mount_path(mut self, path: impl Into<String>) -> Self {
        self.mount_path = path.into();
        self
    }

    /// Set the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the Vault Enterprise namespace.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// Vault KV v2 read response structure.
#[derive(Debug, Deserialize)]
struct VaultReadResponse {
    data: VaultDataWrapper,
}

#[derive(Debug, Deserialize)]
struct VaultDataWrapper {
    data: serde_json::Value,
}

/// Vault KV v2 list response structure.
#[derive(Debug, Deserialize)]
struct VaultListResponse {
    data: VaultListData,
}

#[derive(Debug, Deserialize)]
struct VaultListData {
    keys: Vec<String>,
}

/// Vault KV v2 write request structure.
#[derive(Debug, Serialize)]
struct VaultWriteRequest {
    data: serde_json::Value,
}

/// HashiCorp Vault secret provider using the KV v2 secrets engine.
///
/// This provider communicates with Vault's HTTP API to store and retrieve secrets.
/// It uses token-based authentication and supports Vault Enterprise namespaces.
pub struct VaultSecretProvider {
    config: VaultConfig,
    client: Client,
}

impl VaultSecretProvider {
    /// Create a new Vault secret provider.
    ///
    /// # Arguments
    /// * `config` - Configuration for connecting to Vault
    ///
    /// # Returns
    /// A new VaultSecretProvider instance.
    pub fn new(config: VaultConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(SecretError::Http)?;

        Ok(Self { config, client })
    }

    /// Build the full URL for a secret path.
    ///
    /// KV v2 uses different paths for different operations:
    /// - `/data/` for read/write
    /// - `/metadata/` for list/delete
    fn build_url(&self, path: &str, operation: &str) -> String {
        let base = self.config.address.trim_end_matches('/');
        let mount = &self.config.mount_path;
        format!("{}/v1/{}/{}/{}", base, mount, operation, path)
    }

    /// Add common headers to a request.
    fn add_headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut builder = builder.header("X-Vault-Token", &self.config.token);
        if let Some(ns) = &self.config.namespace {
            builder = builder.header("X-Vault-Namespace", ns);
        }
        builder
    }
}

impl SecretProvider for VaultSecretProvider {
    fn get_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            let url = self.build_url(&path, "data");
            debug!(url = %url, "Reading secret from Vault");

            let request = self.add_headers(self.client.get(&url));
            let response = request.send().await?;

            if response.status() == 404 {
                return Err(SecretError::NotFound(path));
            }

            if response.status() == 403 {
                return Err(SecretError::AccessDenied(path));
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(SecretError::Vault {
                    status,
                    message: body,
                });
            }

            let vault_response: VaultReadResponse = response.json().await?;

            // Extract the "value" key from the secret data
            vault_response
                .data
                .data
                .get("value")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| SecretError::NotFound(format!("{} (no 'value' key)", path)))
        })
    }

    fn set_secret(
        &self,
        path: &str,
        value: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let path = path.to_string();
        let value = value.to_string();
        Box::pin(async move {
            let url = self.build_url(&path, "data");
            debug!(url = %url, "Writing secret to Vault");

            let payload = VaultWriteRequest {
                data: serde_json::json!({ "value": value }),
            };

            let request = self.add_headers(self.client.post(&url)).json(&payload);
            let response = request.send().await?;

            if response.status() == 403 {
                return Err(SecretError::AccessDenied(path));
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(SecretError::Vault {
                    status,
                    message: body,
                });
            }

            info!(path = %path, "Secret written to Vault");
            Ok(())
        })
    }

    fn delete_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            // Delete uses the metadata path to permanently delete
            let url = self.build_url(&path, "metadata");
            debug!(url = %url, "Deleting secret from Vault");

            let request = self.add_headers(self.client.delete(&url));
            let response = request.send().await?;

            if response.status() == 404 {
                return Err(SecretError::NotFound(path));
            }

            if response.status() == 403 {
                return Err(SecretError::AccessDenied(path));
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(SecretError::Vault {
                    status,
                    message: body,
                });
            }

            info!(path = %path, "Secret deleted from Vault");
            Ok(())
        })
    }

    fn list_secrets(
        &self,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let url = self.build_url(&prefix, "metadata");
            debug!(url = %url, "Listing secrets from Vault");

            // Vault uses the LIST HTTP method
            let request = self
                .add_headers(self.client.request(reqwest::Method::from_bytes(b"LIST").unwrap(), &url));
            let response = request.send().await?;

            if response.status() == 404 {
                // No secrets under this prefix
                return Ok(Vec::new());
            }

            if response.status() == 403 {
                return Err(SecretError::AccessDenied(prefix));
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                return Err(SecretError::Vault {
                    status,
                    message: body,
                });
            }

            let vault_response: VaultListResponse = response.json().await?;

            // Prepend prefix to keys for consistency
            let keys: Vec<String> = vault_response
                .data
                .keys
                .into_iter()
                .map(|k| {
                    if prefix.is_empty() {
                        k
                    } else {
                        format!("{}{}", prefix.trim_end_matches('/'), k)
                    }
                })
                .collect();

            Ok(keys)
        })
    }

    fn health_check(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let url = format!(
                "{}/v1/sys/health",
                self.config.address.trim_end_matches('/')
            );

            let response = self.client.get(&url).send().await?;

            // Vault health endpoint returns various status codes:
            // 200 - initialized, unsealed, active
            // 429 - unsealed, standby
            // 472 - disaster recovery mode replication secondary and active
            // 473 - performance standby
            // 501 - not initialized
            // 503 - sealed
            Ok(response.status().is_success() || response.status().as_u16() == 429)
        })
    }

    fn provider_name(&self) -> &'static str {
        "vault"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_config_defaults() {
        let config = VaultConfig::new("https://vault.example.com:8200", "my-token");
        assert_eq!(config.address, "https://vault.example.com:8200");
        assert_eq!(config.token, "my-token");
        assert_eq!(config.mount_path, "secret");
        assert!(config.namespace.is_none());
    }

    #[test]
    fn test_vault_config_builder() {
        let config = VaultConfig::new("https://vault.example.com:8200", "my-token")
            .with_mount_path("kv")
            .with_namespace("my-namespace")
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.mount_path, "kv");
        assert_eq!(config.namespace, Some("my-namespace".to_string()));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_build_url() {
        let config = VaultConfig::new("https://vault.example.com:8200", "token");
        let provider = VaultSecretProvider::new(config).unwrap();

        assert_eq!(
            provider.build_url("my/secret", "data"),
            "https://vault.example.com:8200/v1/secret/data/my/secret"
        );

        assert_eq!(
            provider.build_url("my/secret", "metadata"),
            "https://vault.example.com:8200/v1/secret/metadata/my/secret"
        );
    }

    #[test]
    fn test_build_url_custom_mount() {
        let config = VaultConfig::new("https://vault.example.com:8200", "token")
            .with_mount_path("kv-v2");
        let provider = VaultSecretProvider::new(config).unwrap();

        assert_eq!(
            provider.build_url("app/db-password", "data"),
            "https://vault.example.com:8200/v1/kv-v2/data/app/db-password"
        );
    }

    #[test]
    fn test_provider_name() {
        let config = VaultConfig::new("https://vault.example.com:8200", "token");
        let provider = VaultSecretProvider::new(config).unwrap();
        assert_eq!(provider.provider_name(), "vault");
    }
}
