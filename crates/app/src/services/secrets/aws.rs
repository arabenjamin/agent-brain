//! AWS Secrets Manager secret provider.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aws_sdk_secretsmanager::Client as SecretsManagerClient;
use tokio::sync::OnceCell;
use tracing::{debug, info};

use super::error::{Result, SecretError};
use super::provider::SecretProvider;

/// Configuration for AWS Secrets Manager.
#[derive(Debug, Clone, Default)]
pub struct AwsSecretConfig {
    /// AWS region (e.g., "us-east-1").
    pub region: Option<String>,
    /// Prefix for all secret names (e.g., "/agent-brain/").
    pub prefix: String,
}

impl AwsSecretConfig {
    /// Create a new AWS Secrets Manager configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the AWS region.
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the secret name prefix.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }
}

/// AWS Secrets Manager secret provider.
///
/// This provider uses the AWS SDK to interact with AWS Secrets Manager.
/// Authentication is handled via the standard AWS credential chain
/// (environment variables, IAM roles, config files, etc.).
pub struct AwsSecretProvider {
    config: AwsSecretConfig,
    client: Arc<OnceCell<SecretsManagerClient>>,
    aws_config: Arc<OnceCell<aws_config::SdkConfig>>,
}

impl AwsSecretProvider {
    /// Create a new AWS Secrets Manager provider.
    ///
    /// Note: The AWS client is lazily initialized on first use to allow
    /// for async credential loading.
    pub fn new(config: AwsSecretConfig) -> Self {
        Self {
            config,
            client: Arc::new(OnceCell::new()),
            aws_config: Arc::new(OnceCell::new()),
        }
    }

    /// Get or initialize the AWS SDK config.
    async fn get_aws_config(&self) -> &aws_config::SdkConfig {
        self.aws_config
            .get_or_init(|| async {
                let mut config_loader = aws_config::from_env();

                if let Some(region) = &self.config.region {
                    config_loader = config_loader.region(aws_config::Region::new(region.clone()));
                }

                config_loader.load().await
            })
            .await
    }

    /// Get or initialize the Secrets Manager client.
    async fn get_client(&self) -> &SecretsManagerClient {
        self.client
            .get_or_init(|| async {
                let aws_config = self.get_aws_config().await;
                SecretsManagerClient::new(aws_config)
            })
            .await
    }

    /// Build the full secret name with prefix.
    fn build_secret_name(&self, path: &str) -> String {
        if self.config.prefix.is_empty() {
            path.to_string()
        } else {
            format!(
                "{}/{}",
                self.config.prefix.trim_end_matches('/'),
                path.trim_start_matches('/')
            )
        }
    }

    /// Remove the prefix from a secret name.
    fn strip_prefix(&self, name: &str) -> String {
        if self.config.prefix.is_empty() {
            name.to_string()
        } else {
            let prefix = format!("{}/", self.config.prefix.trim_end_matches('/'));
            name.strip_prefix(&prefix).unwrap_or(name).to_string()
        }
    }

    /// Map AWS error strings to SecretError variants.
    fn map_aws_error(path: &str, err: String) -> SecretError {
        if err.contains("ResourceNotFoundException") || err.contains("not found") {
            SecretError::NotFound(path.to_string())
        } else if err.contains("AccessDeniedException") || err.contains("access denied") {
            SecretError::AccessDenied(path.to_string())
        } else {
            SecretError::Aws(err)
        }
    }
}

impl SecretProvider for AwsSecretProvider {
    fn get_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            let client = self.get_client().await;
            let secret_name = self.build_secret_name(&path);

            debug!(secret_name = %secret_name, "Getting secret from AWS Secrets Manager");

            let result = client
                .get_secret_value()
                .secret_id(&secret_name)
                .send()
                .await
                .map_err(|e| Self::map_aws_error(&path, e.to_string()))?;

