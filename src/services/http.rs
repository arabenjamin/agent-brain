use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::models::{Endpoint, EndpointStatus, HttpMethod};
use crate::repository::Neo4jClient;

/// Default request timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default connection timeout in seconds.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("Request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Invalid method: {0}")]
    InvalidMethod(String),

    #[error("Invalid header: {0}")]
    InvalidHeader(String),

    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Classification of HTTP response status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseClass {
    /// 1xx Informational
    Informational,
    /// 2xx Success
    Success,
    /// 3xx Redirection
    Redirection,
    /// 4xx Client Error
    ClientError,
    /// 5xx Server Error
    ServerError,
}

impl ResponseClass {
    /// Classify a status code.
    pub fn from_status(status: u16) -> Self {
        match status {
            100..=199 => ResponseClass::Informational,
            200..=299 => ResponseClass::Success,
            300..=399 => ResponseClass::Redirection,
            400..=499 => ResponseClass::ClientError,
            _ => ResponseClass::ServerError,
        }
    }

    /// Returns true if this is a successful response (2xx).
    pub fn is_success(&self) -> bool {
        matches!(self, ResponseClass::Success)
    }

    /// Returns true if this is an error response (4xx or 5xx).
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            ResponseClass::ClientError | ResponseClass::ServerError
        )
    }
}

/// Result of executing an HTTP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status_code: u16,

    /// Classification of the status code.
    pub class: ResponseClass,

    /// Response body as text.
    pub body: String,

    /// Response headers.
    pub headers: HashMap<String, String>,

    /// Request duration in milliseconds.
    pub duration_ms: u64,

    /// The URL that was requested.
    pub url: String,

    /// The HTTP method used.
    pub method: String,
}

impl HttpResponse {
    /// Returns true if this is a successful response (2xx).
    pub fn is_success(&self) -> bool {
        self.class.is_success()
    }

    /// Returns true if this is an error response (4xx or 5xx).
    pub fn is_error(&self) -> bool {
        self.class.is_error()
    }

    /// Try to parse the response body as JSON.
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(&self.body)
    }
}

/// Configuration for the HTTP executor.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Request timeout.
    pub timeout: Duration,

    /// Connection timeout.
    pub connect_timeout: Duration,

    /// User-Agent header value.
    pub user_agent: String,

    /// Whether to follow redirects.
    pub follow_redirects: bool,

    /// Maximum number of redirects to follow.
    pub max_redirects: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            user_agent: format!("agent-api/{}", env!("CARGO_PKG_VERSION")),
            follow_redirects: true,
            max_redirects: 10,
        }
    }
}

/// Builder for HTTP requests.
#[derive(Debug, Clone, Default)]
pub struct RequestBuilder {
    pub base_url: Option<String>,
    pub path: Option<String>,
    pub method: Option<HttpMethod>,
    pub headers: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    pub path_params: HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

impl RequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the base URL (e.g., "https://api.example.com").
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set the path (e.g., "/users/{id}").
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the HTTP method.
    pub fn method(mut self, method: HttpMethod) -> Self {
        self.method = Some(method);
        self
    }

