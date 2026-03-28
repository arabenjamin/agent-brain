#![cfg(feature = "http-transport")]
//! Authentication middleware for HTTP transport.
//!
//! This module provides API key authentication using Bearer tokens.

use axum::{
    body::Body,
    extract::Request,
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Error type for authentication failures.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Missing authorization header")]
    MissingHeader,

    #[error("Invalid authorization header format")]
    InvalidFormat,

    #[error("Invalid API key")]
    InvalidKey,

    #[error("Authorization type not supported, expected Bearer")]
    UnsupportedType,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = match &self {
            AuthError::MissingHeader => StatusCode::UNAUTHORIZED,
            AuthError::InvalidFormat => StatusCode::BAD_REQUEST,
            AuthError::InvalidKey => StatusCode::UNAUTHORIZED,
            AuthError::UnsupportedType => StatusCode::BAD_REQUEST,
        };

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32000,
                "message": self.to_string()
            },
            "id": null
        });

        (status, axum::Json(body)).into_response()
    }
}

/// Configuration for API key authentication.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// The expected API key. If None, authentication is disabled.
    pub api_key: Option<String>,
    /// Paths that don't require authentication (e.g., health checks).
    pub excluded_paths: Vec<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            excluded_paths: vec!["/health".to_string()],
        }
    }
}

impl AuthConfig {
    /// Create a new auth config with an API key.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            excluded_paths: vec!["/health".to_string()],
        }
    }

    /// Create a disabled auth config (no authentication required).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Check if a path is excluded from authentication.
    pub fn is_excluded(&self, path: &str) -> bool {
        self.excluded_paths.iter().any(|p| path.starts_with(p))
    }

    /// Check if authentication is enabled.
    pub fn is_enabled(&self) -> bool {
        self.api_key.is_some()
    }
}

/// API key authenticator.
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    config: Arc<AuthConfig>,
}

impl ApiKeyAuth {
    /// Create a new authenticator with the given configuration.
    pub fn new(config: AuthConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Create an authenticator with a specific API key.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self::new(AuthConfig::with_key(api_key))
    }

    /// Create a disabled authenticator (no auth required).
    pub fn disabled() -> Self {
        Self::new(AuthConfig::disabled())
    }

    /// Validate an authorization header value.
    pub fn validate_header(&self, header_value: &str) -> Result<(), AuthError> {
        // Parse "Bearer <token>"
        let parts: Vec<&str> = header_value.splitn(2, ' ').collect();

        if parts.len() != 2 {
            return Err(AuthError::InvalidFormat);
        }

        let auth_type = parts[0];
        let token = parts[1];

        if !auth_type.eq_ignore_ascii_case("bearer") {
            return Err(AuthError::UnsupportedType);
        }

        // Check against expected key
        match &self.config.api_key {
            Some(expected) if expected == token => Ok(()),
            Some(_) => Err(AuthError::InvalidKey),
            None => Ok(()), // Auth disabled
        }
    }

    /// Check if a request is authenticated.
    pub fn authenticate(&self, request: &Request<Body>) -> Result<(), AuthError> {
        // Skip auth for excluded paths
        if self.config.is_excluded(request.uri().path()) {
            return Ok(());
        }

        // Skip if auth is disabled
        if !self.config.is_enabled() {
            return Ok(());
        }

        // Get Authorization header
        let header = request
            .headers()
            .get(AUTHORIZATION)
            .ok_or(AuthError::MissingHeader)?;

        let header_value = header.to_str().map_err(|_| AuthError::InvalidFormat)?;

        self.validate_header(header_value)
    }

    /// Get the configuration.
    pub fn config(&self) -> &AuthConfig {
        &self.config
    }
}

