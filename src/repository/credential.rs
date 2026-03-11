//! Neo4j repository operations for API credentials.

use uuid::Uuid;

use crate::models::ApiCredential;

use super::client::Neo4jClient;
use super::error::{RepositoryError, Result};

impl Neo4jClient {
    /// Create or update an API credential in the database.
    pub async fn create_api_credential(&self, credential: &ApiCredential) -> Result<()> {
        let query = r#"
            MERGE (c:ApiCredential {api_name: $api_name})
            SET c.id = $id,
                c.credential_type = $credential_type,
                c.inject_location = $inject_location,
                c.inject_key = $inject_key,
                c.secret_ref = $secret_ref,
                c.description = $description,
                c.active = $active,
                c.created_at = coalesce(c.created_at, $created_at),
                c.updated_at = $updated_at
        "#;

        self.graph()
            .run(
                neo4rs::query(query)
                    .param("id", credential.id.to_string())
                    .param("api_name", credential.api_name.to_lowercase())
                    .param("credential_type", credential.credential_type.to_string())
                    .param("inject_location", credential.inject_location.to_string())
                    .param("inject_key", credential.inject_key.clone())
                    .param("secret_ref", credential.secret_ref.clone())
                    .param(
                        "description",
                        credential.description.clone().unwrap_or_default(),
                    )
                    .param("active", credential.active)
                    .param("created_at", credential.created_at.to_rfc3339())
                    .param("updated_at", credential.updated_at.to_rfc3339()),
            )
            .await?;

        Ok(())
    }

    /// Get an API credential by API name.
    pub async fn get_api_credential(&self, api_name: &str) -> Result<ApiCredential> {
        let query = r#"
            MATCH (c:ApiCredential {api_name: $api_name})
            RETURN c
        "#;

        let mut result = self
            .graph()
            .execute(neo4rs::query(query).param("api_name", api_name.to_lowercase()))
            .await?;

        if let Some(row) = result.next().await? {
            let node: neo4rs::Node = row
                .get("c")
                .map_err(|e| RepositoryError::InvalidData(format!("Failed to get node: {}", e)))?;
            Self::node_to_api_credential(&node)
        } else {
            Err(RepositoryError::NotFound {
                entity: "ApiCredential",
                id: api_name.to_string(),
            })
        }
    }

    /// List all API credentials.
    pub async fn list_api_credentials(&self) -> Result<Vec<ApiCredential>> {
        let query = r#"
            MATCH (c:ApiCredential)
            RETURN c
            ORDER BY c.api_name
        "#;

        let mut result = self.graph().execute(neo4rs::query(query)).await?;
        let mut credentials = Vec::new();

        while let Some(row) = result.next().await? {
            let node: neo4rs::Node = row
                .get("c")
                .map_err(|e| RepositoryError::InvalidData(format!("Failed to get node: {}", e)))?;
            credentials.push(Self::node_to_api_credential(&node)?);
        }

        Ok(credentials)
    }

    /// Delete an API credential by API name.
    pub async fn delete_api_credential(&self, api_name: &str) -> Result<()> {
        let query = r#"
            MATCH (c:ApiCredential {api_name: $api_name})
            DELETE c
        "#;

        self.graph()
            .run(neo4rs::query(query).param("api_name", api_name.to_lowercase()))
            .await?;

        Ok(())
    }

    /// Check if an API credential exists.
    pub async fn api_credential_exists(&self, api_name: &str) -> Result<bool> {
        let query = r#"
            MATCH (c:ApiCredential {api_name: $api_name})
            RETURN count(c) > 0 AS exists
        "#;

        let mut result = self
            .graph()
            .execute(neo4rs::query(query).param("api_name", api_name.to_lowercase()))
            .await?;

        if let Some(row) = result.next().await? {
            let exists: bool = row.get("exists").map_err(|e| {
                RepositoryError::InvalidData(format!("Failed to get exists: {}", e))
            })?;
            Ok(exists)
        } else {
            Ok(false)
        }
    }

    /// Convert a Neo4j node to an ApiCredential.
    fn node_to_api_credential(node: &neo4rs::Node) -> Result<ApiCredential> {
        let id: String = node
            .get("id")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing id: {}", e)))?;
        let credential_type: String = node
            .get("credential_type")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing credential_type: {}", e)))?;
        let inject_location: String = node
            .get("inject_location")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing inject_location: {}", e)))?;
        let description: String = node.get("description").unwrap_or_default();
        let created_at: String = node
            .get("created_at")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing created_at: {}", e)))?;
        let updated_at: String = node
            .get("updated_at")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing updated_at: {}", e)))?;
        let api_name: String = node
            .get("api_name")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing api_name: {}", e)))?;
        let inject_key: String = node
            .get("inject_key")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing inject_key: {}", e)))?;
        let secret_ref: String = node
            .get("secret_ref")
            .map_err(|e| RepositoryError::InvalidData(format!("Missing secret_ref: {}", e)))?;

        Ok(ApiCredential {
            id: Uuid::parse_str(&id)
                .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
            api_name,
            credential_type: credential_type.parse().map_err(|e: String| {
                RepositoryError::InvalidData(format!("Invalid credential type: {}", e))
            })?,
            inject_location: inject_location.parse().map_err(|e: String| {
                RepositoryError::InvalidData(format!("Invalid inject location: {}", e))
            })?,
            inject_key,
            secret_ref,
            description: if description.is_empty() {
                None
            } else {
                Some(description)
            },
            active: node.get("active").unwrap_or(true),
            created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
                .map_err(|e| RepositoryError::InvalidData(format!("Invalid created_at: {}", e)))?
                .with_timezone(&chrono::Utc),
            updated_at: chrono::DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|e| RepositoryError::InvalidData(format!("Invalid updated_at: {}", e)))?
                .with_timezone(&chrono::Utc),
        })
    }
}