    /// Add a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Add multiple headers.
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Add a query parameter.
    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_params.insert(name.into(), value.into());
        self
    }

    /// Add multiple query parameters.
    pub fn query_params(mut self, params: HashMap<String, String>) -> Self {
        self.query_params.extend(params);
        self
    }

    /// Add a path parameter (replaces {name} in path).
    pub fn path_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.path_params.insert(name.into(), value.into());
        self
    }

    /// Add multiple path parameters.
    pub fn path_params(mut self, params: HashMap<String, String>) -> Self {
        self.path_params.extend(params);
        self
    }

    /// Set the request body as JSON.
    pub fn body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Build the full URL with path parameters substituted.
    pub fn build_url(&self) -> Result<String, HttpError> {
        let base = self
            .base_url
            .as_ref()
            .ok_or_else(|| HttpError::InvalidUrl("Base URL is required".to_string()))?;

        let path = self.path.as_deref().unwrap_or("");

        // Substitute path parameters
        let mut resolved_path = path.to_string();
        for (name, value) in &self.path_params {
            resolved_path = resolved_path.replace(&format!("{{{}}}", name), value);
        }

        // Combine base URL and path
        let url = if base.ends_with('/') && resolved_path.starts_with('/') {
            format!("{}{}", base.trim_end_matches('/'), resolved_path)
        } else if !base.ends_with('/')
            && !resolved_path.starts_with('/')
            && !resolved_path.is_empty()
        {
            format!("{}/{}", base, resolved_path)
        } else {
            format!("{}{}", base, resolved_path)
        };

        Ok(url)
    }

    /// Create a RequestBuilder from an Endpoint.
    pub fn from_endpoint(endpoint: &Endpoint, base_url: impl Into<String>) -> Self {
        Self::new()
            .base_url(base_url)
            .path(&endpoint.path)
            .method(endpoint.method)
    }
}

/// HTTP executor for making API requests.
pub struct HttpExecutor {
    client: Client,
    config: HttpConfig,
    neo4j: Option<Neo4jClient>,
}

impl HttpExecutor {
    /// Create a new HTTP executor with default configuration.
    pub fn new() -> Result<Self, HttpError> {
        Self::with_config(HttpConfig::default())
    }

    /// Create a new HTTP executor with custom configuration.
    pub fn with_config(config: HttpConfig) -> Result<Self, HttpError> {
        let mut client_builder = Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .user_agent(&config.user_agent);

        if config.follow_redirects {
            client_builder =
                client_builder.redirect(reqwest::redirect::Policy::limited(config.max_redirects));
        } else {
            client_builder = client_builder.redirect(reqwest::redirect::Policy::none());
        }

        let client = client_builder.build()?;

        Ok(Self {
            client,
            config,
            neo4j: None,
        })
    }

    /// Attach a Neo4j client for endpoint status updates.
    pub fn with_neo4j(mut self, client: Neo4jClient) -> Self {
        self.neo4j = Some(client);
        self
    }

    /// Get the current configuration.
    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    /// Execute a request using the RequestBuilder.
    pub async fn execute(&self, builder: &RequestBuilder) -> Result<HttpResponse, HttpError> {
        let url = builder.build_url()?;
        let method = builder
            .method
            .ok_or_else(|| HttpError::InvalidMethod("Method is required".to_string()))?;

        self.execute_raw(
            method,
            &url,
            builder.headers.clone(),
            builder.query_params.clone(),
            builder.body.clone(),
        )
        .await
    }

    /// Execute a raw HTTP request.
    pub async fn execute_raw(
        &self,
        method: HttpMethod,
        url: &str,
        headers: HashMap<String, String>,
        query_params: HashMap<String, String>,
        body: Option<serde_json::Value>,
    ) -> Result<HttpResponse, HttpError> {
        let reqwest_method = match method {
            HttpMethod::Get => Method::GET,
            HttpMethod::Post => Method::POST,
            HttpMethod::Put => Method::PUT,
            HttpMethod::Patch => Method::PATCH,
            HttpMethod::Delete => Method::DELETE,
            HttpMethod::Head => Method::HEAD,
            HttpMethod::Options => Method::OPTIONS,
        };

        debug!(
            method = %method,
            url = %url,
            "Executing HTTP request"
        );

        let start = Instant::now();

        let mut request = self.client.request(reqwest_method.clone(), url);

        // Add headers
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }

        // Add query parameters
        if !query_params.is_empty() {
            request = request.query(&query_params);
        }

        // Add body
        if let Some(body) = &body {
            request = request
                .header("Content-Type", "application/json")
                .json(body);
        }

        let response = request.send().await?;
        let duration = start.elapsed();

        let status_code = response.status().as_u16();
        let class = ResponseClass::from_status(status_code);

