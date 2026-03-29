//! Secret provider trait definition.

use std::future::Future;
use std::pin::Pin;

use super::error::Result;

/// Trait for secret storage providers.
///
/// Implementations should be Send + Sync for use across async contexts.
/// Each method returns a pinned boxed future to allow for async operations
/// without requiring the async_trait macro.
///
/// # Example
///
/// ```ignore
/// let provider = LocalSecretProvider::new(config)?;
/// provider.set_secret("api/key", "secret123").await?;
/// let value = provider.get_secret("api/key").await?;
/// ```
pub trait SecretProvider: Send + Sync {
    /// Get a secret value by path.
    ///
    /// # Arguments
    /// * `path` - The path/key of the secret (e.g., "openweathermap/api_key")
    ///
    /// # Returns
    /// The secret value as a string, or an error if not found.
    fn get_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;

    /// Store a secret at the given path.
    ///
    /// # Arguments
    /// * `path` - The path/key to store the secret at
    /// * `value` - The secret value to store
    ///
    /// # Returns
    /// Ok(()) on success, or an error if the operation failed.
    fn set_secret(
        &self,
        path: &str,
        value: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Delete a secret at the given path.
    ///
    /// # Arguments
    /// * `path` - The path/key of the secret to delete
    ///
    /// # Returns
    /// Ok(()) on success, or NotFound error if the secret doesn't exist.
    fn delete_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// List all secrets under a given prefix.
    ///
    /// # Arguments
    /// * `prefix` - The prefix to filter secrets by (e.g., "openweathermap/")
    ///
    /// # Returns
    /// A list of secret paths matching the prefix.
    fn list_secrets(
        &self,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + '_>>;

    /// Check if the provider is healthy and reachable.
    ///
    /// # Returns
    /// true if the provider is healthy, false otherwise.
    fn health_check(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>>;

    /// Get the provider name for logging/debugging.
    fn provider_name(&self) -> &'static str;
}

/// Type alias for a boxed secret provider.
pub type BoxedSecretProvider = Box<dyn SecretProvider>;
