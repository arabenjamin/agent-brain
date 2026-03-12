//! Integration tests for the repository-to-OpenAPI generation service.

use std::sync::Arc;

use agent_brain::mcp::tools::{ToolHandler, ToolRegistry};
use agent_brain::services::{
    ContextStore, LlmConfig, MergeStrategy, RepoAccessMethod, RepoAnalysisConfig, RepoError,
    RepoPlatform, RepoSource,
};
use agent_brain::skills::api::ApiSkill;
use serde_json::json;
use tokio::sync::RwLock;

fn create_api_skill(llm_config: Option<LlmConfig>) -> ApiSkill {
    let context_store = ContextStore::new();
    ApiSkill::new(None, Arc::new(RwLock::new(llm_config)), context_store, None)
}

// ============================================================================
// Tool Registry Tests
// ============================================================================

#[test]
fn test_build_openapi_from_repo_tool_exists() {
    let mut registry = ToolRegistry::new();
    let api_skill = create_api_skill(None);
    registry.register_skill(Box::new(api_skill));

    let tool = registry.get("build_openapi_from_repo");
    assert!(tool.is_some());

    let tool = tool.unwrap();
    assert_eq!(tool.name, "build_openapi_from_repo");
    assert!(tool.description.contains("repository"));
    assert!(tool.description.contains("OpenAPI"));
    assert!(
        tool.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("repo_url"))
    );
    assert!(
        tool.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("api_title"))
    );
}

#[test]
fn test_build_openapi_from_repo_tool_schema() {
    let mut registry = ToolRegistry::new();
    let api_skill = create_api_skill(None);
    registry.register_skill(Box::new(api_skill));

    let tool = registry.get("build_openapi_from_repo").unwrap();

    let props = &tool.input_schema["properties"];

    // Check required properties exist
    assert!(props["repo_url"].is_object());
    assert!(props["api_title"].is_object());

    // Check optional properties exist
    assert!(props["api_version"].is_object());
    assert!(props["base_url"].is_object());
    assert!(props["ref_name"].is_object());
    assert!(props["subdirectory"].is_object());
    assert!(props["include_patterns"].is_object());
    assert!(props["merge_strategy"].is_object());
    assert!(props["output_format"].is_object());
    assert!(props["auto_ingest"].is_object());

    // Check merge_strategy has enum values
    let merge_strategies = props["merge_strategy"]["enum"].as_array().unwrap();
    assert!(merge_strategies.contains(&json!("enhance")));
    assert!(merge_strategies.contains(&json!("replace")));
    assert!(merge_strategies.contains(&json!("ignore")));

    // Check output_format has enum values
    let output_formats = props["output_format"]["enum"].as_array().unwrap();
    assert!(output_formats.contains(&json!("json")));
    assert!(output_formats.contains(&json!("yaml")));
}

// ============================================================================
// URL Parsing Tests
// ============================================================================

#[test]
fn test_parse_github_url_https() {
    let result = RepoSource::parse("https://github.com/owner/repo");
    assert!(result.is_ok());

    let source = result.unwrap();
    assert_eq!(source.platform, RepoPlatform::GitHub);
    assert_eq!(source.owner, "owner");
    assert_eq!(source.repo, "repo");
    assert!(source.ref_name.is_none());
}

#[test]
fn test_parse_github_url_with_ref() {
    let result = RepoSource::parse("https://github.com/owner/repo/tree/develop");
    assert!(result.is_ok());

    let source = result.unwrap();
    assert_eq!(source.platform, RepoPlatform::GitHub);
    assert_eq!(source.owner, "owner");
    assert_eq!(source.repo, "repo");
    assert_eq!(source.ref_name, Some("develop".to_string()));
}

#[test]
fn test_parse_github_url_with_path() {
    let result = RepoSource::parse("https://github.com/owner/repo/tree/main/src/api");
    assert!(result.is_ok());

    let source = result.unwrap();
    assert_eq!(source.platform, RepoPlatform::GitHub);
    assert_eq!(source.owner, "owner");
    assert_eq!(source.repo, "repo");
    // Path after branch is included in ref_name since it's parsed as the branch path
    assert!(source.ref_name.is_some());
}

#[test]
fn test_parse_gitlab_url() {
    let result = RepoSource::parse("https://gitlab.com/owner/repo");
    assert!(result.is_ok());

    let source = result.unwrap();
    assert_eq!(source.platform, RepoPlatform::GitLab);
    assert_eq!(source.owner, "owner");
    assert_eq!(source.repo, "repo");
}

