use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::models::{Endpoint, EndpointStatus, HealingAction, HealingEvent, HttpMethod};
use crate::repository::Neo4jClient;
use crate::services::http::{HttpExecutor, HttpResponse, RequestBuilder};
use crate::services::llm::{ErrorAnalysis, LlmClient};

/// Maximum number of healing retry attempts.
const MAX_HEALING_RETRIES: usize = 2;

/// Minimum confidence threshold for applying healing suggestions.
const MIN_HEALING_CONFIDENCE: f32 = 0.7;

#[derive(Debug, Error)]
pub enum HealingError {
    #[error("HTTP error: {0}")]
    Http(#[from] crate::services::http::HttpError),

    #[error("LLM error: {0}")]
    Llm(#[from] crate::services::llm::LlmError),

    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),

    #[error("Endpoint not found: {0}")]
    EndpointNotFound(Uuid),

    #[error("Healing failed after {0} attempts")]
    MaxRetriesExceeded(usize),

    #[error("No LLM client configured for healing")]
    NoLlmClient,
}

/// Configuration for the healing orchestrator.
#[derive(Debug, Clone)]
pub struct HealingConfig {
    /// Maximum number of retry attempts.
    pub max_retries: usize,

    /// Minimum confidence for applying healing.
    pub min_confidence: f32,

    /// Whether to enable automatic healing.
    pub auto_heal: bool,

    /// Whether to apply changes to the graph.
    pub apply_to_graph: bool,
}

impl Default for HealingConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_HEALING_RETRIES,
            min_confidence: MIN_HEALING_CONFIDENCE,
            auto_heal: true,
            apply_to_graph: true,
        }
    }
}

impl HealingConfig {
    /// Disable automatic healing (only report issues).
    pub fn analysis_only() -> Self {
        Self {
            auto_heal: false,
            apply_to_graph: false,
            ..Default::default()
        }
    }
}

/// Result of a healing execution attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealingResult {
    /// The final HTTP response.
    pub response: HttpResponseSummary,

    /// Whether the request was successful.
    pub success: bool,

    /// Whether healing was applied.
    pub healed: bool,

    /// Number of attempts made.
    pub attempts: usize,

    /// Healing events created during this execution.
    pub healing_events: Vec<HealingEventSummary>,

    /// The final endpoint status.
    pub endpoint_status: EndpointStatus,

    /// Error analysis from LLM (if performed).
    pub analysis: Option<ErrorAnalysisSummary>,
}

/// Summary of an HTTP response for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponseSummary {
    pub status_code: u16,
    pub body: String,
    pub duration_ms: u64,
}

impl From<&HttpResponse> for HttpResponseSummary {
    fn from(response: &HttpResponse) -> Self {
        Self {
            status_code: response.status_code,
            body: response.body.clone(),
            duration_ms: response.duration_ms,
        }
    }
}

/// Summary of a healing event for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealingEventSummary {
    pub id: Uuid,
    pub action: HealingAction,
    pub reasoning: String,
    pub verified: bool,
}

impl From<&HealingEvent> for HealingEventSummary {
    fn from(event: &HealingEvent) -> Self {
        Self {
            id: event.id,
            action: event.action.clone(),
            reasoning: event.ai_reasoning.clone(),
            verified: event.verified,
        }
    }
}

/// Summary of error analysis for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorAnalysisSummary {
    pub is_doc_issue: bool,
    pub confidence: f32,
    pub reasoning: String,
    pub suggested_action: Option<String>,
}

impl From<&ErrorAnalysis> for ErrorAnalysisSummary {
    fn from(analysis: &ErrorAnalysis) -> Self {
        Self {
            is_doc_issue: analysis.is_doc_issue,
            confidence: analysis.confidence,
            reasoning: analysis.reasoning.clone(),
            suggested_action: analysis
                .suggested_action
                .as_ref()
                .map(|a| format!("{:?}", a)),
        }
    }
}

/// Request context for healing execution.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Base URL for API requests.
    pub base_url: String,

    /// Path parameters to substitute.
    pub path_params: HashMap<String, String>,

    /// Query parameters.
    pub query_params: HashMap<String, String>,

    /// Request headers.
    pub headers: HashMap<String, String>,

    /// Request body (for POST/PUT/PATCH).
    pub body: Option<serde_json::Value>,
}

impl RequestContext {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            path_params: HashMap::new(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: None,
        }
    }

    pub fn with_path_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.path_params.insert(name.into(), value.into());
        self
    }

    pub fn with_query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_params.insert(name.into(), value.into());
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn with_body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

