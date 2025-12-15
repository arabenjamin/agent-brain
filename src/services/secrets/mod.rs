//! Secret management module.
//!
//! This module provides a provider abstraction for secure secret storage,
//! with implementations for local encrypted storage, HashiCorp Vault,
//! and AWS Secrets Manager.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │              SecretProvider (trait)                  │
//! └──────────┬──────────────┬──────────────┬────────────┘
//!            │              │              │
//!    LocalProvider    VaultProvider   AwsProvider
//!    (AES-GCM)       (KV v2)         (Secrets Mgr)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use agent_api::services::secrets::{LocalSecretConfig, LocalSecretProvider, SecretProvider};
//!
//! let config = LocalSecretConfig::new(".secrets.enc")
//!     .with_encryption_key("my-secret-key");
//! let provider = LocalSecretProvider::new(config)?;
//!
//! // Store a secret
//! provider.set_secret("api/openweathermap", "my-api-key").await?;
//!
//! // Retrieve the secret
//! let key = provider.get_secret("api/openweathermap").await?;
//! ```

mod aws;
mod error;
mod local;
mod manager;
mod provider;
mod vault;

pub use aws::{AwsSecretConfig, AwsSecretProvider};
pub use error::{Result, SecretError};
pub use local::{LocalSecretConfig, LocalSecretProvider};
pub use manager::{CredentialManager, CredentialManagerConfig};
pub use provider::{BoxedSecretProvider, SecretProvider};
pub use vault::{VaultConfig, VaultSecretProvider};