        // Collect headers
        let mut response_headers = HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                response_headers.insert(name.to_string(), v.to_string());
            }
        }

        let response_body = response.text().await?;

        info!(
            method = %reqwest_method,
            url = %url,
            status = status_code,
            duration_ms = duration.as_millis() as u64,
            "HTTP request completed"
        );

        Ok(HttpResponse {
            status_code,
            class,
            body: response_body,
            headers: response_headers,
            duration_ms: duration.as_millis() as u64,
            url: url.to_string(),
            method: reqwest_method.to_string(),
        })
    }

    /// Execute a request for an endpoint and update its status in Neo4j.
    pub async fn execute_for_endpoint(
        &self,
        endpoint_id: Uuid,
        builder: &RequestBuilder,
    ) -> Result<HttpResponse, HttpError> {
        let response = self.execute(builder).await?;

        // Update endpoint status if we have a Neo4j client
        if let Some(neo4j) = &self.neo4j {
            let status = if response.is_success() {
                EndpointStatus::Verified
            } else if response.class == ResponseClass::ClientError {
                // 4xx errors might indicate documentation issues
                EndpointStatus::DocumentationInvalid
            } else {
                // 5xx errors indicate the endpoint is broken
                EndpointStatus::Broken
            };

            if let Err(e) = neo4j
                .update_endpoint_status(endpoint_id, status, Some(response.status_code))
                .await
            {
                warn!(
                    endpoint_id = %endpoint_id,
                    error = %e,
                    "Failed to update endpoint status"
                );
            } else {
                debug!(
                    endpoint_id = %endpoint_id,
                    status = ?status,
                    http_status = response.status_code,
                    "Updated endpoint status"
                );
            }
        }

        Ok(response)
    }

    /// Execute a simple GET request.
    pub async fn get(&self, url: &str) -> Result<HttpResponse, HttpError> {
        self.execute_raw(HttpMethod::Get, url, HashMap::new(), HashMap::new(), None)
            .await
    }

    /// Execute a simple POST request with JSON body.
    pub async fn post(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<HttpResponse, HttpError> {
        self.execute_raw(
            HttpMethod::Post,
            url,
            HashMap::new(),
            HashMap::new(),
            Some(body),
        )
        .await
    }

    /// Execute a simple PUT request with JSON body.
    pub async fn put(&self, url: &str, body: serde_json::Value) -> Result<HttpResponse, HttpError> {
        self.execute_raw(
            HttpMethod::Put,
            url,
            HashMap::new(),
            HashMap::new(),
            Some(body),
        )
        .await
    }

    /// Execute a simple DELETE request.
    pub async fn delete(&self, url: &str) -> Result<HttpResponse, HttpError> {
        self.execute_raw(
            HttpMethod::Delete,
            url,
            HashMap::new(),
            HashMap::new(),
            None,
        )
        .await
    }
}

impl Default for HttpExecutor {
    fn default() -> Self {
        Self::new().expect("Failed to create default HTTP executor")
    }
}

/// Parse headers from a "Key: Value" string format.
pub fn parse_header_string(header: &str) -> Result<(String, String), HttpError> {
    let parts: Vec<&str> = header.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(HttpError::InvalidHeader(format!(
            "Invalid header format: {}. Expected 'Key: Value'",
            header
        )));
    }
    Ok((parts[0].trim().to_string(), parts[1].trim().to_string()))
}