            result
                .secret_string()
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    SecretError::Aws(format!(
                        "Secret {} exists but has no string value (might be binary)",
                        path
                    ))
                })
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
            let client = self.get_client().await;
            let secret_name = self.build_secret_name(&path);

            debug!(secret_name = %secret_name, "Setting secret in AWS Secrets Manager");

            // Try to update first, create if it doesn't exist
            let update_result = client
                .put_secret_value()
                .secret_id(&secret_name)
                .secret_string(&value)
                .send()
                .await;

            match update_result {
                Ok(_) => {
                    info!(path = %path, "Secret updated in AWS Secrets Manager");
                    Ok(())
                }
                Err(e) => {
                    let err_str = e.to_string();
                    // Check if it's a not found error - need to create
                    if err_str.contains("ResourceNotFoundException")
                        || err_str.contains("not found")
                    {
                        // Secret doesn't exist, create it
                        client
                            .create_secret()
                            .name(&secret_name)
                            .secret_string(&value)
                            .send()
                            .await
                            .map_err(|e| SecretError::Aws(e.to_string()))?;

                        info!(path = %path, "Secret created in AWS Secrets Manager");
                        Ok(())
                    } else {
                        Err(Self::map_aws_error(&path, err_str))
                    }
                }
            }
        })
    }

    fn delete_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            let client = self.get_client().await;
            let secret_name = self.build_secret_name(&path);

            debug!(secret_name = %secret_name, "Deleting secret from AWS Secrets Manager");

            client
                .delete_secret()
                .secret_id(&secret_name)
                .force_delete_without_recovery(true)
                .send()
                .await
                .map_err(|e| Self::map_aws_error(&path, e.to_string()))?;

            info!(path = %path, "Secret deleted from AWS Secrets Manager");
            Ok(())
        })
    }

    fn list_secrets(
        &self,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let client = self.get_client().await;
            let full_prefix = self.build_secret_name(&prefix);

            debug!(prefix = %full_prefix, "Listing secrets from AWS Secrets Manager");

            let mut secrets = Vec::new();
            let mut next_token: Option<String> = None;

            loop {
                let mut request = client.list_secrets();

                // Filter by prefix
                if !full_prefix.is_empty() {
                    request = request.filters(
                        aws_sdk_secretsmanager::types::Filter::builder()
                            .key(aws_sdk_secretsmanager::types::FilterNameStringType::Name)
                            .values(&full_prefix)
                            .build(),
                    );
                }

                if let Some(token) = &next_token {
                    request = request.next_token(token);
                }

                let response = request
                    .send()
                    .await
                    .map_err(|e| SecretError::Aws(e.to_string()))?;

                for secret in response.secret_list() {
                    if let Some(name) = secret.name() {
                        // Only include secrets that start with our prefix
                        if name.starts_with(&full_prefix) {
                            secrets.push(self.strip_prefix(name));
                        }
                    }
                }

                next_token = response.next_token().map(|s| s.to_string());
                if next_token.is_none() {
                    break;
                }
            }

            Ok(secrets)
        })
    }

    fn health_check(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let client = self.get_client().await;

            // Try to list secrets with max results of 1 to verify connectivity
            let result = client.list_secrets().max_results(1).send().await;

            Ok(result.is_ok())
        })
    }

    fn provider_name(&self) -> &'static str {
        "aws"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aws_config_defaults() {
        let config = AwsSecretConfig::new();
        assert!(config.region.is_none());
        assert!(config.prefix.is_empty());
    }

    #[test]
    fn test_aws_config_builder() {
        let config = AwsSecretConfig::new()
            .with_region("us-west-2")
            .with_prefix("/agent-brain");

        assert_eq!(config.region, Some("us-west-2".to_string()));
        assert_eq!(config.prefix, "/agent-brain");
    }

    #[test]
    fn test_build_secret_name() {
        let config = AwsSecretConfig::new().with_prefix("/agent-brain");
        let provider = AwsSecretProvider::new(config);

        assert_eq!(
            provider.build_secret_name("openweathermap/key"),
            "/agent-brain/openweathermap/key"
        );
    }

    #[test]
    fn test_build_secret_name_no_prefix() {
        let config = AwsSecretConfig::new();
        let provider = AwsSecretProvider::new(config);

        assert_eq!(
            provider.build_secret_name("openweathermap/key"),
            "openweathermap/key"
        );
    }

    #[test]
    fn test_strip_prefix() {
        let config = AwsSecretConfig::new().with_prefix("/agent-brain");
        let provider = AwsSecretProvider::new(config);

        assert_eq!(
            provider.strip_prefix("/agent-brain/openweathermap/key"),
            "openweathermap/key"
        );
    }

    #[test]
    fn test_provider_name() {
        let config = AwsSecretConfig::new();
        let provider = AwsSecretProvider::new(config);
        assert_eq!(provider.provider_name(), "aws");
    }
}
