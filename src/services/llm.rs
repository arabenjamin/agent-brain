use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::models::HealingAction;

/// Default timeout for LLM requests (2 minutes for slow models).
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Default Ollama API URL.
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Default model to use.
const DEFAULT_MODEL: &str = "qwen3.5:4b";

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Model not available: {0}")]
    ModelNotAvailable(String),

    #[error("Generation failed: {0}")]
    GenerationFailed(String),

    #[error("Failed to parse LLM response: {0}")]
    ParseError(String),

    #[error("Server not reachable: {0}")]
    ServerNotReachable(String),

    #[error("Provider error: {0}")]
    Provider(#[from] crate::services::llm_providers::LlmProviderError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LlmProviderType {
    #[default]
    Ollama,
    Anthropic,
    Gemini,
    /// vLLM or any OpenAI-compatible endpoint (LM Studio, Groq, Together, etc.)
    VLlm,
}

impl std::fmt::Display for LlmProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Configuration for the LLM client.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// LLM provider type.
    pub provider: LlmProviderType,

    /// API base URL.
    pub base_url: Option<String>,

    /// API key (required for cloud providers).
    pub api_key: Option<String>,

    /// Model name to use for text generation.
    pub model: String,

    /// Model name to use for embeddings.
    pub embed_model: Option<String>,

    /// Base URL for the embedding endpoint when it differs from the generation endpoint.
    /// Used by the vLLM provider to route embed() calls to a separate server (e.g. port 8001).
    pub embed_base_url: Option<String>,

    /// Request timeout.
    pub timeout: Duration,

    /// Temperature for generation (0.0 - 1.0).
    pub temperature: f32,

    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: LlmProviderType::Ollama,
            base_url: Some(DEFAULT_OLLAMA_URL.to_string()),
            api_key: None,
            model: DEFAULT_MODEL.to_string(),
            embed_model: None,
            embed_base_url: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            temperature: 0.7,
            max_tokens: None,
        }
    }
}

impl LlmConfig {
    /// Create config from environment-style parameters.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Set the provider type.
    pub fn with_provider(mut self, provider: LlmProviderType) -> Self {
        self.provider = provider;
        self
    }

    /// Set the API key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set the model name.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set the embedding model (separate from the generation model).
    pub fn with_embed_model(mut self, model: impl Into<String>) -> Self {
        self.embed_model = Some(model.into());
        self
    }

    /// Set a separate base URL for the embedding endpoint.
    /// When set, embed() calls are routed to this URL instead of `base_url`.
    pub fn with_embed_base_url(mut self, url: impl Into<String>) -> Self {
        self.embed_base_url = Some(url.into());
        self
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature.clamp(0.0, 1.0);
        self
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// LLM chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// Result of LLM generation with metadata.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// The generated text.
    pub text: String,

    /// Total duration in nanoseconds (if available).
    pub duration_ns: Option<u64>,

    /// Number of tokens evaluated (if available).
    pub tokens_evaluated: Option<u32>,
}

/// Analysis result from error analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorAnalysis {
    /// Whether the error suggests a documentation issue.
    pub is_doc_issue: bool,

    /// Suggested healing action, if any.
    pub suggested_action: Option<HealingAction>,

    /// The LLM's reasoning.
    pub reasoning: String,

    /// Confidence level (0.0 - 1.0).
    pub confidence: f32,

    /// Suggested corrected request body, if applicable.
    pub corrected_body: Option<serde_json::Value>,
}

/// LLM client for interacting with various providers.
pub struct LlmClient {
    provider: Arc<dyn crate::services::llm_providers::LlmProvider>,
    embed_provider: Arc<dyn crate::services::llm_providers::LlmProvider>,
    config: LlmConfig,
}

impl LlmClient {
    /// Create a new LLM client with default configuration.
    pub fn new() -> Result<Self, LlmError> {
        Self::with_config(LlmConfig::default())
    }

