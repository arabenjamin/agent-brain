mod common;

use agent_api::models::{Endpoint, HttpMethod, Parameter, ParameterLocation, Resource, Schema};
use agent_api::repository::Neo4jClient;
use common::{init_test_env, neo4j_test_config};

async fn setup_client() -> Neo4jClient {
    init_test_env();
    let (uri, user, password) = neo4j_test_config();
    let client = Neo4jClient::new(&uri, &user, &password)
        .await
        .expect("Failed to connect to Neo4j");
    client.init_schema().await.expect("Failed to init schema");
    client
}

#[tokio::test]
async fn test_resource_crud() {
    let client = setup_client().await;

    // Create
    let resource = Resource::new("Users", "User management API");
    client
        .create_resource(&resource)
        .await
        .expect("Failed to create resource");

    // Read
    let fetched = client
        .get_resource(resource.id)
        .await
        .expect("Failed to get resource");
    assert_eq!(fetched.name, "Users");
    assert_eq!(fetched.description, "User management API");

    // List
    let resources = client
        .list_resources()
        .await
        .expect("Failed to list resources");
    assert!(resources.iter().any(|r| r.id == resource.id));

    // Delete
    client
        .delete_resource(resource.id)
        .await
        .expect("Failed to delete resource");

    // Verify deleted
    let result = client.get_resource(resource.id).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_endpoint_crud() {
    let client = setup_client().await;

    // Create
    let endpoint = Endpoint::new(
        "/users/{id}",
        HttpMethod::Get,
        "Get user by ID",
        Some("getUser".to_string()),
    );
    client
        .create_endpoint(&endpoint)
        .await
        .expect("Failed to create endpoint");

    // Read
    let fetched = client
        .get_endpoint(endpoint.id)
        .await
        .expect("Failed to get endpoint");
    assert_eq!(fetched.path, "/users/{id}");
    assert_eq!(fetched.method, HttpMethod::Get);

    // Find by path
    let found = client
        .find_endpoints_by_path("users")
        .await
        .expect("Failed to find endpoints");
    assert!(found.iter().any(|e| e.id == endpoint.id));

    // Delete
    client
        .delete_endpoint(endpoint.id)
        .await
        .expect("Failed to delete endpoint");
}

#[tokio::test]
async fn test_parameter_crud() {
    let client = setup_client().await;

    // Create endpoint first
    let endpoint = Endpoint::new("/test", HttpMethod::Post, "Test endpoint", None);
    client.create_endpoint(&endpoint).await.unwrap();

    // Create parameter
    let param = Parameter::new("userId", ParameterLocation::Path, true)
        .with_type("string")
        .with_description("The user identifier");
    client.create_parameter(&param).await.unwrap();

    // Link to endpoint
    client
        .link_endpoint_to_parameter(endpoint.id, param.id)
        .await
        .unwrap();

    // Get parameters for endpoint
    let params = client
        .get_parameters_for_endpoint(endpoint.id)
        .await
        .unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name, "userId");

    // Cleanup
    client.delete_parameter(param.id).await.unwrap();
    client.delete_endpoint(endpoint.id).await.unwrap();
}

#[tokio::test]
async fn test_schema_crud() {
    let client = setup_client().await;

    // Create
    let schema = Schema::new(
        "UserResponse",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "name": {"type": "string"}
            }
        }),
    );
    client.create_schema(&schema).await.unwrap();

    // Read
    let fetched = client.get_schema(schema.id).await.unwrap();
    assert_eq!(fetched.name, "UserResponse");

    // Find by name
    let found = client.get_schema_by_name("UserResponse").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, schema.id);

    // Delete
    client.delete_schema(schema.id).await.unwrap();
}

#[tokio::test]
async fn test_resource_endpoint_relationship() {
    let client = setup_client().await;

    // Create resource and endpoint
    let resource = Resource::new("TestResource", "Test resource");
    let endpoint = Endpoint::new("/test", HttpMethod::Get, "Test", None);

    client.create_resource(&resource).await.unwrap();
    client.create_endpoint(&endpoint).await.unwrap();

    // Link them
    client
        .link_resource_to_endpoint(resource.id, endpoint.id)
        .await
        .unwrap();

    // Get endpoints for resource
    let endpoints = client
        .get_endpoints_for_resource(resource.id)
        .await
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id, endpoint.id);

    // Cleanup
    client.delete_endpoint(endpoint.id).await.unwrap();
    client.delete_resource(resource.id).await.unwrap();
}
