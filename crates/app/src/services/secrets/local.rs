//! Local file-based secret provider with AES-256-GCM encryption.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::error::{Result, SecretError};
use super::provider::SecretProvider;

/// Salt for key derivation.
const KEY_DERIVATION_SALT: &[u8] = b"agent-brain-secrets-v1";

/// Nonce size for AES-GCM (96 bits = 12 bytes).
const NONCE_SIZE: usize = 12;

/// Configuration for local secret storage.
#[derive(Debug, Clone)]
pub struct LocalSecretConfig {
    /// Path to the encrypted secrets file.
    pub file_path: PathBuf,
    /// Encryption key (will be derived using PBKDF2-like approach).
    pub encryption_key: Option<String>,
    /// Whether to auto-save on every write operation.
    pub auto_save: bool,
}

impl Default for LocalSecretConfig {
    fn default() -> Self {
        Self {
            file_path: PathBuf::from(".secrets.enc"),
            encryption_key: None,
            auto_save: true,
        }
    }
}

impl LocalSecretConfig {
    /// Create a new local secret config with the given file path.
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
            ..Default::default()
        }
    }

    /// Set the encryption key.
    pub fn with_encryption_key(mut self, key: impl Into<String>) -> Self {
        self.encryption_key = Some(key.into());
        self
    }

    /// Set whether to auto-save on writes.
    pub fn with_auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = auto_save;
        self
    }
}

/// Encrypted secrets file format.
#[derive(serde::Serialize, serde::Deserialize)]
struct EncryptedFile {
    /// Version for future format changes.
    version: u32,
    /// Random nonce used for encryption.
    nonce: Vec<u8>,
    /// Encrypted data (JSON map of path -> value).
    ciphertext: Vec<u8>,
}

/// Local file-based secret provider with AES-256-GCM encryption.
///
/// Secrets are stored in an encrypted JSON file on disk.
/// The encryption key is derived from a user-provided password/key.
pub struct LocalSecretProvider {
    config: LocalSecretConfig,
    secrets: Arc<RwLock<HashMap<String, String>>>,
    cipher: Aes256Gcm,
}

impl LocalSecretProvider {
    /// Create a new local secret provider.
    ///
    /// # Arguments
    /// * `config` - Configuration for the provider
    ///
    /// # Returns
    /// A new LocalSecretProvider instance, or an error if initialization fails.
    pub fn new(config: LocalSecretConfig) -> Result<Self> {
        let key_str = config
            .encryption_key
            .as_deref()
            .unwrap_or("default-dev-key-change-in-production");

        let key = Self::derive_key(key_str);
        let cipher = Aes256Gcm::new(&key);

        let provider = Self {
            config,
            secrets: Arc::new(RwLock::new(HashMap::new())),
            cipher,
        };

        Ok(provider)
    }

    /// Load secrets from the encrypted file.
    ///
    /// This should be called after construction to load existing secrets.
    pub async fn load(&self) -> Result<()> {
        if !self.config.file_path.exists() {
            debug!(path = ?self.config.file_path, "Secrets file does not exist, starting fresh");
            return Ok(());
        }

        let encrypted_data = tokio::fs::read(&self.config.file_path).await?;

        let encrypted_file: EncryptedFile =
            serde_json::from_slice(&encrypted_data).map_err(|e| {
                SecretError::Decryption(format!("Invalid encrypted file format: {}", e))
            })?;

        if encrypted_file.version != 1 {
            return Err(SecretError::Decryption(format!(
                "Unsupported file version: {}",
                encrypted_file.version
            )));
        }

        let nonce = Nonce::from_slice(&encrypted_file.nonce);
        let plaintext = self
            .cipher
            .decrypt(nonce, encrypted_file.ciphertext.as_ref())
            .map_err(|e| SecretError::Decryption(format!("Decryption failed: {}", e)))?;

        let secrets: HashMap<String, String> = serde_json::from_slice(&plaintext)?;

        let mut store = self.secrets.write().await;
        *store = secrets;

        info!(
            path = ?self.config.file_path,
            count = store.len(),
            "Loaded secrets from encrypted file"
        );

        Ok(())
    }

    /// Save secrets to the encrypted file.
    pub async fn save(&self) -> Result<()> {
        let store = self.secrets.read().await;
        let plaintext = serde_json::to_vec(&*store)?;

        // Generate a random nonce for each save
        let mut nonce_bytes = [0u8; NONCE_SIZE];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext.as_ref())
            .map_err(|e| SecretError::Encryption(format!("Encryption failed: {}", e)))?;

        let encrypted_file = EncryptedFile {
            version: 1,
            nonce: nonce_bytes.to_vec(),
            ciphertext,
        };

        let encrypted_data = serde_json::to_vec(&encrypted_file)?;

        // Write atomically by writing to a temp file first
        let temp_path = self.config.file_path.with_extension("tmp");
        tokio::fs::write(&temp_path, &encrypted_data).await?;
        tokio::fs::rename(&temp_path, &self.config.file_path).await?;

        debug!(
            path = ?self.config.file_path,
            count = store.len(),
            "Saved secrets to encrypted file"
        );