    /// Create a new LLM client with custom configuration.
    pub fn with_config(config: LlmConfig) -> Result<Self, LlmError> {
        use crate::services::llm_providers::{
            ProviderConfig, anthropic::AnthropicProvider, gemini::GeminiProvider,
            ollama::OllamaProvider,
        };

        let provider_config = ProviderConfig {
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            timeout: config.timeout,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        };

        let provider: Arc<dyn crate::services::llm_providers::LlmProvider> = match config.provider {
            LlmProviderType::Ollama => Arc::new(OllamaProvider::new(provider_config)),
            LlmProviderType::Anthropic => Arc::new(AnthropicProvider::new(provider_config)),
            LlmProviderType::Gemini => Arc::new(GeminiProvider::new(provider_config)),
            LlmProviderType::VLlm => {
                use crate::services::llm_providers::openai_compat::OpenAiCompatProvider;
                Arc::new(OpenAiCompatProvider::new(provider_config))
            }
        };

        // Initialize embedding provider (separate from generation if requested)
        let embed_provider = if let Some(ref embed_model) = config.embed_model {
            let embed_base = config.embed_base_url.clone().or(config.base_url.clone());
            let embed_config = ProviderConfig {
                model: embed_model.clone(),
                api_key: config.api_key.clone(),
                base_url: embed_base,
                timeout: config.timeout,
                temperature: 0.0,
                max_tokens: None,
            };
            // For vLLM, route embeddings to the OpenAI-compat endpoint (potentially a separate server).
            // For all other providers, keep using Ollama as the local embedding backend.
            match config.provider {
                LlmProviderType::VLlm => {
                    use crate::services::llm_providers::openai_compat::OpenAiCompatProvider;
                    Arc::new(OpenAiCompatProvider::new(embed_config))
                        as Arc<dyn crate::services::llm_providers::LlmProvider>
                }
                _ => Arc::new(OllamaProvider::new(embed_config))
                    as Arc<dyn crate::services::llm_providers::LlmProvider>,
            }
        } else {
            provider.clone()
        };

        Ok(Self {
            provider,
            embed_provider,
            config,
        })
    }

    /// Get the current configuration.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Check if the provider is reachable.
    pub async fn health_check(&self) -> Result<bool, LlmError> {
        Ok(self.provider.health_check().await)
    }

    /// Generate text from a prompt.
    pub async fn generate(&self, prompt: &str) -> Result<LlmResponse, LlmError> {
        self.generate_with_system(prompt, None).await
    }

    /// Generate text with a system prompt.
    pub async fn generate_with_system(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmError> {
        self.provider
            .generate(prompt, system)
            .await
            .map_err(LlmError::from)
    }

    /// Generate embeddings for a text.
    pub async fn embeddings(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed_provider
            .embed(text)
            .await
            .map_err(LlmError::from)
    }

    /// Chat with the model using message history.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmError> {
        self.provider.chat(messages).await.map_err(LlmError::from)
    }

    /// Analyze an API error and suggest a fix.
    pub async fn analyze_error(
        &self,
        endpoint_path: &str,
        method: &str,
        request_body: Option<&serde_json::Value>,
        error_status: u16,
        error_body: &str,
        schema_info: Option<&str>,
    ) -> Result<ErrorAnalysis, LlmError> {
        let prompt = build_error_analysis_prompt(
            endpoint_path,
            method,
            request_body,
            error_status,
            error_body,
            schema_info,
        );

        let system = ERROR_ANALYSIS_SYSTEM_PROMPT;

        let response = self.generate_with_system(&prompt, Some(system)).await?;

        parse_error_analysis(&response.text)
    }

    /// Suggest a corrected request body based on error.
    pub async fn suggest_correction(
        &self,
        endpoint_path: &str,
        method: &str,
        original_body: &serde_json::Value,
        error_body: &str,
        schema_info: Option<&str>,
    ) -> Result<Option<serde_json::Value>, LlmError> {
        let prompt = build_correction_prompt(
            endpoint_path,
            method,
            original_body,
            error_body,
            schema_info,
        );

        let system = CORRECTION_SYSTEM_PROMPT;

        let response = self.generate_with_system(&prompt, Some(system)).await?;

        parse_corrected_body(&response.text)
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::with_config(LlmConfig::default()).expect("Failed to create default LLM client")
    }
}

