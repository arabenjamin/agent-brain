//! Secret management module.
//!
//! This module provides a provider abstraction for secure secret storage,
//! with implementations for local encrypted storage, HashiCorp Vault,
//! and AWS Secrets Manager.

#[cfg(feature = "aws")]
mod aws;
mod error;
mod local;
mod provider;
mod vault;

#[cfg(feature = "aws")]
pub use aws::{AwsSecretConfig, AwsSecretProvider};
pub use error::{Result, SecretError};
pub use local::{LocalSecretConfig, LocalSecretProvider};
pub use provider::{BoxedSecretProvider, SecretProvider};
pub use vault::{VaultConfig, VaultSecretProvider};
