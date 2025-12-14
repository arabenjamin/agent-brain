//! OpenAPI specification discovery service.
//!
//! This module provides automatic discovery of OpenAPI specifications by:
//! - Probing common spec locations (e.g., /openapi.json, /swagger.json)
//! - Parsing HTML pages for OpenAPI links
//! - Using LLM to analyze responses and suggest spec locations

use std::collections::HashSet;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use url::Url;

use super::llm::{ChatMessage, LlmClient};

// ============================================================================
// Types
// ============================================================================

/// Result of discovering OpenAPI specifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    /// Base URL that was searched
    pub base_url: String,
    /// Discovered OpenAPI spec URLs with confidence scores
    pub candidates: Vec<DiscoveryCandidate>,
    /// URLs that were probed but didn't contain specs
    pub probed_urls: Vec<String>,
    /// Any errors encountered during discovery
    pub errors: Vec<String>,
}

/// A candidate OpenAPI specification URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryCandidate {
    /// The URL where the spec was found
    pub url: String,
    /// How the spec was discovered
    pub method: DiscoveryMethod,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Detected format (json/yaml)
    pub format: Option<String>,
    /// API title if detected from spec
    pub api_title: Option<String>,
    /// API version if detected from spec
    pub api_version: Option<String>,
}

/// How a spec was discovered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DiscoveryMethod {
    /// Found at a common well-known path
    CommonPath,
    /// Found via link in HTML page
    HtmlLink,
    /// Suggested by LLM analysis
    LlmSuggestion,
    /// Found in API response headers or metadata
    ApiMetadata,
}

/// Configuration for the discovery service.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Timeout for each HTTP request
    pub request_timeout: Duration,
    /// Maximum number of URLs to probe
    pub max_probes: usize,
    /// Whether to parse HTML pages for links
    pub parse_html: bool,
    /// Whether to use LLM for smart suggestions
    pub use_llm: bool,
    /// Whether to validate discovered specs
    pub validate_specs: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            max_probes: 20,
            parse_html: true,
            use_llm: true,
            validate_specs: true,
        }
    }
}

// ============================================================================
// Discovery Service
// ============================================================================

/// Service for discovering OpenAPI specifications.
pub struct DiscoveryService {
    client: Client,
    config: DiscoveryConfig,
    llm: Option<LlmClient>,
}