// ============================================================================
// Prompt Templates
// ============================================================================

pub const ERROR_ANALYSIS_SYSTEM_PROMPT: &str = r#"You are an API documentation expert. Your task is to analyze API errors and determine if they indicate documentation issues.

You must respond in valid JSON format with the following structure:
{
  "is_doc_issue": boolean,
  "action_type": "rename_parameter" | "change_type" | "add_parameter" | "update_path" | "update_schema" | null,
  "action_details": { ... } | null,
  "reasoning": "string",
  "confidence": number between 0 and 1,
  "corrected_body": { ... } | null
}

action_details depends on action_type:
- rename_parameter: {"old_name": "...", "new_name": "..."}
- change_type: {"param_name": "...", "old_type": "...", "new_type": "..."}
- add_parameter: {"param_name": "...", "required": boolean}
- update_path: {"old_path": "...", "new_path": "..."}
- update_schema: {"status_code": number, "diff_summary": "..."}

Be precise and only suggest changes when the error clearly indicates a documentation mismatch."#;

pub const CORRECTION_SYSTEM_PROMPT: &str = r#"You are an API request expert. Your task is to analyze a failed API request and suggest a corrected request body.

You must respond with ONLY a valid JSON object that represents the corrected request body.
Do not include any explanation or markdown formatting - just the raw JSON.
If you cannot determine a correction, respond with: null"#;

pub fn build_error_analysis_prompt(
    endpoint_path: &str,
    method: &str,
    request_body: Option<&serde_json::Value>,
    error_status: u16,
    error_body: &str,
    schema_info: Option<&str>,
) -> String {
    let body_str = request_body
        .map(|b| serde_json::to_string_pretty(b).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "No body".to_string());

    let schema_str = schema_info.unwrap_or("No schema information available");

    format!(
        r#"Analyze this API error:

## Endpoint
{method} {endpoint_path}

## Request Body
{body_str}

## Error Response
Status: {error_status}
Body: {error_body}

## Schema Information
{schema_str}

Determine if this error indicates a documentation issue and suggest a fix if applicable."#
    )
}

pub fn build_correction_prompt(
    endpoint_path: &str,
    method: &str,
    original_body: &serde_json::Value,
    error_body: &str,
    schema_info: Option<&str>,
) -> String {
    let body_str = serde_json::to_string_pretty(original_body).unwrap_or_else(|_| "{}".to_string());
    let schema_str = schema_info.unwrap_or("No schema information available");

    format!(
        r#"Fix this API request:

## Endpoint
{method} {endpoint_path}

## Original Request Body
{body_str}

## Error Response
{error_body}

## Schema Information
{schema_str}

Provide the corrected JSON request body only."#
    )
}

// ============================================================================
// Response Parsing
// ============================================================================

#[derive(Debug, Deserialize)]
struct RawErrorAnalysis {
    is_doc_issue: bool,
    action_type: Option<String>,
    action_details: Option<serde_json::Value>,
    reasoning: String,
    confidence: f32,
    corrected_body: Option<serde_json::Value>,
}

