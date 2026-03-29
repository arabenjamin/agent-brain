//! Shared in-process resource registry for cross-agent state sharing.
//!
//! Agents running as concurrent background jobs can register named resources
//! (WebSocket connection IDs, auth tokens, API sessions, config handles, etc.)
//! that other agents look up by key.  TTL-based expiry is checked lazily on
//! every `get` call so callers never see stale resources without an explicit
//! `release`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ============================================================================
// Types
// ============================================================================

/// A single registered resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEntry {
    pub key: String,
    pub value: String,
    /// Logical type tag — e.g. `"ws_connection"`, `"auth_token"`, `"api_session"`.
    pub resource_type: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Optional time-to-live in seconds. `None` means the resource never expires.
    pub ttl_secs: Option<u64>,
    /// Arbitrary extra metadata stored alongside the resource value.
    pub metadata: Option<serde_json::Value>,
}

impl ResourceEntry {
    /// Returns `true` if the entry has not yet expired.
    pub fn is_alive(&self) -> bool {
        let Some(ttl) = self.ttl_secs else {
            return true;
        };
        let created: chrono::DateTime<Utc> = self.created_at.parse().unwrap_or_else(|_| Utc::now());
        Utc::now().signed_duration_since(created).num_seconds() < ttl as i64
    }
}

// ============================================================================
// Registry
// ============================================================================

/// In-process registry for named resources shared across concurrent agent jobs.
///
/// Wrap in `Arc` and hand the same instance to every skill / service that
/// needs cross-agent resource sharing.
#[derive(Debug, Clone)]
pub struct ResourceRegistry {
    entries: Arc<RwLock<HashMap<String, ResourceEntry>>>,
}

impl Default for ResourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a resource, overwriting any existing entry with the same key.
    pub async fn register(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
        resource_type: impl Into<String>,
        ttl_secs: Option<u64>,
        metadata: Option<serde_json::Value>,
    ) {
        let key = key.into();
        let entry = ResourceEntry {
            key: key.clone(),
            value: value.into(),
            resource_type: resource_type.into(),
            created_at: Utc::now().to_rfc3339(),
            ttl_secs,
            metadata,
        };
        self.entries.write().await.insert(key, entry);
    }

    /// Retrieve a resource by key.
    ///
    /// Returns `None` if the key is not found **or** the entry has expired.
    /// Expired entries are removed lazily on this call.
    pub async fn get(&self, key: &str) -> Option<ResourceEntry> {
        let entry = self.entries.read().await.get(key).cloned()?;
        if entry.is_alive() {
            Some(entry)
        } else {
            self.entries.write().await.remove(key);
            None
        }
    }

    /// Release (delete) a resource. Returns `true` if the key existed.
    pub async fn release(&self, key: &str) -> bool {
        self.entries.write().await.remove(key).is_some()
    }

    /// List all live resources, optionally filtered by `resource_type`.
    /// Expired entries are pruned during the scan.
    pub async fn list(&self, resource_type: Option<&str>) -> Vec<ResourceEntry> {
        let mut map = self.entries.write().await;
        // Prune expired entries in one pass.
        map.retain(|_, e| e.is_alive());
        let mut entries: Vec<ResourceEntry> = map
            .values()
            .filter(|e| resource_type.is_none_or(|t| e.resource_type == t))
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries
    }
}