impl DiscoveryService {
    /// Create a new discovery service.
    pub fn new() -> Result<Self, DiscoveryError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("agent-api/0.1.0 (OpenAPI Discovery)")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| DiscoveryError::HttpClient(e.to_string()))?;

        Ok(Self {
            client,
            config: DiscoveryConfig::default(),
            llm: None,
        })
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: DiscoveryConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the LLM client for smart discovery.
    pub fn with_llm(mut self, llm: LlmClient) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Discover OpenAPI specifications from a base URL.
    pub async fn discover(&self, base_url: &str) -> Result<DiscoveryResult, DiscoveryError> {
        let base = Url::parse(base_url).map_err(|e| DiscoveryError::InvalidUrl(e.to_string()))?;

        info!(base_url = %base_url, "Starting OpenAPI discovery");

        let mut result = DiscoveryResult {
            base_url: base_url.to_string(),
            candidates: Vec::new(),
            probed_urls: Vec::new(),
            errors: Vec::new(),
        };

        let mut seen_urls: HashSet<String> = HashSet::new();

        // Phase 1: Probe common paths
        let common_candidates = self.probe_common_paths(&base, &mut seen_urls).await;
        for candidate in common_candidates {
            if !result.candidates.iter().any(|c| c.url == candidate.url) {
                result.candidates.push(candidate);
            }
        }

        // Phase 2: Parse HTML pages for links
        if self.config.parse_html {
            let html_candidates = self.discover_from_html(&base, &mut seen_urls).await;
            for candidate in html_candidates {
                if !result.candidates.iter().any(|c| c.url == candidate.url) {
                    result.candidates.push(candidate);
                }
            }
        }

        // Phase 3: LLM-assisted discovery
        if self.config.use_llm && self.llm.is_some() {
            match self.llm_assisted_discovery(&base, &result.candidates, &mut seen_urls).await {
                Ok(llm_candidates) => {
                    for candidate in llm_candidates {
                        if !result.candidates.iter().any(|c| c.url == candidate.url) {
                            result.candidates.push(candidate);
                        }
                    }
                }
                Err(e) => {
                    result.errors.push(format!("LLM discovery failed: {}", e));
                }
            }
        }

        // Sort by confidence
        result.candidates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        result.probed_urls = seen_urls.into_iter().collect();

        info!(
            candidates = result.candidates.len(),
            probed = result.probed_urls.len(),
            "Discovery complete"
        );

        Ok(result)
    }

    /// Probe common OpenAPI spec paths.
    async fn probe_common_paths(
        &self,
        base: &Url,
        seen: &mut HashSet<String>,
    ) -> Vec<DiscoveryCandidate> {
        let common_paths = [
            // OpenAPI 3.x common paths
            "/openapi.json",
            "/openapi.yaml",
            "/openapi.yml",
            "/api/openapi.json",
            "/api/openapi.yaml",
            "/docs/openapi.json",
            "/docs/openapi.yaml",
            // Swagger 2.x common paths
            "/swagger.json",
            "/swagger.yaml",
            "/swagger.yml",
            "/api/swagger.json",
            "/docs/swagger.json",
            "/v2/swagger.json",
            // Versioned paths
            "/v1/openapi.json",
            "/v2/openapi.json",
            "/v3/openapi.json",
            "/api/v1/openapi.json",
            "/api/v2/openapi.json",
            "/api/v3/openapi.json",
            // API documentation endpoints
            "/api-docs",
            "/api-docs.json",
            "/api/api-docs",
            "/swagger/v1/swagger.json",
            "/swagger/v2/swagger.json",
            // .well-known
            "/.well-known/openapi.json",
            "/.well-known/schema-discovery",
        ];

        let mut candidates = Vec::new();

        for path in common_paths {
            if candidates.len() >= self.config.max_probes {
                break;
            }

            let url = match base.join(path) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let url_str = url.to_string();
            if seen.contains(&url_str) {
                continue;
            }
            seen.insert(url_str.clone());

            debug!(url = %url_str, "Probing common path");

            if let Some(candidate) = self.probe_url(&url_str, DiscoveryMethod::CommonPath).await {
                candidates.push(candidate);
            }
        }

        candidates
    }

    /// Discover specs by parsing HTML documentation pages.
    async fn discover_from_html(
        &self,
        base: &Url,
        seen: &mut HashSet<String>,
    ) -> Vec<DiscoveryCandidate> {
        let doc_paths = [
            "/",
            "/docs",
            "/documentation",
            "/api",
            "/api/docs",
            "/developer",
            "/developers",
            "/reference",
            "/api-reference",
        ];

        let mut candidates = Vec::new();

        for path in doc_paths {
            let url = match base.join(path) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let url_str = url.to_string();
            debug!(url = %url_str, "Checking HTML page for OpenAPI links");

            match self.fetch_html(&url_str).await {
                Ok(html) => {
                    let links = self.extract_openapi_links(&html, &url);
                    for link in links {
                        if seen.contains(&link) {
                            continue;
                        }
                        seen.insert(link.clone());

                        if let Some(candidate) =
                            self.probe_url(&link, DiscoveryMethod::HtmlLink).await
                        {
                            candidates.push(candidate);
                        }
                    }
                }
                Err(e) => {
                    debug!(url = %url_str, error = %e, "Failed to fetch HTML");
                }
            }
        }

        candidates
    }

    /// Use LLM to suggest potential spec locations.
    async fn llm_assisted_discovery(
        &self,
        base: &Url,
        existing_candidates: &[DiscoveryCandidate],
        seen: &mut HashSet<String>,
    ) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
        let llm = self.llm.as_ref().ok_or(DiscoveryError::LlmNotConfigured)?;

        // First, try to get some context about the API
        let api_context = self.gather_api_context(base).await;

        let prompt = self.build_llm_prompt(base, &api_context, existing_candidates);

        debug!("Asking LLM for OpenAPI spec suggestions");

        let response = llm
            .chat(&[ChatMessage::user(&prompt)])
            .await
            .map_err(|e| DiscoveryError::LlmError(e.to_string()))?;

        let suggested_paths = self.parse_llm_suggestions(&response.text);

        let mut candidates = Vec::new();

        for path in suggested_paths {
            let url = match base.join(&path) {
                Ok(u) => u.to_string(),
                Err(_) => {
                    // Try as absolute URL
                    if path.starts_with("http") {
                        path
                    } else {
                        continue;
                    }
                }
            };

            if seen.contains(&url) {
                continue;
            }
            seen.insert(url.clone());

            debug!(url = %url, "Probing LLM-suggested path");

            if let Some(candidate) = self.probe_url(&url, DiscoveryMethod::LlmSuggestion).await {
                candidates.push(candidate);
            }
        }

        Ok(candidates)
    }

    /// Gather context about the API to help LLM make better suggestions.
    async fn gather_api_context(&self, base: &Url) -> String {
        let mut context = String::new();

        // Try to fetch the root page
        if let Ok(response) = self
            .client
            .get(base.as_str())
            .send()
            .await
        {
            // Check for API-related headers
            if let Some(link) = response.headers().get("link") {
                if let Ok(link_str) = link.to_str() {
                    context.push_str(&format!("Link header: {}\n", link_str));
                }
            }

            // Check content type
            if let Some(ct) = response.headers().get("content-type") {
                if let Ok(ct_str) = ct.to_str() {
                    context.push_str(&format!("Content-Type: {}\n", ct_str));
                }
            }

            // Get a snippet of the response body
            if let Ok(body) = response.text().await {
                let snippet: String = body.chars().take(1000).collect();
                context.push_str(&format!("Response snippet:\n{}\n", snippet));
            }
        }

        context
    }

    /// Build the prompt for LLM-assisted discovery.
    fn build_llm_prompt(
        &self,
        base: &Url,
        api_context: &str,
        existing_candidates: &[DiscoveryCandidate],
    ) -> String {
        let existing_info = if existing_candidates.is_empty() {
            "No OpenAPI specs found yet at common locations.".to_string()
        } else {
            format!(
                "Already found {} candidate(s): {}",
                existing_candidates.len(),
                existing_candidates
                    .iter()
                    .map(|c| c.url.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        format!(
            r#"I'm trying to find OpenAPI/Swagger specification files for an API.

Base URL: {}

{}

Context gathered from the API:
{}

Based on this information, suggest additional paths where OpenAPI specs might be located.
Consider:
- Framework-specific paths (FastAPI uses /openapi.json, Spring uses /v3/api-docs)
- Version-specific paths
- Documentation paths that might contain spec links
- Any patterns visible in the context

Return ONLY a list of paths to try, one per line, starting with /
Do not include explanations, just the paths.
Example format:
/api/v1/openapi.json
/docs/api-spec.yaml
/swagger/doc.json"#,
            base, existing_info, api_context
        )
    }

    /// Parse LLM response to extract suggested paths.
    fn parse_llm_suggestions(&self, response: &str) -> Vec<String> {
        response
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .filter(|line| line.starts_with('/') || line.starts_with("http"))
            .map(|line| line.to_string())
            .take(10) // Limit suggestions
            .collect()
    }

    /// Probe a URL to check if it contains an OpenAPI spec.
    async fn probe_url(&self, url: &str, method: DiscoveryMethod) -> Option<DiscoveryCandidate> {
        let response = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                debug!(url = %url, error = %e, "Failed to fetch URL");
                return None;
            }
        };

        if response.status() != StatusCode::OK {
            debug!(url = %url, status = %response.status(), "Non-OK status");
            return None;
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = match response.text().await {
            Ok(b) => b,
            Err(_) => return None,
        };

        // Try to detect if this is an OpenAPI spec
        let (is_openapi, format, api_title, api_version) = self.validate_openapi_content(&body, &content_type);

        if !is_openapi {
            debug!(url = %url, "Content is not an OpenAPI spec");
            return None;
        }

        let confidence = match method {
            DiscoveryMethod::CommonPath => 0.9,
            DiscoveryMethod::HtmlLink => 0.8,
            DiscoveryMethod::LlmSuggestion => 0.7,
            DiscoveryMethod::ApiMetadata => 0.85,
        };

        info!(url = %url, method = ?method, title = ?api_title, "Found OpenAPI spec");

        Some(DiscoveryCandidate {
            url: url.to_string(),
            method,
            confidence,
            format,
            api_title,
            api_version,
        })
    }

    /// Validate if content is an OpenAPI spec and extract metadata.
    fn validate_openapi_content(
        &self,
        content: &str,
        content_type: &str,
    ) -> (bool, Option<String>, Option<String>, Option<String>) {
        // Try JSON first
        if content_type.contains("json") || content.trim().starts_with('{') {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
                return self.validate_openapi_json(&json, "json");
            }
        }

        // Try YAML
        if content_type.contains("yaml")
            || content_type.contains("yml")
            || content.contains("openapi:")
            || content.contains("swagger:")
        {
            if let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(content) {
                return self.validate_openapi_json(&yaml, "yaml");
            }
        }

        (false, None, None, None)
    }

    /// Validate JSON/YAML content as OpenAPI spec.
    fn validate_openapi_json(
        &self,
        value: &serde_json::Value,
        format: &str,
    ) -> (bool, Option<String>, Option<String>, Option<String>) {
        // Check for OpenAPI 3.x
        if value.get("openapi").is_some() {
            let title = value
                .get("info")
                .and_then(|i| i.get("title"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());

            let version = value
                .get("info")
                .and_then(|i| i.get("version"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            return (true, Some(format.to_string()), title, version);
        }

        // Check for Swagger 2.x
        if value.get("swagger").is_some() {
            let title = value
                .get("info")
                .and_then(|i| i.get("title"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());

            let version = value
                .get("info")
                .and_then(|i| i.get("version"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            return (true, Some(format.to_string()), title, version);
        }

        (false, None, None, None)
    }

    /// Fetch HTML content from a URL.
    async fn fetch_html(&self, url: &str) -> Result<String, DiscoveryError> {
        let response = self
            .client
            .get(url)
            .header("Accept", "text/html")
            .send()
            .await
            .map_err(|e| DiscoveryError::HttpClient(e.to_string()))?;

        if !response.status().is_success() {
            return Err(DiscoveryError::HttpClient(format!(
                "Status: {}",
                response.status()
            )));
        }

        response
            .text()
            .await
            .map_err(|e| DiscoveryError::HttpClient(e.to_string()))
    }

    /// Extract potential OpenAPI links from HTML.
    fn extract_openapi_links(&self, html: &str, base: &Url) -> Vec<String> {
        let document = Html::parse_document(html);
        let mut links = Vec::new();

        // Look for links with OpenAPI-related text
        let link_selector = Selector::parse("a[href]").unwrap();
        let openapi_keywords = [
            "openapi",
            "swagger",
            "api-docs",
            "api-spec",
            "spec.json",
            "spec.yaml",
            "schema",
        ];

        for element in document.select(&link_selector) {
            if let Some(href) = element.value().attr("href") {
                let href_lower = href.to_lowercase();
                let text_lower = element.text().collect::<String>().to_lowercase();

                let is_openapi_link = openapi_keywords
                    .iter()
                    .any(|kw| href_lower.contains(kw) || text_lower.contains(kw));

                if is_openapi_link {
                    if let Ok(absolute) = base.join(href) {
                        links.push(absolute.to_string());
                    }
                }
            }
        }

        // Also look for script tags or data attributes with spec URLs
        let script_selector = Selector::parse("script").unwrap();
        for element in document.select(&script_selector) {
            let script_text = element.text().collect::<String>();
            // Look for URLs in script content
            for keyword in &openapi_keywords {
                if script_text.contains(keyword) {
                    // Simple URL extraction from script
                    for word in script_text.split(&['"', '\'', ' ', '\n'][..]) {
                        if (word.starts_with('/') || word.starts_with("http"))
                            && openapi_keywords.iter().any(|kw| word.contains(kw))
                        {
                            if let Ok(absolute) = base.join(word) {
                                links.push(absolute.to_string());
                            }
                        }
                    }
                }
            }
        }

        links
    }
}

impl Default for DiscoveryService {
    fn default() -> Self {
        Self::new().expect("Failed to create default DiscoveryService")
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Errors that can occur during discovery.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("HTTP client error: {0}")]
    HttpClient(String),

    #[error("LLM not configured")]
    LlmNotConfigured,

    #[error("LLM error: {0}")]
    LlmError(String),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discovery_config_default() {
        let config = DiscoveryConfig::default();
        assert_eq!(config.request_timeout, Duration::from_secs(10));
        assert_eq!(config.max_probes, 20);
        assert!(config.parse_html);
        assert!(config.use_llm);
        assert!(config.validate_specs);
    }

    #[test]
    fn test_validate_openapi_json_v3() {
        let service = DiscoveryService::new().unwrap();
        let json = serde_json::json!({
            "openapi": "3.0.0",
            "info": {
                "title": "Test API",
                "version": "1.0.0"
            }
        });

        let (is_valid, format, title, version) = service.validate_openapi_json(&json, "json");
        assert!(is_valid);
        assert_eq!(format, Some("json".to_string()));
        assert_eq!(title, Some("Test API".to_string()));
        assert_eq!(version, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_validate_openapi_json_swagger() {
        let service = DiscoveryService::new().unwrap();
        let json = serde_json::json!({
            "swagger": "2.0",
            "info": {
                "title": "Legacy API",
                "version": "2.0.0"
            }
        });

        let (is_valid, format, title, version) = service.validate_openapi_json(&json, "json");
        assert!(is_valid);
        assert_eq!(title, Some("Legacy API".to_string()));
        assert_eq!(version, Some("2.0.0".to_string()));
    }

    #[test]
    fn test_validate_openapi_json_invalid() {
        let service = DiscoveryService::new().unwrap();
        let json = serde_json::json!({
            "name": "Not an API spec",
            "data": []
        });

        let (is_valid, _, _, _) = service.validate_openapi_json(&json, "json");
        assert!(!is_valid);
    }

    #[test]
    fn test_parse_llm_suggestions() {
        let service = DiscoveryService::new().unwrap();
        let response = r#"
/api/v1/openapi.json
/docs/swagger.yaml
/v3/api-docs
Not a path
/another/path.json
"#;

        let suggestions = service.parse_llm_suggestions(response);
        assert_eq!(suggestions.len(), 4);
        assert_eq!(suggestions[0], "/api/v1/openapi.json");
        assert_eq!(suggestions[1], "/docs/swagger.yaml");
    }

    #[test]
    fn test_extract_openapi_links() {
        let service = DiscoveryService::new().unwrap();
        let base = Url::parse("https://api.example.com").unwrap();
        let html = r#"
        <html>
            <body>
                <a href="/openapi.json">API Spec</a>
                <a href="/docs/swagger.yaml">Swagger Docs</a>
                <a href="/about">About Us</a>
            </body>
        </html>
        "#;

        let links = service.extract_openapi_links(html, &base);
        assert_eq!(links.len(), 2);
        assert!(links.contains(&"https://api.example.com/openapi.json".to_string()));
        assert!(links.contains(&"https://api.example.com/docs/swagger.yaml".to_string()));
    }

    #[test]
    fn test_discovery_candidate_serialization() {
        let candidate = DiscoveryCandidate {
            url: "https://api.example.com/openapi.json".to_string(),
            method: DiscoveryMethod::CommonPath,
            confidence: 0.9,
            format: Some("json".to_string()),
            api_title: Some("Test API".to_string()),
            api_version: Some("1.0.0".to_string()),
        };

        let json = serde_json::to_string(&candidate).unwrap();
        assert!(json.contains("CommonPath"));
        assert!(json.contains("0.9"));
    }
}