pub fn parse_error_analysis(response: &str) -> Result<ErrorAnalysis, LlmError> {
    // Try to extract JSON from the response (it might be wrapped in markdown)
    let json_str = extract_json(response);

    let raw: RawErrorAnalysis = serde_json::from_str(json_str).map_err(|e| {
        LlmError::ParseError(format!(
            "Failed to parse error analysis: {}. Response: {}",
            e,
            &response[..response.len().min(200)]
        ))
    })?;

    let suggested_action = match (raw.action_type.as_deref(), raw.action_details) {
        (Some("rename_parameter"), Some(details)) => {
            let old_name = details["old_name"].as_str().unwrap_or("").to_string();
            let new_name = details["new_name"].as_str().unwrap_or("").to_string();
            Some(HealingAction::RenameParameter {
                old_name,
                new_name,
                param_id: Uuid::nil(), // Will be filled in by the healing orchestrator
            })
        }
        (Some("change_type"), Some(details)) => {
            let param_name = details["param_name"].as_str().unwrap_or("").to_string();
            let old_type = details["old_type"].as_str().unwrap_or("").to_string();
            let new_type = details["new_type"].as_str().unwrap_or("").to_string();
            Some(HealingAction::ChangeParameterType {
                param_name,
                old_type,
                new_type,
            })
        }
        (Some("add_parameter"), Some(details)) => {
            let param_name = details["param_name"].as_str().unwrap_or("").to_string();
            let required = details["required"].as_bool().unwrap_or(true);
            Some(HealingAction::AddMissingParameter {
                param_name,
                required,
                detected_in_error_msg: String::new(), // Will be filled in
            })
        }
        (Some("update_path"), Some(details)) => {
            let old_path = details["old_path"].as_str().unwrap_or("").to_string();
            let new_path = details["new_path"].as_str().unwrap_or("").to_string();
            Some(HealingAction::UpdateEndpointPath { old_path, new_path })
        }
        (Some("update_schema"), Some(details)) => {
            let status_code = details["status_code"].as_u64().unwrap_or(200) as u16;
            let diff_summary = details["diff_summary"].as_str().unwrap_or("").to_string();
            Some(HealingAction::UpdateResponseSchema {
                status_code,
                diff_summary,
            })
        }
        _ => None,
    };

    Ok(ErrorAnalysis {
        is_doc_issue: raw.is_doc_issue,
        suggested_action,
        reasoning: raw.reasoning,
        confidence: raw.confidence.clamp(0.0, 1.0),
        corrected_body: raw.corrected_body,
    })
}

pub fn parse_corrected_body(response: &str) -> Result<Option<serde_json::Value>, LlmError> {
    let json_str = extract_json(response);

    if json_str.trim() == "null" {
        return Ok(None);
    }

    serde_json::from_str(json_str)
        .map(Some)
        .map_err(|e| LlmError::ParseError(format!("Failed to parse corrected body: {}", e)))
}

