//! Error types for secret management.

use thiserror::Error;

/// Errors that can occur during secret operations.
#[derive(Debug, Error)]
pub enum SecretError {
    /// Secret was not found at the given path.
    #[error("Secret not found: {0}")]
    NotFound(String),

    /// Access to the secret was denied.
    #[error("Access denied to secret: {0}")]
    AccessDenied(String),

    /// Error during encryption.
    #[error("Encryption error: {0}")]
    Encryption(String),

    /// Error during decryption.
    #[error("Decryption error: {0}")]
    Decryption(String),

    /// File I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// HTTP request error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// HashiCorp Vault error.
    #[error("Vault error: {status} - {message}")]
    Vault {
        /// HTTP status code from Vault.
        status: u16,
        /// Error message from Vault.
        message: String,
    },

    /// AWS Secrets Manager error.
    #[error("AWS error: {0}")]
    Aws(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Repository/database error.
    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),

    /// Credential not found for the given API.
    #[error("Credential not found for API: {0}")]
    CredentialNotFound(String),

    /// Invalid credential type.
    #[error("Invalid credential type: {0}")]
    InvalidCredentialType(String),

    /// Provider not configured.
    #[error("Secret provider not configured")]
    ProviderNotConfigured,
}

/// Result type alias for secret operations.
pub type Result<T> = std::result::Result<T, SecretError>;