#[test]
fn test_parse_gitlab_url_with_ref() {
    let result = RepoSource::parse("https://gitlab.com/owner/repo/-/tree/develop");
    assert!(result.is_ok());

    let source = result.unwrap();
    assert_eq!(source.platform, RepoPlatform::GitLab);
    assert_eq!(source.owner, "owner");
    assert_eq!(source.repo, "repo");
    assert_eq!(source.ref_name, Some("develop".to_string()));
}

#[test]
fn test_parse_invalid_url() {
    // Non-URL strings are treated as local paths (Ok), not errors.
    // Only unsupported platforms (e.g. bitbucket) return Err.
    let result = RepoSource::parse("not-a-url");
    assert!(result.is_ok());
}

#[test]
fn test_parse_unsupported_platform() {
    let result = RepoSource::parse("https://bitbucket.org/owner/repo");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        RepoError::UnsupportedPlatform(_)
    ));
}

#[test]
fn test_parse_url_missing_repo() {
    let result = RepoSource::parse("https://github.com/owner");
    assert!(result.is_err());
}

// ============================================================================
// Merge Strategy Tests
// ============================================================================

#[test]
fn test_merge_strategy_parse_str() {
    // MergeStrategy::parse_str returns the strategy or defaults to Enhance
    assert_eq!(MergeStrategy::parse_str("enhance"), MergeStrategy::Enhance);
    assert_eq!(MergeStrategy::parse_str("replace"), MergeStrategy::Replace);
    assert_eq!(MergeStrategy::parse_str("ignore"), MergeStrategy::Ignore);
    // Unknown strings default to Enhance
    assert_eq!(MergeStrategy::parse_str("unknown"), MergeStrategy::Enhance);
}

#[test]
fn test_merge_strategy_default() {
    assert_eq!(MergeStrategy::default(), MergeStrategy::Enhance);
}

// ============================================================================
// RepoAnalysisConfig Tests
// ============================================================================

#[test]
fn test_repo_analysis_config_default() {
    let config = RepoAnalysisConfig::default();

    assert!(!config.exclude_patterns.is_empty());
    assert!(config.max_file_size > 0);
    assert!(config.clone_threshold_bytes > 0);
    assert!(config.detect_existing_spec);
}

#[test]
fn test_repo_analysis_config_custom() {
    use std::time::Duration;

    let config = RepoAnalysisConfig {
        include_patterns: vec!["**/*.rs".to_string()],
        exclude_patterns: vec!["**/test/**".to_string()],
        max_file_size: 50_000,
        clone_threshold_bytes: 5_000_000,
        max_api_files: 50,
        request_timeout: Duration::from_secs(60),
        detect_existing_spec: false,
    };

    assert_eq!(config.include_patterns.len(), 1);
    assert_eq!(config.exclude_patterns.len(), 1);
    assert_eq!(config.max_file_size, 50_000);
    assert_eq!(config.clone_threshold_bytes, 5_000_000);
    assert!(!config.detect_existing_spec);
}

// ============================================================================
// RepoAccessMethod Tests
// ============================================================================

#[test]
fn test_repo_access_method_display() {
    assert_eq!(format!("{}", RepoAccessMethod::Api), "api");
    assert_eq!(format!("{}", RepoAccessMethod::Clone), "clone");
}

// ============================================================================
// RepoPlatform Tests
// ============================================================================

#[test]
fn test_repo_platform_display() {
    assert_eq!(format!("{}", RepoPlatform::GitHub), "GitHub");
    assert_eq!(format!("{}", RepoPlatform::GitLab), "GitLab");
}

// ============================================================================
// RepoSource Helper Method Tests
// ============================================================================

#[test]
fn test_repo_source_api_base_url() {
    let github = RepoSource::parse("https://github.com/owner/repo").unwrap();
    assert_eq!(github.api_base_url(), "https://api.github.com");

    let gitlab = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
    assert_eq!(gitlab.api_base_url(), "https://gitlab.com/api/v4");
}

#[test]
fn test_repo_source_repo_api_url() {
    let github = RepoSource::parse("https://github.com/owner/repo").unwrap();
    assert_eq!(
        github.repo_api_url(),
        "https://api.github.com/repos/owner/repo"
    );

    let gitlab = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
    // GitLab URL-encodes the project path
    assert!(gitlab.repo_api_url().contains("owner%2Frepo"));
}