/// Parse multiple headers from string format.
pub fn parse_headers(headers: &[String]) -> Result<HashMap<String, String>, HttpError> {
    let mut map = HashMap::new();
    for header in headers {
        let (key, value) = parse_header_string(header)?;
        map.insert(key, value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_class_from_status() {
        assert_eq!(
            ResponseClass::from_status(100),
            ResponseClass::Informational
        );
        assert_eq!(ResponseClass::from_status(200), ResponseClass::Success);
        assert_eq!(ResponseClass::from_status(201), ResponseClass::Success);
        assert_eq!(ResponseClass::from_status(301), ResponseClass::Redirection);
        assert_eq!(ResponseClass::from_status(400), ResponseClass::ClientError);
        assert_eq!(ResponseClass::from_status(404), ResponseClass::ClientError);
        assert_eq!(ResponseClass::from_status(500), ResponseClass::ServerError);
        assert_eq!(ResponseClass::from_status(503), ResponseClass::ServerError);
    }

    #[test]
    fn test_response_class_is_success() {
        assert!(ResponseClass::Success.is_success());
        assert!(!ResponseClass::ClientError.is_success());
        assert!(!ResponseClass::ServerError.is_success());
    }

    #[test]
    fn test_response_class_is_error() {
        assert!(!ResponseClass::Success.is_error());
        assert!(ResponseClass::ClientError.is_error());
        assert!(ResponseClass::ServerError.is_error());
    }

    #[test]
    fn test_request_builder_url() {
        let builder = RequestBuilder::new()
            .base_url("https://api.example.com")
            .path("/users/{id}")
            .path_param("id", "123");

        let url = builder.build_url().unwrap();
        assert_eq!(url, "https://api.example.com/users/123");
    }

    #[test]
    fn test_request_builder_url_trailing_slash() {
        let builder = RequestBuilder::new()
            .base_url("https://api.example.com/")
            .path("/users");

        let url = builder.build_url().unwrap();
        assert_eq!(url, "https://api.example.com/users");
    }

    #[test]
    fn test_request_builder_url_no_path() {
        let builder = RequestBuilder::new().base_url("https://api.example.com");

        let url = builder.build_url().unwrap();
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn test_request_builder_multiple_path_params() {
        let builder = RequestBuilder::new()
            .base_url("https://api.example.com")
            .path("/users/{user_id}/posts/{post_id}")
            .path_param("user_id", "42")
            .path_param("post_id", "99");

        let url = builder.build_url().unwrap();
        assert_eq!(url, "https://api.example.com/users/42/posts/99");
    }

    #[test]
    fn test_request_builder_from_endpoint() {
        let endpoint = Endpoint::new("/users/{id}", HttpMethod::Get, "Get user", None);

        let builder = RequestBuilder::from_endpoint(&endpoint, "https://api.example.com")
            .path_param("id", "123");

        assert_eq!(builder.method, Some(HttpMethod::Get));
        let url = builder.build_url().unwrap();
        assert_eq!(url, "https://api.example.com/users/123");
    }

    #[test]
    fn test_parse_header_string() {
        let (key, value) = parse_header_string("Content-Type: application/json").unwrap();
        assert_eq!(key, "Content-Type");
        assert_eq!(value, "application/json");
    }

    #[test]
    fn test_parse_header_string_with_colon_in_value() {
        let (key, value) = parse_header_string("Authorization: Bearer token:with:colons").unwrap();
        assert_eq!(key, "Authorization");
        assert_eq!(value, "Bearer token:with:colons");
    }

    #[test]
    fn test_parse_header_string_invalid() {
        let result = parse_header_string("InvalidHeader");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_headers() {
        let headers = vec![
            "Content-Type: application/json".to_string(),
            "Authorization: Bearer token".to_string(),
        ];

        let map = parse_headers(&headers).unwrap();
        assert_eq!(
            map.get("Content-Type"),
            Some(&"application/json".to_string())
        );
        assert_eq!(map.get("Authorization"), Some(&"Bearer token".to_string()));
    }

    #[test]
    fn test_http_config_default() {
        let config = HttpConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert!(config.follow_redirects);
        assert_eq!(config.max_redirects, 10);
    }

    #[test]
    fn test_http_executor_creation() {
        let executor = HttpExecutor::new();
        assert!(executor.is_ok());
    }

    #[test]
    fn test_http_executor_with_config() {
        let config = HttpConfig {
            timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(5),
            user_agent: "test-agent/1.0".to_string(),
            follow_redirects: false,
            max_redirects: 5,
        };

        let executor = HttpExecutor::with_config(config.clone());
        assert!(executor.is_ok());

        let executor = executor.unwrap();
        assert_eq!(executor.config().timeout, Duration::from_secs(60));
        assert_eq!(executor.config().connect_timeout, Duration::from_secs(5));
    }
}
