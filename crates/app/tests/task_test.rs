//! Integration tests for the task skill with Neo4j persistence.
//! These tests require a running Neo4j instance.

use agent_brain::mcp::tools::{ToolHandler, ToolRegistry};
use agent_brain::skills::{task::TaskSkill, Skill};
use agent_brain::repository::Neo4jClient;
use serde_json::json;
use uuid::Uuid;

#[test]
fn test_create_task_tool_exists() {
    let mut registry = ToolRegistry::new();
    let task_skill = TaskSkill::new(None, None);
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
    let task_skill = TaskSkill::new(None, None);
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
    
    if let Some(content) = result.content.first() {
        if let agent_brain::mcp::protocol::Content::Text { text } = content {
            println!("Tool output: {}", text);
            assert!(text.contains("Persistence layer (Neo4j) not available"));
        }
    }
}

// NOTE: This test requires a running Neo4j instance on localhost:7688
#[tokio::test]
// #[ignore] // Remove ignore to run against live DB
async fn test_create_task_with_live_db() {
    // Check if we can connect to the DB first, otherwise skip gracefully
    let uri = "bolt://localhost:7688";
    let user = "neo4j";
    let pass = "password";
    
    let client_result = Neo4jClient::new(uri, user, pass).await;
    
    if let Ok(client) = client_result {
        // Initialize schema if needed (might already be initialized)
        let _ = client.init_schema().await;

        let task_skill = TaskSkill::new(None, Some(client.clone()));
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

        assert!(result.is_error.is_none() || !result.is_error.unwrap());
        
        // Verify it exists in DB
        // Parse ID from output
        if let Some(content) = result.content.first() {
            if let agent_brain::mcp::protocol::Content::Text { text } = content {
                let json: serde_json::Value = serde_json::from_str(text).unwrap();
                let id = json["task_id"].as_str().unwrap();
                
                let task = client.get_task(id).await.unwrap();
                assert!(task.is_some());
                let t = task.unwrap();
                assert_eq!(t.goal, goal);
                assert_eq!(t.context, Some("Integration test context".to_string()));
                
                println!("Successfully created and verified task: {}", id);
            }
        }
    } else {
        println!("Skipping live DB test: Could not connect to Neo4j at {}", uri);
    }
}
