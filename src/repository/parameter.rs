use neo4rs::{Row, query};
use uuid::Uuid;

use super::{Neo4jClient, RepositoryError, Result};
use crate::models::{Parameter, ParameterLocation};

impl Neo4jClient {
    /// Create a new Parameter node.
    pub async fn create_parameter(&self, parameter: &Parameter) -> Result<()> {
        let q = query(
            "CREATE (p:Parameter {
                id: $id,
                name: $name,
                location: $location,
                required: $required,
                param_type: $param_type,
                description: $description
            })",
        )
        .param("id", parameter.id.to_string())
        .param("name", parameter.name.clone())
        .param("location", parameter.location.to_string())
        .param("required", parameter.required)
        .param(
            "param_type",
            parameter.param_type.clone().unwrap_or_default(),
        )
        .param(
            "description",
            parameter.description.clone().unwrap_or_default(),
        );

        self.graph().run(q).await?;
        Ok(())
    }

    /// Find a Parameter by ID.
    pub async fn get_parameter(&self, id: Uuid) -> Result<Parameter> {
        let q = query("MATCH (p:Parameter {id: $id}) RETURN p").param("id", id.to_string());

        let mut result = self.graph().execute(q).await?;

        if let Some(row) = result.next().await? {
            row_to_parameter(row)
        } else {
            Err(RepositoryError::NotFound {
                entity: "Parameter",
                id: id.to_string(),
            })
        }
    }

    /// Get all Parameters for an Endpoint.
    pub async fn get_parameters_for_endpoint(&self, endpoint_id: Uuid) -> Result<Vec<Parameter>> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id})-[:REQUIRES_PARAM]->(p:Parameter)
             RETURN p
             ORDER BY p.required DESC, p.name",
        )
        .param("endpoint_id", endpoint_id.to_string());

        let mut result = self.graph().execute(q).await?;
        let mut parameters = Vec::new();

        while let Some(row) = result.next().await? {
            parameters.push(row_to_parameter(row)?);
        }

        Ok(parameters)
    }

    /// Link an Endpoint to a Parameter.
    pub async fn link_endpoint_to_parameter(
        &self,
        endpoint_id: Uuid,
        parameter_id: Uuid,
    ) -> Result<()> {
        let q = query(
            "MATCH (e:Endpoint {id: $endpoint_id}), (p:Parameter {id: $parameter_id})
             MERGE (e)-[:REQUIRES_PARAM]->(p)",
        )
        .param("endpoint_id", endpoint_id.to_string())
        .param("parameter_id", parameter_id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }

    /// Update a parameter's name (used during healing).
    pub async fn update_parameter_name(&self, id: Uuid, new_name: &str) -> Result<()> {
        let q = query(
            "MATCH (p:Parameter {id: $id})
             SET p.name = $new_name, p.last_updated = datetime()",
        )
        .param("id", id.to_string())
        .param("new_name", new_name);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Update a parameter's type (used during healing).
    pub async fn update_parameter_type(&self, id: Uuid, new_type: &str) -> Result<()> {
        let q = query(
            "MATCH (p:Parameter {id: $id})
             SET p.param_type = $new_type, p.last_updated = datetime()",
        )
        .param("id", id.to_string())
        .param("new_type", new_type);

        self.graph().run(q).await?;
        Ok(())
    }

    /// Delete a Parameter and all its relationships.
    pub async fn delete_parameter(&self, id: Uuid) -> Result<()> {
        let q = query("MATCH (p:Parameter {id: $id}) DETACH DELETE p").param("id", id.to_string());

        self.graph().run(q).await?;
        Ok(())
    }
}

fn row_to_parameter(row: Row) -> Result<Parameter> {
    let node: neo4rs::Node = row.get("p").map_err(|e| {
        RepositoryError::InvalidData(format!("Failed to get parameter node: {}", e))
    })?;

    let id: String = node
        .get("id")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get id: {}", e)))?;
    let name: String = node
        .get("name")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get name: {}", e)))?;
    let location: String = node
        .get("location")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get location: {}", e)))?;
    let required: bool = node
        .get("required")
        .map_err(|e| RepositoryError::InvalidData(format!("Failed to get required: {}", e)))?;
    let param_type: String = node.get("param_type").unwrap_or_default();
    let description: String = node.get("description").unwrap_or_default();

    let location: ParameterLocation = serde_json::from_str(&format!("\"{}\"", location))
        .map_err(|e| RepositoryError::InvalidData(format!("Invalid location: {}", e)))?;

    Ok(Parameter {
        id: Uuid::parse_str(&id)
            .map_err(|e| RepositoryError::InvalidData(format!("Invalid UUID: {}", e)))?,
        name,
        location,
        required,
        param_type: if param_type.is_empty() {
            None
        } else {
            Some(param_type)
        },
        description: if description.is_empty() {
            None
        } else {
            Some(description)
        },
    })
}