/// The healing orchestrator manages the self-healing API execution loop.
pub struct HealingOrchestrator {
    http: HttpExecutor,
    llm: Option<LlmClient>,
    neo4j: Option<Neo4jClient>,
    config: HealingConfig,
}

impl HealingOrchestrator {
    /// Create a new healing orchestrator with just HTTP capabilities.
    pub fn new(http: HttpExecutor) -> Self {
        Self {
            http,
            llm: None,
            neo4j: None,
            config: HealingConfig::default(),
        }
    }

    /// Create a healing orchestrator with all components.
    pub fn with_all(http: HttpExecutor, llm: LlmClient, neo4j: Neo4jClient) -> Self {
        Self {
            http,
            llm: Some(llm),
            neo4j: Some(neo4j),
            config: HealingConfig::default(),
        }
    }

    /// Set the LLM client.
    pub fn with_llm(mut self, llm: LlmClient) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Set the Neo4j client.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.neo4j = Some(neo4j);
        self
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: HealingConfig) -> Self {
        self.config = config;
        self
    }

    /// Execute a request with automatic healing.
    ///
    /// This implements the self-healing loop:
    /// 1. Execute the request
    /// 2. If successful (2xx), mark endpoint as verified
    /// 3. If error (4xx/5xx):
    ///    a. Use LLM to analyze the error
    ///    b. If LLM suggests a fix, apply it and retry
    ///    c. If retry succeeds, create healing event
    ///    d. If still failing, mark endpoint as broken
    pub async fn execute_with_healing(
        &self,
        endpoint: &Endpoint,
        context: &RequestContext,
    ) -> Result<HealingResult, HealingError> {
        let mut attempts = 0;
        let mut healing_events: Vec<HealingEvent> = Vec::new();
        let mut current_body = context.body.clone();
        let mut last_analysis: Option<ErrorAnalysis> = None;

        loop {
            attempts += 1;

            // Build the request
            let mut builder = RequestBuilder::from_endpoint(endpoint, &context.base_url)
                .path_params(context.path_params.clone())
                .query_params(context.query_params.clone())
                .headers(context.headers.clone());

            if let Some(body) = &current_body {
                builder = builder.body(body.clone());
            }

            // Execute the request
            debug!(
                endpoint_id = %endpoint.id,
                path = %endpoint.path,
                method = %endpoint.method,
                attempt = attempts,
                "Executing request"
            );

            let response = self.http.execute(&builder).await?;

            // Check if successful
            if response.is_success() {
                info!(
                    endpoint_id = %endpoint.id,
                    status = response.status_code,
                    attempts = attempts,
                    healed = !healing_events.is_empty(),
                    "Request successful"
                );

                // Update endpoint status (verified regardless of whether healed)
                let status = EndpointStatus::Verified;

                self.update_endpoint_status(endpoint.id, status, response.status_code)
                    .await?;

                // If we healed, verify the healing events
                for event in &healing_events {
                    self.verify_healing_event(event.id).await?;
                }

                // Mark as healed if we applied fixes
                if !healing_events.is_empty() {
                    self.mark_endpoint_healed(endpoint.id).await?;
                }

                return Ok(HealingResult {
                    response: HttpResponseSummary::from(&response),
                    success: true,
                    healed: !healing_events.is_empty(),
                    attempts,
                    healing_events: healing_events
                        .iter()
                        .map(HealingEventSummary::from)
                        .collect(),
                    endpoint_status: status,
                    analysis: last_analysis.as_ref().map(ErrorAnalysisSummary::from),
                });
            }

            // Request failed - check if we should try healing
            if attempts >= self.config.max_retries {
                warn!(
                    endpoint_id = %endpoint.id,
                    status = response.status_code,
                    attempts = attempts,
                    "Max retries exceeded"
                );

                let status = EndpointStatus::Broken;
                self.update_endpoint_status(endpoint.id, status, response.status_code)
                    .await?;

                return Ok(HealingResult {
                    response: HttpResponseSummary::from(&response),
                    success: false,
                    healed: false,
                    attempts,
                    healing_events: healing_events
                        .iter()
                        .map(HealingEventSummary::from)
                        .collect(),
                    endpoint_status: status,
                    analysis: last_analysis.as_ref().map(ErrorAnalysisSummary::from),
                });
            }

            // Try to heal if enabled and LLM is available
            if !self.config.auto_heal {
                let status = EndpointStatus::Broken;
                self.update_endpoint_status(endpoint.id, status, response.status_code)
                    .await?;

                return Ok(HealingResult {
                    response: HttpResponseSummary::from(&response),
                    success: false,
                    healed: false,
                    attempts,
                    healing_events: Vec::new(),
                    endpoint_status: status,
                    analysis: None,
                });
            }

            let llm = match &self.llm {
                Some(llm) => llm,
                None => {
                    warn!("No LLM client available for healing");
                    let status = EndpointStatus::Broken;
                    self.update_endpoint_status(endpoint.id, status, response.status_code)
                        .await?;

                    return Ok(HealingResult {
                        response: HttpResponseSummary::from(&response),
                        success: false,
                        healed: false,
                        attempts,
                        healing_events: Vec::new(),
                        endpoint_status: status,
                        analysis: None,
                    });
                }
            };

            // Get schema info for context
            let schema_info = self.get_endpoint_schema_info(endpoint.id).await?;

            // Analyze the error
            info!(
                endpoint_id = %endpoint.id,
                status = response.status_code,
                "Analyzing error with LLM"
            );

            let analysis = llm
                .analyze_error(
                    &endpoint.path,
                    &endpoint.method.to_string(),
                    current_body.as_ref(),
                    response.status_code,
                    &response.body,
                    schema_info.as_deref(),
                )
                .await?;

            last_analysis = Some(analysis.clone());

            // Check if it's a documentation issue we can fix
            if !analysis.is_doc_issue {
                info!(
                    endpoint_id = %endpoint.id,
                    reasoning = %analysis.reasoning,
                    "Error is not a documentation issue"
                );

                let status = EndpointStatus::Broken;
                self.update_endpoint_status(endpoint.id, status, response.status_code)
                    .await?;

                return Ok(HealingResult {
                    response: HttpResponseSummary::from(&response),
                    success: false,
                    healed: false,
                    attempts,
                    healing_events: Vec::new(),
                    endpoint_status: status,
                    analysis: Some(ErrorAnalysisSummary::from(&analysis)),
                });
            }

            // Check confidence threshold
            if analysis.confidence < self.config.min_confidence {
                info!(
                    endpoint_id = %endpoint.id,
                    confidence = analysis.confidence,
                    threshold = self.config.min_confidence,
                    "Confidence too low for automatic healing"
                );

                let status = EndpointStatus::DocumentationInvalid;
                self.update_endpoint_status(endpoint.id, status, response.status_code)
                    .await?;

                return Ok(HealingResult {
                    response: HttpResponseSummary::from(&response),
                    success: false,
                    healed: false,
                    attempts,
                    healing_events: Vec::new(),
                    endpoint_status: status,
                    analysis: Some(ErrorAnalysisSummary::from(&analysis)),
                });
            }

            // Apply the suggested fix
            if let Some(action) = &analysis.suggested_action {
                info!(
                    endpoint_id = %endpoint.id,
                    action = ?action,
                    confidence = analysis.confidence,
                    "Applying healing action"
                );

                // Create healing event (unverified until retry succeeds)
                let event = HealingEvent::unverified(
                    endpoint.id,
                    action.clone(),
                    &response.body,
                    &analysis.reasoning,
                );

                // Apply to graph if enabled
                if self.config.apply_to_graph {
                    self.apply_healing_action(endpoint.id, action).await?;
                    self.create_healing_event(&event).await?;
                }

                healing_events.push(event);
            }

            // Use corrected body if provided
            if let Some(corrected) = &analysis.corrected_body {
                debug!(
                    endpoint_id = %endpoint.id,
                    "Using corrected request body from LLM"
                );
                current_body = Some(corrected.clone());
            }
        }
    }

    /// Execute a simple request without healing (for testing).
    pub async fn execute_simple(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> Result<HttpResponse, HealingError> {
        let mut builder = RequestBuilder::new().base_url(url).method(method);

        if let Some(body) = body {
            builder = builder.body(body);
        }

        Ok(self.http.execute(&builder).await?)
    }

    // ========================================================================
    // Helper methods for Neo4j operations
    // ========================================================================

    async fn update_endpoint_status(
        &self,
        endpoint_id: Uuid,
        status: EndpointStatus,
        http_status: u16,
    ) -> Result<(), HealingError> {
        if let Some(neo4j) = &self.neo4j {
            neo4j
                .update_endpoint_status(endpoint_id, status, Some(http_status))
                .await?;
        }
        Ok(())
    }

    async fn mark_endpoint_healed(&self, endpoint_id: Uuid) -> Result<(), HealingError> {
        if let Some(neo4j) = &self.neo4j {
            neo4j.mark_endpoint_healed(endpoint_id).await?;
        }
        Ok(())
    }

    async fn verify_healing_event(&self, event_id: Uuid) -> Result<(), HealingError> {
        if let Some(neo4j) = &self.neo4j {
            neo4j.verify_healing_event(event_id).await?;
        }
        Ok(())
    }

    async fn create_healing_event(&self, event: &HealingEvent) -> Result<(), HealingError> {
        if let Some(neo4j) = &self.neo4j {
            neo4j.create_healing_event(event).await?;
        }
        Ok(())
    }

    async fn apply_healing_action(
        &self,
        endpoint_id: Uuid,
        action: &HealingAction,
    ) -> Result<(), HealingError> {
        if let Some(neo4j) = &self.neo4j {
            neo4j.apply_healing_action(endpoint_id, action).await?;
        }
        Ok(())
    }

    async fn get_endpoint_schema_info(
        &self,
        endpoint_id: Uuid,
    ) -> Result<Option<String>, HealingError> {
        let Some(neo4j) = &self.neo4j else {
            return Ok(None);
        };

        // Get parameters for the endpoint to provide context
        let params = neo4j.get_parameters_for_endpoint(endpoint_id).await?;

        if params.is_empty() {
            return Ok(None);
        }

        let info = params
            .iter()
            .map(|p| {
                format!(
                    "- {}: {} (location: {}, required: {})",
                    p.name,
                    p.param_type.as_deref().unwrap_or("unknown"),
                    p.location,
                    p.required
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(Some(format!("Parameters:\n{}", info)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healing_config_default() {
        let config = HealingConfig::default();
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.min_confidence, 0.7);
        assert!(config.auto_heal);
        assert!(config.apply_to_graph);
    }

    #[test]
    fn test_healing_config_analysis_only() {
        let config = HealingConfig::analysis_only();
        assert!(!config.auto_heal);
        assert!(!config.apply_to_graph);
    }

    #[test]
    fn test_request_context_builder() {
        let context = RequestContext::new("https://api.example.com")
            .with_path_param("id", "123")
            .with_query_param("limit", "10")
            .with_header("Authorization", "Bearer token")
            .with_body(serde_json::json!({"key": "value"}));

        assert_eq!(context.base_url, "https://api.example.com");
        assert_eq!(context.path_params.get("id"), Some(&"123".to_string()));
        assert_eq!(context.query_params.get("limit"), Some(&"10".to_string()));
        assert_eq!(
            context.headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
        assert!(context.body.is_some());
    }

    #[test]
    fn test_http_response_summary_from() {
        let response = HttpResponse {
            status_code: 200,
            class: crate::services::http::ResponseClass::Success,
            body: "OK".to_string(),
            headers: HashMap::new(),
            duration_ms: 100,
            url: "http://test".to_string(),
            method: "GET".to_string(),
        };

        let summary = HttpResponseSummary::from(&response);
        assert_eq!(summary.status_code, 200);
        assert_eq!(summary.body, "OK");
        assert_eq!(summary.duration_ms, 100);
    }

    #[test]
    fn test_healing_event_summary_from() {
        let event = HealingEvent::new(
            Uuid::new_v4(),
            HealingAction::RenameParameter {
                old_name: "id".to_string(),
                new_name: "user_id".to_string(),
                param_id: Uuid::nil(),
            },
            "Missing user_id",
            "Error indicates user_id is required",
        );

        let summary = HealingEventSummary::from(&event);
        assert_eq!(summary.id, event.id);
        assert!(summary.verified); // HealingEvent::new creates verified events
        assert!(!summary.reasoning.is_empty());
    }

    #[test]
    fn test_error_analysis_summary_from() {
        let analysis = ErrorAnalysis {
            is_doc_issue: true,
            suggested_action: Some(HealingAction::RenameParameter {
                old_name: "id".to_string(),
                new_name: "user_id".to_string(),
                param_id: Uuid::nil(),
            }),
            reasoning: "The error indicates a parameter mismatch".to_string(),
            confidence: 0.85,
            corrected_body: None,
        };

        let summary = ErrorAnalysisSummary::from(&analysis);
        assert!(summary.is_doc_issue);
        assert_eq!(summary.confidence, 0.85);
        assert!(summary.suggested_action.is_some());
    }

    #[test]
    fn test_healing_orchestrator_creation() {
        let http = HttpExecutor::new().unwrap();
        let orchestrator = HealingOrchestrator::new(http);
        assert!(orchestrator.llm.is_none());
        assert!(orchestrator.neo4j.is_none());
    }

    #[test]
    fn test_healing_orchestrator_with_config() {
        let http = HttpExecutor::new().unwrap();
        let config = HealingConfig {
            max_retries: 5,
            min_confidence: 0.9,
            auto_heal: false,
            apply_to_graph: false,
        };

        let orchestrator = HealingOrchestrator::new(http).with_config(config);
        assert_eq!(orchestrator.config.max_retries, 5);
        assert_eq!(orchestrator.config.min_confidence, 0.9);
        assert!(!orchestrator.config.auto_heal);
    }
}