        Ok(())
    }

    /// Derive a 256-bit key from a password using a simple PBKDF2-like approach.
    ///
    /// Note: For production use, consider using the `ring` or `argon2` crate
    /// for proper key derivation.
    fn derive_key(password: &str) -> Key<Aes256Gcm> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Simple key derivation - iterate hash function
        let mut data = Vec::new();
        data.extend_from_slice(password.as_bytes());
        data.extend_from_slice(KEY_DERIVATION_SALT);

        // Multiple rounds of hashing
        for _ in 0..100_000 {
            let mut hasher = DefaultHasher::new();
            data.hash(&mut hasher);
            let hash = hasher.finish();
            data = hash.to_le_bytes().to_vec();
            data.extend_from_slice(KEY_DERIVATION_SALT);
        }

        // Expand to 32 bytes (256 bits)
        let mut key_bytes = [0u8; 32];
        let mut hasher = DefaultHasher::new();
        for i in 0..4 {
            data.push(i as u8);
            data.hash(&mut hasher);
            let hash = hasher.finish().to_le_bytes();
            key_bytes[i * 8..(i + 1) * 8].copy_from_slice(&hash);
        }

        *Key::<Aes256Gcm>::from_slice(&key_bytes)
    }
}

impl SecretProvider for LocalSecretProvider {
    fn get_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            let store = self.secrets.read().await;
            store
                .get(&path)
                .cloned()
                .ok_or_else(|| SecretError::NotFound(path))
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
            {
                let mut store = self.secrets.write().await;
                store.insert(path.clone(), value);
            }

            if self.config.auto_save {
                self.save().await?;
            }

            debug!(path = %path, "Secret stored");
            Ok(())
        })
    }

    fn delete_secret(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let path = path.to_string();
        Box::pin(async move {
            {
                let mut store = self.secrets.write().await;
                if store.remove(&path).is_none() {
                    return Err(SecretError::NotFound(path));
                }
            }

            if self.config.auto_save {
                self.save().await?;
            }

            debug!(path = %path, "Secret deleted");
            Ok(())
        })
    }

    fn list_secrets(
        &self,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let store = self.secrets.read().await;
            let keys: Vec<String> = store
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            Ok(keys)
        })
    }

    fn health_check(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move { Ok(true) })
    }

    fn provider_name(&self) -> &'static str {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_local_provider_basic_operations() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        let config = LocalSecretConfig::new(&file_path).with_encryption_key("test-key-123");

        let provider = LocalSecretProvider::new(config).unwrap();

        // Set a secret
        provider
            .set_secret("test/api_key", "secret123")
            .await
            .unwrap();

        // Get the secret
        let value = provider.get_secret("test/api_key").await.unwrap();
        assert_eq!(value, "secret123");

        // List secrets
        let keys = provider.list_secrets("test/").await.unwrap();
        assert!(keys.contains(&"test/api_key".to_string()));

        // Delete the secret
        provider.delete_secret("test/api_key").await.unwrap();
        assert!(provider.get_secret("test/api_key").await.is_err());
    }

    #[tokio::test]
    async fn test_local_provider_persistence() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        // Create provider and set secrets
        {
            let config =
                LocalSecretConfig::new(&file_path).with_encryption_key("persistence-test-key");
            let provider = LocalSecretProvider::new(config).unwrap();

            provider
                .set_secret("persistent/secret1", "value1")
                .await
                .unwrap();
            provider
                .set_secret("persistent/secret2", "value2")
                .await
                .unwrap();
        }

        // Create new provider and load secrets
        {
            let config =
                LocalSecretConfig::new(&file_path).with_encryption_key("persistence-test-key");
            let provider = LocalSecretProvider::new(config).unwrap();
            provider.load().await.unwrap();

            assert_eq!(
                provider.get_secret("persistent/secret1").await.unwrap(),
                "value1"
            );
            assert_eq!(
                provider.get_secret("persistent/secret2").await.unwrap(),
                "value2"
            );
        }
    }

    #[tokio::test]
    async fn test_local_provider_wrong_key() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        // Create with one key
        {
            let config = LocalSecretConfig::new(&file_path).with_encryption_key("correct-key");
            let provider = LocalSecretProvider::new(config).unwrap();
            provider.set_secret("test", "value").await.unwrap();
        }

        // Try to load with wrong key
        {
            let config = LocalSecretConfig::new(&file_path).with_encryption_key("wrong-key");
            let provider = LocalSecretProvider::new(config).unwrap();
            let result = provider.load().await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_local_provider_list_with_prefix() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        let config = LocalSecretConfig::new(&file_path).with_encryption_key("test-key");
        let provider = LocalSecretProvider::new(config).unwrap();

        provider.set_secret("api/weather/key", "w1").await.unwrap();
        provider.set_secret("api/weather/token", "w2").await.unwrap();
        provider.set_secret("api/maps/key", "m1").await.unwrap();
        provider.set_secret("db/password", "p1").await.unwrap();

        let weather_secrets = provider.list_secrets("api/weather/").await.unwrap();
        assert_eq!(weather_secrets.len(), 2);

        let api_secrets = provider.list_secrets("api/").await.unwrap();
        assert_eq!(api_secrets.len(), 3);

        let all_secrets = provider.list_secrets("").await.unwrap();
        assert_eq!(all_secrets.len(), 4);
    }

    #[tokio::test]
    async fn test_local_provider_health_check() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("secrets.enc");

        let config = LocalSecretConfig::new(&file_path);
        let provider = LocalSecretProvider::new(config).unwrap();

        assert!(provider.health_check().await.unwrap());
    }

    #[test]
    fn test_provider_name() {
        let config = LocalSecretConfig::default();
        let provider = LocalSecretProvider::new(config).unwrap();
        assert_eq!(provider.provider_name(), "local");
    }
}