/// Axum middleware layer for API key authentication.
pub async fn auth_middleware(
    auth: Arc<ApiKeyAuth>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, AuthError> {
    auth.authenticate(&request)?;
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::AUTHORIZATION;

    #[test]
    fn test_auth_error_display() {
        assert_eq!(
            AuthError::MissingHeader.to_string(),
            "Missing authorization header"
        );
        assert_eq!(AuthError::InvalidKey.to_string(), "Invalid API key");
    }

    #[test]
    fn test_auth_config_default() {
        let config = AuthConfig::default();
        assert!(config.api_key.is_none());
        assert!(!config.is_enabled());
        assert!(config.is_excluded("/health"));
        assert!(!config.is_excluded("/mcp"));
    }

    #[test]
    fn test_auth_config_with_key() {
        let config = AuthConfig::with_key("test-key");
        assert_eq!(config.api_key, Some("test-key".to_string()));
        assert!(config.is_enabled());
    }

    #[test]
    fn test_auth_config_excluded_paths() {
        let config = AuthConfig {
            api_key: Some("key".to_string()),
            excluded_paths: vec!["/health".to_string(), "/metrics".to_string()],
        };

        assert!(config.is_excluded("/health"));
        assert!(config.is_excluded("/health/ready"));
        assert!(config.is_excluded("/metrics"));
        assert!(!config.is_excluded("/mcp"));
    }

    #[test]
    fn test_api_key_auth_disabled() {
        let auth = ApiKeyAuth::disabled();
        assert!(!auth.config().is_enabled());

        // Should pass without any header
        let result = auth.validate_header("Bearer anything");
        assert!(result.is_ok());
    }

    #[test]
    fn test_api_key_auth_valid_token() {
        let auth = ApiKeyAuth::with_key("secret-key-123");

        let result = auth.validate_header("Bearer secret-key-123");
        assert!(result.is_ok());
    }

    #[test]
    fn test_api_key_auth_invalid_token() {
        let auth = ApiKeyAuth::with_key("secret-key-123");

        let result = auth.validate_header("Bearer wrong-key");
        assert!(matches!(result, Err(AuthError::InvalidKey)));
    }

    #[test]
    fn test_api_key_auth_missing_bearer_prefix() {
        let auth = ApiKeyAuth::with_key("secret-key");

        let result = auth.validate_header("secret-key");
        assert!(matches!(result, Err(AuthError::InvalidFormat)));
    }

    #[test]
    fn test_api_key_auth_wrong_auth_type() {
        let auth = ApiKeyAuth::with_key("secret-key");

        let result = auth.validate_header("Basic c2VjcmV0LWtleQ==");
        assert!(matches!(result, Err(AuthError::UnsupportedType)));
    }

    #[test]
    fn test_api_key_auth_case_insensitive_bearer() {
        let auth = ApiKeyAuth::with_key("key123");

        // Should accept various cases of "Bearer"
        assert!(auth.validate_header("Bearer key123").is_ok());
        assert!(auth.validate_header("bearer key123").is_ok());
        assert!(auth.validate_header("BEARER key123").is_ok());
    }

    #[test]
    fn test_api_key_auth_empty_token() {
        let auth = ApiKeyAuth::with_key("secret");

        let result = auth.validate_header("Bearer ");
        assert!(matches!(result, Err(AuthError::InvalidKey)));
    }

    #[tokio::test]
    async fn test_authenticate_request_excluded_path() {
        let auth = ApiKeyAuth::with_key("secret");

        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        // Should pass without auth header because path is excluded
        let result = auth.authenticate(&request);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_authenticate_request_missing_header() {
        let auth = ApiKeyAuth::with_key("secret");

        let request = Request::builder().uri("/mcp").body(Body::empty()).unwrap();

        let result = auth.authenticate(&request);
        assert!(matches!(result, Err(AuthError::MissingHeader)));
    }

    #[tokio::test]
    async fn test_authenticate_request_valid() {
        let auth = ApiKeyAuth::with_key("my-api-key");

        let request = Request::builder()
            .uri("/mcp")
            .header(AUTHORIZATION, "Bearer my-api-key")
            .body(Body::empty())
            .unwrap();

        let result = auth.authenticate(&request);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_authenticate_request_invalid_key() {
        let auth = ApiKeyAuth::with_key("correct-key");

        let request = Request::builder()
            .uri("/mcp")
            .header(AUTHORIZATION, "Bearer wrong-key")
            .body(Body::empty())
            .unwrap();

        let result = auth.authenticate(&request);
        assert!(matches!(result, Err(AuthError::InvalidKey)));
    }

    #[tokio::test]
    async fn test_authenticate_disabled_auth() {
        let auth = ApiKeyAuth::disabled();

        let request = Request::builder().uri("/mcp").body(Body::empty()).unwrap();

        // Should pass without any header when auth is disabled
        let result = auth.authenticate(&request);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auth_error_into_response() {
        let error = AuthError::MissingHeader;
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let error = AuthError::InvalidFormat;
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
