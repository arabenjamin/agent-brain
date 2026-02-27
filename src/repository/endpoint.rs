use neo4rs::{Row, query};
use uuid::Uuid;

use super::{Neo4jClient, RepositoryError, Result};
use crate::models::{Endpoint, EndpointStatus, HttpMethod};

impl Neo4jClient {
    /// Create a new Endpoint node.
    pub async fn create_endpoint(&self, endpoint: &Endpoint) -> Result<()> {
        let q = if let Some(ref emb) = endpoint.embedding {
            query(
                "CREATE (e:Endpoint {
                    id: $id,
                    path: $path,
                    method: $method,
                    summary: $summary,
                    operation_id: $operation_id,
                    status: $status,
                    last_verified_status: $last_verified_status,
                    healed_by_ai: $healed_by_ai,
                    embedding: $embedding
                })",
            )
            .param("embedding", emb.clone())
        } else {
            query(
                "CREATE (e:Endpoint {
                    id: $id,
                    path: $path,
                    method: $method,
                    summary: $summary,
                    operation_id: $operation_id,
                    status: $status,
                    last_verified_status: $last_verified_status,
                    healed_by_ai: $healed_by_ai
                })",
            )
        }
        .param("id", endpoint.id.to_string())
        .param("path", endpoint.path.clone())
        .param("method", endpoint.method.to_string())
        .param("summary", endpoint.summary.clone())
        .param(
            "operation_id",
            endpoint.operation_id.clone().unwrap_or_default(),
        )
        .param("status", serde_json::to_string(&endpoint.status)?)
        .param(
            "last_verified_status",
            endpoint.last_verified_status.map(|s| s as i64),
        )
        .param("healed_by_ai", endpoint.healed_by_ai);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Update the embedding for an endpoint.
    pub async fn update_endpoint_embedding(&self, id: Uuid, embedding: Vec<f32>) -> Result<()> {
        let q = query("MATCH (e:Endpoint {id: $id}) SET e.embedding = $embedding")
            .param("id", id.to_string())
            .param("embedding", embedding);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Find an Endpoint by ID.
    pub async fn get_endpoint(&self, id: Uuid) -> Result<Endpoint> {
        let q = query("MATCH (e:Endpoint {id: $id}) RETURN e").param("id", id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            row_to_endpoint(row)
        } else {
            Err(RepositoryError::NotFound {
                entity: "Endpoint",
                id: id.to_string(),
            })
        }
    }

    /// Find endpoints by path pattern (fuzzy match).
    pub async fn find_endpoints_by_path(&self, pattern: &str) -> Result<Vec<Endpoint>> {
        let q = query(
            "MATCH (e:Endpoint)
             WHERE e.path CONTAINS $pattern OR e.summary CONTAINS $pattern
             RETURN e
             ORDER BY e.path",
        )
        .param("pattern", pattern);

        let mut result = self.graph().execute(q).await?;
        let mut endpoints = Vec::new();

        while let Some(row) = result.next().await? {
            endpoints.push(row_to_endpoint(row)?);
        }

        Ok(endpoints)
    }

    /// Find endpoint by exact path and method.
    pub async fn get_endpoint_by_path_method(
        &self,
        path: &str,
        method: HttpMethod,
    ) -> Result<Option<Endpoint>> {
        let q = query("MATCH (e:Endpoint {path: $path, method: $method}) RETURN e")
            .param("path", path)
            .param("method", method.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            Ok(Some(row_to_endpoint(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get all Endpoints.
    pub async fn list_endpoints(&self) -> Result<Vec<Endpoint>> {
        let q = query("MATCH (e:Endpoint) RETURN e ORDER BY e.path, e.method");
        let mut result = self.graph().execute(q).await?;
        let mut endpoints = Vec::new();

        while let Some(row) = result.next().await? {
            endpoints.push(row_to_endpoint(row)?);
        }

        Ok(endpoints)
    }

    /// Find endpoints using semantic search (vector similarity).
    pub async fn find_endpoints_semantic(
        &self,
        embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<Endpoint>> {
        let q = query(
            "CALL db.index.vector.queryNodes('endpoint_embeddings', $limit, $embedding)
             YIELD node AS e, score
             RETURN e, score
             ORDER BY score DESC",
        )
        .param("limit", limit as i64)
        .param("embedding", embedding);

        let mut result = self.graph().execute(q).await?;
        let mut endpoints = Vec::new();

        while let Some(row) = result.next().await? {
            endpoints.push(row_to_endpoint(row)?);
        }

        Ok(endpoints)
    }

    /// Get all Endpoints for a Resource.
    pub async fn get_endpoints_for_resource(&self, resource_id: Uuid) -> Result<Vec<Endpoint>> {
        let q = query(
            "MATCH (r:Resource {id: $resource_id})-[:HAS_ENDPOINT]->(e:Endpoint)
             RETURN e
             ORDER BY e.path, e.method",
        )
        .param("resource_id", resource_id.to_string());

        let mut result = self.graph().execute(q).await?;
        let mut endpoints = Vec::new();

        while let Some(row) = result.next().await? {
            endpoints.push(row_to_endpoint(row)?);
        }

        Ok(endpoints)
    }

    /// Update endpoint status after verification.
    pub async fn update_endpoint_status(
        &self,
        id: Uuid,
        status: EndpointStatus,
        http_status: Option<u16>,
    ) -> Result<()> {
        let q = query(
            "MATCH (e:Endpoint {id: $id})
             SET e.status = $status, e.last_verified_status = $http_status",
        )
        .param("id", id.to_string())
        .param("status", serde_json::to_string(&status)?)
        .param("http_status", http_status.map(|s| s as i64));

        self.graph().run(q).await?;
        Ok(())
    }

    /// Mark endpoint as healed by AI.
    pub async fn mark_endpoint_healed(&self, id: Uuid) -> Result<()> {
        let q = query(
            "MATCH (e:Endpoint {id: $id})
             SET e.healed_by_ai = true",
        )
        .param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Delete an Endpoint and all its relationships.
    pub async fn delete_endpoint(&self, id: Uuid) -> Result<()> {
        let q = query("MATCH (e:Endpoint {id: $id}) DETACH DELETE e").param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }
}

fn row_to_endpoint(row: Row) -> Result<Endpoint> {
    let node: neo4rs::Node = row
        .get("e")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get endpoint node: {}", e)))?;

    let id: String = node
        .get("id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get id: {}", e)))?;
    let path: String = node
        .get("path")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get path: {}", e)))?;
    let method: String = node
        .get("method")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get method: {}", e)))?;
    let summary: String = node
        .get("summary")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get summary: {}", e)))?;
    let operation_id: String = node.get("operation_id").unwrap_or_default();
    let status: String = node
        .get("status")
        .unwrap_or_else(|_| "\"unknown\"".to_string());
    let status_code: Option<i64> = node.get("last_verified_status").ok();
    let healed_by_ai: bool = node.get("healed_by_ai").unwrap_or(false);
    let embedding: Option<Vec<f32>> = node.get("embedding").ok();

    let method: HttpMethod = serde_json::from_str(&format!("\"{}\"", method))
        .map_err(|e| RepositoryError::InvalidData(format!("Invalid method: {}", e)))?;

    let status: EndpointStatus = serde_json::from_str(&status).unwrap_or(EndpointStatus::Unknown);

    Ok(Endpoint {
        id: Uuid::parse_str(&id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
        path,
        method,
        summary,
        operation_id: if operation_id.is_empty() {
            None
        } else {
            Some(operation_id)
        },
        status,
        last_verified_status: status_code.map(|s| s as u16),
        healed_by_ai,
        embedding,
    })
}
