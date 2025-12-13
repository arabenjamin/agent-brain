use neo4rs::{Row, query};
use uuid::Uuid;

use super::{Neo4jClient, RepositoryError, Result};
use crate::models::Resource;

impl Neo4jClient {
    /// Create a new Resource node.
    pub async fn create_resource(&self, resource: &Resource) -> Result<()> {
        let q = query("CREATE (r:Resource {id: $id, name: $name, description: $description})")
            .param("id", resource.id.to_string())
            .param("name", resource.name.clone())
            .param("description", resource.description.clone());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Find a Resource by ID.
    pub async fn get_resource(&self, id: Uuid) -> Result<Resource> {
        let q = query("MATCH (r:Resource {id: $id}) RETURN r").param("id", id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            row_to_resource(row)
        } else {
            Err(RepositoryError::NotFound {
                entity: "Resource",
                id: id.to_string(),
            })
        }
    }

    /// Find a Resource by name.
    pub async fn get_resource_by_name(&self, name: &str) -> Result<Option<Resource>> {
        let q = query("MATCH (r:Resource {name: $name}) RETURN r").param("name", name);

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            Ok(Some(row_to_resource(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get all Resources.
    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        let q = query("MATCH (r:Resource) RETURN r ORDER BY r.name");
        let mut result = self.graph().execute(q).await?;
        let mut resources = Vec::new();

        while let Some(row) = result.next().await? {
            resources.push(row_to_resource(row)?);
        }

        Ok(resources)
    }

    /// Delete a Resource and all its relationships.
    pub async fn delete_resource(&self, id: Uuid) -> Result<()> {
        let q = query("MATCH (r:Resource {id: $id}) DETACH DELETE r").param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Link a Resource to an Endpoint.
    pub async fn link_resource_to_endpoint(
        &self,
        resource_id: Uuid,
        endpoint_id: Uuid,
    ) -> Result<()> {
        let q = query(
            "MATCH (r:Resource {id: $resource_id}), (e:Endpoint {id: $endpoint_id})
             MERGE (r)-[:HAS_ENDPOINT]->(e)",
        )
        .param("resource_id", resource_id.to_string())
        .param("endpoint_id", endpoint_id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }
}

fn row_to_resource(row: Row) -> Result<Resource> {
    let node: neo4rs::Node = row
        .get("r")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get resource node: {}", e)))?;

    let id: String = node
        .get("id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get id: {}", e)))?;
    let name: String = node
        .get("name")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get name: {}", e)))?;
    let description: String = node
        .get("description")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get description: {}", e)))?;

    Ok(Resource {
        id: Uuid::parse_str(&id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
        name,
        description,
    })
}