#[test]
fn test_repo_source_clone_url_without_token() {
    let github = RepoSource::parse("https://github.com/owner/repo").unwrap();
    assert_eq!(github.clone_url(None), "https://github.com/owner/repo.git");

    let gitlab = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
    assert_eq!(gitlab.clone_url(None), "https://gitlab.com/owner/repo.git");
}

#[test]
fn test_repo_source_clone_url_with_token() {
    let github = RepoSource::parse("https://github.com/owner/repo").unwrap();
    assert_eq!(
        github.clone_url(Some("mytoken")),
        "https://mytoken@github.com/owner/repo.git"
    );

    let gitlab = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
    assert_eq!(
        gitlab.clone_url(Some("mytoken")),
        "https://oauth2:mytoken@gitlab.com/owner/repo.git"
    );
}

// ============================================================================
// Tool Handler Tests
// ============================================================================

#[tokio::test]
async fn test_build_openapi_from_repo_requires_llm() {
    // No LLM config
    let api_skill = create_api_skill(None);
    let handler = ToolHandler::new(vec![Box::new(api_skill)]);

    let result = handler
        .execute(
            "build_openapi_from_repo",
            Some(json!({
                "repo_url": "https://github.com/owner/repo",
                "api_title": "Test API"
            })),
        )
        .await;

    // Should return an error because LLM is required
    assert!(
        result.is_error.unwrap_or(false),
        "Should error without LLM config"
    );

    if let Some(content) = result.content.first()
        && let agent_brain::mcp::protocol::Content::Text { text } = content
    {
        assert!(text.contains("LLM") || text.contains("configuration"));
    }
}

#[tokio::test]
async fn test_build_openapi_from_repo_invalid_url() {
    let api_skill = create_api_skill(Some(LlmConfig::default()));
    let handler = ToolHandler::new(vec![Box::new(api_skill)]);

    let result = handler
        .execute(
            "build_openapi_from_repo",
            Some(json!({
                "repo_url": "not-a-valid-url",
                "api_title": "Test API"
            })),
        )
        .await;

    // Should return an error for invalid URL
    assert!(
        result.is_error.unwrap_or(false),
        "Tool should return error for invalid URL"
    );
}

#[tokio::test]
async fn test_build_openapi_from_repo_unsupported_platform() {
    let api_skill = create_api_skill(Some(LlmConfig::default()));
    let handler = ToolHandler::new(vec![Box::new(api_skill)]);

    let result = handler
        .execute(
            "build_openapi_from_repo",
            Some(json!({
                "repo_url": "https://bitbucket.org/owner/repo",
                "api_title": "Test API"
            })),
        )
        .await;

    // Should return an error for unsupported platform
    assert!(
        result.is_error.unwrap_or(false),
        "Tool should return error for unsupported platform"
    );
}

// ============================================================================
// Network Integration Tests (only run with network access)
// ============================================================================

#[tokio::test]
#[ignore] // Requires network access and valid repo
async fn test_build_openapi_from_public_repo() {
    let api_skill = create_api_skill(Some(LlmConfig::default()));
    let handler = ToolHandler::new(vec![Box::new(api_skill)]);

    // Test with a small public repo
    let result = handler
        .execute(
            "build_openapi_from_repo",
            Some(json!({
                "repo_url": "https://github.com/expressjs/express",
                "api_title": "Express API",
                "output_format": "json"
            })),
        )
        .await;

    if let Some(content) = result.content.first()
        && let agent_brain::mcp::protocol::Content::Text { text } = content
    {
        println!("Tool output:\n{}", text);
    }
}

#[tokio::test]
#[ignore] // Requires network access
async fn test_github_api_rate_limit_handling() {
    // Test that rate limit errors are handled gracefully
    let api_skill = create_api_skill(Some(LlmConfig::default()));
    let handler = ToolHandler::new(vec![Box::new(api_skill)]);

    let result = handler
        .execute(
            "build_openapi_from_repo",
            Some(json!({
                "repo_url": "https://github.com/torvalds/linux",  // Very large repo
                "api_title": "Linux Kernel",
                "output_format": "json"
            })),
        )
        .await;

    // Should either succeed, fail gracefully, or clone instead
    if result.is_error.unwrap_or(false)
        && let Some(content) = result.content.first()
        && let agent_brain::mcp::protocol::Content::Text { text } = content
    {
        println!("Expected error for large repo: {}", text);
    }
}