/// Extract JSON from a response that might be wrapped in markdown code blocks.
pub fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();

    // Check for ```json ... ``` blocks
    if let Some(start) = trimmed.find("```json") {
        let content_start = start + 7;
        if let Some(end) = trimmed[content_start..].find("```") {
            return trimmed[content_start..content_start + end].trim();
        }
    }

    // Check for ``` ... ``` blocks
    if let Some(start) = trimmed.find("```") {
        let content_start = start + 3;
        // Skip the optional language identifier on the first line
        let content = &trimmed[content_start..];
        let actual_start = content.find('\n').map(|i| i + 1).unwrap_or(0);
        if let Some(end) = content[actual_start..].find("```") {
            return content[actual_start..actual_start + end].trim();
        }
    }

    // Look for JSON object
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        return &trimmed[start..=end];
    }

    // Look for JSON array
    if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        return &trimmed[start..=end];
    }

    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_config_default() {
        let config = LlmConfig::default();
        assert_eq!(config.base_url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(config.model, "qwen3.5:4b");
        assert_eq!(config.temperature, 0.7);
    }

    #[test]
    fn test_llm_config_builder() {
        let config = LlmConfig::new("http://custom:1234", "mistral")
            .with_temperature(0.5)
            .with_max_tokens(1000)
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.base_url.as_deref(), Some("http://custom:1234"));
        assert_eq!(config.model, "mistral");
        assert_eq!(config.temperature, 0.5);
        assert_eq!(config.max_tokens, Some(1000));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_llm_config_temperature_clamping() {
        let config = LlmConfig::default().with_temperature(2.0);
        assert_eq!(config.temperature, 1.0);

        let config = LlmConfig::default().with_temperature(-0.5);
        assert_eq!(config.temperature, 0.0);
    }

    #[test]
    fn test_chat_message_constructors() {
        let system = ChatMessage::system("You are helpful.");
        assert_eq!(system.role, "system");
        assert_eq!(system.content, "You are helpful.");

        let user = ChatMessage::user("Hello");
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "Hello");

        let assistant = ChatMessage::assistant("Hi there!");
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content, "Hi there!");
    }

    #[test]
    fn test_extract_json_plain() {
        let text = r#"{"key": "value"}"#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_with_markdown() {
        let text = r#"Here is the response:
```json
{"key": "value"}
```
That's all."#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_with_plain_code_block() {
        let text = r#"```
{"key": "value"}
```"#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_surrounded_by_text() {
        let text = r#"The corrected body is: {"name": "test", "id": 123} and that should work."#;
        assert_eq!(extract_json(text), r#"{"name": "test", "id": 123}"#);
    }

    #[test]
    fn test_parse_error_analysis() {
        let response = r#"{
            "is_doc_issue": true,
            "action_type": "rename_parameter",
            "action_details": {"old_name": "id", "new_name": "user_id"},
            "reasoning": "The error message indicates user_id is required",
            "confidence": 0.85,
            "corrected_body": null
        }"#;

        let analysis = parse_error_analysis(response).unwrap();
        assert!(analysis.is_doc_issue);
        assert_eq!(analysis.confidence, 0.85);
        assert!(analysis.suggested_action.is_some());

        if let Some(HealingAction::RenameParameter {
            old_name, new_name, ..
        }) = analysis.suggested_action
        {
            assert_eq!(old_name, "id");
            assert_eq!(new_name, "user_id");
        } else {
            panic!("Expected RenameParameter action");
        }
    }

    #[test]
    fn test_parse_error_analysis_no_issue() {
        let response = r#"{
            "is_doc_issue": false,
            "action_type": null,
            "action_details": null,
            "reasoning": "This is a server error, not a documentation issue",
            "confidence": 0.95,
            "corrected_body": null
        }"#;

        let analysis = parse_error_analysis(response).unwrap();
        assert!(!analysis.is_doc_issue);
        assert!(analysis.suggested_action.is_none());
    }

    #[test]
    fn test_parse_error_analysis_add_parameter() {
        let response = r#"{
            "is_doc_issue": true,
            "action_type": "add_parameter",
            "action_details": {"param_name": "api_key", "required": true},
            "reasoning": "Error indicates api_key is missing",
            "confidence": 0.9,
            "corrected_body": null
        }"#;

        let analysis = parse_error_analysis(response).unwrap();
        assert!(analysis.is_doc_issue);

        if let Some(HealingAction::AddMissingParameter {
            param_name,
            required,
            ..
        }) = analysis.suggested_action
        {
            assert_eq!(param_name, "api_key");
            assert!(required);
        } else {
            panic!("Expected AddMissingParameter action");
        }
    }

    #[test]
    fn test_parse_corrected_body() {
        let response = r#"{"user_id": 123, "name": "test"}"#;
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_some());
        let body = body.unwrap();
        assert_eq!(body["user_id"], 123);
        assert_eq!(body["name"], "test");
    }

    #[test]
    fn test_parse_corrected_body_null() {
        let response = "null";
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_none());
    }

    #[test]
    fn test_parse_corrected_body_with_markdown() {
        let response = r#"Here's the fix:
```json
{"user_id": 456}
```"#;
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_some());
        assert_eq!(body.unwrap()["user_id"], 456);
    }

    #[test]
    fn test_build_error_analysis_prompt() {
        let body = serde_json::json!({"id": 123});
        let prompt = build_error_analysis_prompt(
            "/users/{id}",
            "GET",
            Some(&body),
            400,
            "Missing required field: user_id",
            Some("User schema with id field"),
        );

        assert!(prompt.contains("/users/{id}"));
        assert!(prompt.contains("GET"));
        assert!(prompt.contains("400"));
        assert!(prompt.contains("Missing required field"));
    }

    #[test]
    fn test_llm_client_creation() {
        let client = LlmClient::new();
        assert!(client.is_ok());
    }

    #[test]
    fn test_llm_client_with_config() {
        let config = LlmConfig::new("http://test:1234", "test-model");
        let client = LlmClient::with_config(config);
        assert!(client.is_ok());

        let client = client.unwrap();
        assert_eq!(
            client.config().base_url.as_deref(),
            Some("http://test:1234")
        );
        assert_eq!(client.config().model, "test-model");
    }
}
