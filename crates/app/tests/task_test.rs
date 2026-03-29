//! Integration tests for the task skill with Neo4j persistence.
//! These tests require a running Neo4j instance.

use std::sync::Arc;

use agent_brain::mcp::tools::{ToolHandler, ToolRegistry};
use agent_brain::repository::Neo4jClient;
use agent_brain::services::{LlmConfig, SharedLlm};
use agent_brain::skills::task::TaskSkill;
use serde_json::json;
use tokio::sync::RwLock;
use uuid::Uuid;

#[test]
fn test_create_task_tool_exists() {
    let mut registry = ToolRegistry::new();
    let task_skill = TaskSkill::new(
        SharedLlm::new(Arc::new(RwLock::new(None::<LlmConfig>))),
        None,
        None,
    );
    registry.register_skill(Box::new(task_skill));

    let tool = registry.get("create_task");
    assert!(tool.is_some());

    let tool = tool.unwrap();
    assert_eq!(tool.name, "create_task");
    assert!(tool.description.contains("Create"));
    assert!(
        tool.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("goal"))
    );
}

#[tokio::test]
async fn test_create_task_without_db() {
    let task_skill = TaskSkill::new(
        SharedLlm::new(Arc::new(RwLock::new(None::<LlmConfig>))),
        None,
        None,
    );
    let handler = ToolHandler::new(vec![Box::new(task_skill)]);

    let result = handler
        .execute(
            "create_task",
            Some(json!({
                "goal": "Test goal without DB",
                "context": "Should error gracefully"
            })),
        )
        .await;

    // Should fail because DB is missing
    assert!(result.is_error.unwrap_or(false));

    if let Some(content) = result.content.first()
        && let agent_brain::mcp::protocol::Content::Text { text } = content
    {
        println!("Tool output: {}", text);
        assert!(text.contains("Persistence layer (Neo4j) not available"));
    }
}

// NOTE: This test requires a running Neo4j instance.
// Uses NEO4J_URI / NEO4J_USER / NEO4J_PASSWORD env vars (CI) or
// falls back to localhost:7687 / neo4j / password (local dev).
#[tokio::test]
async fn test_create_task_with_live_db() {
    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
    let pass = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "password".to_string());

    let client = match Neo4jClient::new(&uri, &user, &pass).await {
        Ok(c) => c,
        Err(e) => {
            println!("Skipping live DB test: Could not connect to Neo4j at {uri}: {e}");
            return;
        }
    };

    // Verify connectivity with init_schema; skip if we can't reach Neo4j.
    if let Err(e) = client.init_schema().await {
        println!("Skipping live DB test: init_schema failed (Neo4j unreachable?): {e}");
        return;
    }

    let task_skill = TaskSkill::new(
        SharedLlm::new(Arc::new(RwLock::new(None::<LlmConfig>))),
        Some(Arc::new(client.clone())),
        None,
    );
    let handler = ToolHandler::new(vec![Box::new(task_skill)]);

    let goal = format!("Integration Test Task {}", Uuid::new_v4());

    let result = handler
        .execute(
            "create_task",
            Some(json!({
                "goal": goal,
                "context": "Integration test context"
            })),
        )
        .await;

    assert!(
        result.is_error.is_none() || !result.is_error.unwrap(),
        "create_task returned an error"
    );

    // Verify it exists in DB
    if let Some(content) = result.content.first()
        && let agent_brain::mcp::protocol::Content::Text { text } = content
    {
        let json: serde_json::Value = serde_json::from_str(text).unwrap();
        let id = json["id"].as_str().unwrap();

        let task = client.get_task(id).await.unwrap();
        assert!(task.is_some());
        let t = task.unwrap();
        assert_eq!(t.goal, goal);
        assert_eq!(t.context, Some("Integration test context".to_string()));

        println!("Successfully created and verified task: {id}");
    }
}
