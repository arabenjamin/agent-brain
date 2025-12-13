use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("Neo4j error: {0}")]
    Neo4j(#[from] neo4rs::Error),

    #[error("Entity not found: {entity} with id {id}")]
    NotFound { entity: &'static str, id: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid data: {0}")]
    InvalidData(String),
}

pub type Result<T> = std::result::Result<T, RepositoryError>;
