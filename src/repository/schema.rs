use neo4rs::{Row, query};
use uuid::Uuid;

use super::{Neo4jClient, RepositoryError, Result};
use crate::models::Schema;

impl Neo4jClient {
    /// Create a new Schema node.
    pub async fn create_schema(&self, schema: &Schema) -> Result<()> {
        let q = query("CREATE (s:Schema {id: $id, name: $name, json_structure: $json_structure})")
            .param("id", schema.id.to_string())
            .param("name", schema.name.clone())
            .param("json_structure", schema.json_structure.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Find a Schema by ID.
    pub async fn get_schema(&self, id: Uuid) -> Result<Schema> {
        let q = query("MATCH (s:Schema {id: $id}) RETURN s").param("id", id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            row_to_schema(row)
        } else {
            Err(RepositoryError::NotFound {
                entity: "Schema",
                id: id.to_string(),
            })
        }
    }

    /// Find a Schema by name.
    pub async fn get_schema_by_name(&self, name: &str) -> Result<Option<Schema>> {
        let q = query("MATCH (s:Schema {name: $name}) RETURN s").param("name", name);

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            Ok(Some(row_to_schema(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get all Schemas.
    pub async fn list_schemas(&self) -> Result<Vec<Schema>> {
        let q = query("MATCH (s:Schema) RETURN s ORDER BY s.name");
        let mut result = self.graph().execute(q).await?;
        let mut schemas = Vec::new();

        while let Some(row) = result.next().await? {
            schemas.push(row_to_schema(row)?);
        }

        Ok(schemas)
    }

    /// Link an Endpoint to a response Schema.
    pub async fn link_endpoint_returns_schema(
        &self,
        endpoint_id: Uuid,
        schema_id: Uuid,
        status_code: u16,
    ) -> Result<()> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id}), (s:Schema {id: $schema_id})
             MERGE (e)-[:RETURNS_SCHEMA {status: $status}]->(s)",
        )
        .param("endpoint_id", endpoint_id.to_string())
        .param("schema_id", schema_id.to_string())
        .param("status", status_code as i64);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Link an Endpoint to a request body Schema.
    pub async fn link_endpoint_accepts_schema(
        &self,
        endpoint_id: Uuid,
        schema_id: Uuid,
    ) -> Result<()> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id}), (s:Schema {id: $schema_id})
             MERGE (e)-[:ACCEPTS_SCHEMA]->(s)",
        )
        .param("endpoint_id", endpoint_id.to_string())
        .param("schema_id", schema_id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Link one Schema to another (for nested objects).
    pub async fn link_schema_to_schema(&self, parent_id: Uuid, child_id: Uuid) -> Result<()> {
        let q = query(
            "MATCH (p:Schema {id: $parent_id}), (c:Schema {id: $child_id})
             MERGE (p)-[:LINKS_TO]->(c)",
        )
        .param("parent_id", parent_id.to_string())
        .param("child_id", child_id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Get the response Schema for an Endpoint by status code.
    pub async fn get_response_schema(
        &self,
        endpoint_id: Uuid,
        status_code: u16,
    ) -> Result<Option<Schema>> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id})-[:RETURNS_SCHEMA {status: $status}]->(s:Schema)
             RETURN s",
        )
        .param("endpoint_id", endpoint_id.to_string())
        .param("status", status_code as i64);

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            Ok(Some(row_to_schema(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get the request body Schema for an Endpoint.
    pub async fn get_request_schema(&self, endpoint_id: Uuid) -> Result<Option<Schema>> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id})-[:ACCEPTS_SCHEMA]->(s:Schema)
             RETURN s",
        )
        .param("endpoint_id", endpoint_id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            Ok(Some(row_to_schema(row)?))
        } else {
            Ok(None)
        }
    }

    /// Delete a Schema and all its relationships.
    pub async fn delete_schema(&self, id: Uuid) -> Result<()> {
        let q = query("MATCH (s:Schema {id: $id}) DETACH DELETE s").param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }
}

fn row_to_schema(row: Row) -> Result<Schema> {
    let node: neo4rs::Node = row
        .get("s")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get schema node: {}", e)))?;

    let id: String = node
        .get("id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get id: {}", e)))?;
    let name: String = node
        .get("name")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get name: {}", e)))?;
    let json_structure: String = node.get("json_structure").map_err(|e| {
        RepositoryError::InvalidData(format!("Failed to get json_structure: {}", e))
    })?;

    let json_structure: serde_json::Value = serde_json::from_str(&json_structure)?;

    Ok(Schema {
        id: Uuid::parse_str(&id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
        name,
        json_structure,
    })
}
