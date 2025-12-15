//! Repository-to-OpenAPI generation service.
//!
//! This module provides functionality to analyze source code repositories
//! and generate OpenAPI specifications using LLM-assisted code analysis.
//!
//! Supports:
//! - GitHub repositories (public and private with authentication)
//! - GitLab repositories (public and private with authentication)
//! - Auto-detection of access method (API vs clone)
//! - Framework-agnostic LLM-based code analysis
//! - Merging with existing OpenAPI specs found in the repository

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use glob::Pattern;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::TempDir;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::docgen::{
    Components, ExtractedEndpoint, MediaType, OpenApiSpec, Operation, Parameter, RequestBody,
    Response, SchemaObject, SchemaRef,
};
use super::llm::{ChatMessage, LlmClient};

// ============================================================================
// Types
// ============================================================================

/// Detected repository platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoPlatform {
    GitHub,
    GitLab,
}

impl std::fmt::Display for RepoPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoPlatform::GitHub => write!(f, "GitHub"),
            RepoPlatform::GitLab => write!(f, "GitLab"),
        }
    }
}

/// Parsed repository source information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSource {
    /// Platform type (GitHub or GitLab)
    pub platform: RepoPlatform,
    /// Repository owner/organization
    pub owner: String,
    /// Repository name
    pub repo: String,
    /// Branch/tag/commit reference
    pub ref_name: Option<String>,
    /// Original URL
    pub url: String,
}

impl RepoSource {
    /// Parse a repository URL into its components.
    ///
    /// Supports:
    /// - `https://github.com/owner/repo`
    /// - `https://github.com/owner/repo/tree/branch`
    /// - `https://gitlab.com/owner/repo`
    /// - `https://gitlab.com/owner/repo/-/tree/branch`
    pub fn parse(url: &str) -> Result<Self, RepoError> {
        let url = url.trim().trim_end_matches('/');

        // Parse URL
        let parsed = url::Url::parse(url).map_err(|_| RepoError::InvalidUrl(url.to_string()))?;

        let host = parsed.host_str().ok_or_else(|| RepoError::InvalidUrl(url.to_string()))?;

        // Detect platform
        let platform = if host.contains("github") {
            RepoPlatform::GitHub
        } else if host.contains("gitlab") {
            RepoPlatform::GitLab
        } else {
            return Err(RepoError::UnsupportedPlatform(host.to_string()));
        };

        // Parse path segments
        let path = parsed.path().trim_start_matches('/');
        let segments: Vec<&str> = path.split('/').collect();

        if segments.len() < 2 {
            return Err(RepoError::InvalidUrl(format!(
                "URL must include owner and repository: {}",
                url
            )));
        }

        let owner = segments[0].to_string();
        let repo = segments[1].to_string();

        // Extract branch/ref if present
        let ref_name = match platform {
            RepoPlatform::GitHub => {
                // GitHub: /owner/repo/tree/branch
                if segments.len() >= 4 && segments[2] == "tree" {
                    Some(segments[3..].join("/"))
                } else {
                    None
                }
            }
            RepoPlatform::GitLab => {
                // GitLab: /owner/repo/-/tree/branch
                if segments.len() >= 5 && segments[2] == "-" && segments[3] == "tree" {
                    Some(segments[4..].join("/"))
                } else {
                    None
                }
            }
        };

        Ok(Self {
            platform,
            owner,
            repo,
            ref_name,
            url: url.to_string(),
        })
    }

    /// Get the API base URL for this repository.
    pub fn api_base_url(&self) -> String {
        match self.platform {
            RepoPlatform::GitHub => "https://api.github.com".to_string(),
            RepoPlatform::GitLab => "https://gitlab.com/api/v4".to_string(),
        }
    }

    /// Get the repository API URL.
    pub fn repo_api_url(&self) -> String {
        match self.platform {
            RepoPlatform::GitHub => {
                format!("{}/repos/{}/{}", self.api_base_url(), self.owner, self.repo)
            }
            RepoPlatform::GitLab => {
                let project_path = format!("{}/{}", self.owner, self.repo);
                let encoded = urlencoding::encode(&project_path);
                format!("{}/projects/{}", self.api_base_url(), encoded)
            }
        }
    }

    /// Get the clone URL with optional token for authentication.
    pub fn clone_url(&self, token: Option<&str>) -> String {
        match self.platform {
            RepoPlatform::GitHub => {
                if let Some(t) = token {
                    format!("https://{}@github.com/{}/{}.git", t, self.owner, self.repo)
                } else {
                    format!("https://github.com/{}/{}.git", self.owner, self.repo)
                }
            }
            RepoPlatform::GitLab => {
                if let Some(t) = token {
                    format!(
                        "https://oauth2:{}@gitlab.com/{}/{}.git",
                        t, self.owner, self.repo
                    )
                } else {
                    format!("https://gitlab.com/{}/{}.git", self.owner, self.repo)
                }
            }
        }
    }
}

/// Method used to access the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoAccessMethod {
    /// Used REST API to fetch files
    Api,
    /// Cloned repository to temp directory
    Clone,
}

impl std::fmt::Display for RepoAccessMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoAccessMethod::Api => write!(f, "api"),
            RepoAccessMethod::Clone => write!(f, "clone"),
        }
    }
}

/// Information about an existing OpenAPI spec found in the repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingSpecInfo {
    /// File path within the repository
    pub path: String,
    /// Format (json or yaml)
    pub format: String,
    /// API title from the spec
    pub api_title: Option<String>,
    /// API version from the spec
    pub api_version: Option<String>,
    /// Number of endpoints in the spec
    pub endpoints_count: usize,
}

/// Repository statistics collected during analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStats {
    /// Total files scanned
    pub total_files_scanned: usize,
    /// Files actually analyzed by LLM
    pub files_analyzed: usize,
    /// Access method used
    pub access_method: RepoAccessMethod,
    /// Duration in milliseconds
    pub duration_ms: u64,
}

/// Result of analyzing a repository.
#[derive(Debug, Clone, Serialize)]
pub struct RepoAnalysisResult {
    /// The generated OpenAPI specification
    pub spec: OpenApiSpec,
    /// Repository source information
    pub source: RepoSource,
    /// Files that were analyzed
    pub analyzed_files: Vec<String>,
    /// Total endpoints extracted
    pub endpoints_found: usize,
    /// Total schemas extracted
    pub schemas_found: usize,
    /// Existing spec info if found
    pub existing_spec: Option<ExistingSpecInfo>,
    /// Whether the result was merged with an existing spec
    pub merged_with_existing: bool,
    /// Warnings during analysis
    pub warnings: Vec<String>,
    /// Statistics
    pub stats: RepoStats,
}

/// Merge strategy for combining extracted endpoints with existing spec.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MergeStrategy {
    /// Enhance existing spec with newly discovered endpoints
    #[default]
    Enhance,
    /// Replace existing spec entirely with extracted data
    Replace,
    /// Ignore existing spec, generate from code only
    Ignore,
}

impl MergeStrategy {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "replace" => Self::Replace,
            "ignore" => Self::Ignore,
            _ => Self::Enhance,
        }
    }
}

/// Configuration for repository analysis.
#[derive(Debug, Clone)]
pub struct RepoAnalysisConfig {
    /// Size threshold (bytes) for switching from API to clone
    pub clone_threshold_bytes: u64,
    /// Maximum files to analyze via API
    pub max_api_files: usize,
    /// Request timeout
    pub request_timeout: Duration,
    /// Maximum file size to send to LLM (bytes)
    pub max_file_size: usize,
    /// Whether to search for existing specs
    pub detect_existing_spec: bool,
    /// File patterns to include (glob patterns)
    pub include_patterns: Vec<String>,
    /// File patterns to exclude (glob patterns)
    pub exclude_patterns: Vec<String>,
}

impl Default for RepoAnalysisConfig {
    fn default() -> Self {
        Self {
            clone_threshold_bytes: 10 * 1024 * 1024, // 10 MB
            max_api_files: 100,
            request_timeout: Duration::from_secs(30),
            max_file_size: 50 * 1024, // 50 KB
            detect_existing_spec: true,
            include_patterns: Vec::new(),
            exclude_patterns: default_exclude_patterns(),
        }
    }
}

/// Error types for repository analysis.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("Invalid repository URL: {0}")]
    InvalidUrl(String),

    #[error("Repository not found: {0}")]
    RepoNotFound(String),

    #[error("Access denied to repository. Configure credentials for '{0}' API using configure_api_credential tool.")]
    AccessDenied(String),

    #[error("Rate limited by {0}. Please wait and try again.")]
    RateLimited(String),

    #[error("Failed to clone repository: {0}")]
    CloneFailed(String),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("LLM analysis failed: {0}")]
    LlmError(String),

    #[error("No API endpoints found in repository")]
    NoEndpointsFound,

    #[error("Repository too large for analysis (size: {0} bytes, max: {1} bytes)")]
    RepoTooLarge(u64, u64),

    #[error("Failed to parse existing spec: {0}")]
    SpecParseError(String),

    #[error("Platform not supported: {0}")]
    UnsupportedPlatform(String),
}

// ============================================================================
// Constants
// ============================================================================

/// Default patterns for excluding files from analysis.
fn default_exclude_patterns() -> Vec<String> {
    vec![
        "**/node_modules/**".to_string(),
        "**/vendor/**".to_string(),
        "**/.git/**".to_string(),
        "**/target/**".to_string(),
        "**/build/**".to_string(),
        "**/dist/**".to_string(),
        "**/__pycache__/**".to_string(),
        "**/venv/**".to_string(),
        "**/.venv/**".to_string(),
        "**/test/**".to_string(),
        "**/tests/**".to_string(),
        "**/*_test.*".to_string(),
        "**/*_spec.*".to_string(),
        "**/fixtures/**".to_string(),
        "**/migrations/**".to_string(),
        "**/*.min.js".to_string(),
        "**/*.bundle.js".to_string(),
    ]
}

/// Patterns for API route files (high priority).
const API_FILE_PATTERNS: &[&str] = &[
    // Generic patterns
    "**/routes/**/*",
    "**/api/**/*",
    "**/controllers/**/*",
    "**/handlers/**/*",
    "**/endpoints/**/*",
    "**/resources/**/*",
    // Framework-specific naming
    "**/*_controller.*",
    "**/*_handler.*",
    "**/*_router.*",
    "**/*_api.*",
    "**/*_routes.*",
    "**/*_endpoint.*",
    // Python (Django/FastAPI/Flask)
    "**/views.py",
    "**/urls.py",
    "**/routers.py",
    "**/endpoints.py",
    // Rust
    "**/router.rs",
    "**/routes.rs",
    "**/handlers.rs",
    // Go
    "**/handler.go",
    "**/router.go",
    "**/routes.go",
    // Java/Kotlin
    "**/*Controller.java",
    "**/*Controller.kt",
    "**/*Resource.java",
    // JavaScript/TypeScript
    "**/routes.js",
    "**/routes.ts",
    "**/router.js",
    "**/router.ts",
];

/// Language extensions for code analysis.
const LANGUAGE_EXTENSIONS: &[(&str, &[&str])] = &[
    ("rust", &[".rs"]),
    ("python", &[".py"]),
    ("javascript", &[".js", ".mjs", ".cjs"]),
    ("typescript", &[".ts", ".tsx"]),
    ("java", &[".java"]),
    ("kotlin", &[".kt", ".kts"]),
    ("go", &[".go"]),
    ("ruby", &[".rb"]),
    ("php", &[".php"]),
    ("csharp", &[".cs"]),
];

/// Locations where OpenAPI specs are commonly found.
const OPENAPI_SPEC_PATTERNS: &[&str] = &[
    "openapi.json",
    "openapi.yaml",
    "openapi.yml",
    "swagger.json",
    "swagger.yaml",
    "swagger.yml",
    "api/openapi.json",
    "api/openapi.yaml",
    "api/openapi.yml",
    "docs/openapi.json",
    "docs/openapi.yaml",
    "docs/openapi.yml",
    "spec/openapi.json",
    "spec/openapi.yaml",
    "spec/openapi.yml",
    ".openapi/spec.json",
    ".openapi/spec.yaml",
];

// ============================================================================
// Repository Analyzer Service
// ============================================================================

/// Service for analyzing repositories and generating OpenAPI specs.
pub struct RepoAnalyzerService {
    client: Client,
    llm: LlmClient,
    config: RepoAnalysisConfig,
}

impl RepoAnalyzerService {
    /// Create a new repository analyzer service.
    pub fn new(llm: LlmClient) -> Result<Self, RepoError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("agent-api/0.1.0 (OpenAPI Generator)")
            .build()
            .map_err(|e| RepoError::HttpError(e.to_string()))?;

        Ok(Self {
            client,
            llm,
            config: RepoAnalysisConfig::default(),
        })
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: RepoAnalysisConfig) -> Self {
        self.config = config;
        self
    }

    /// Analyze a repository and generate an OpenAPI specification.
    pub async fn analyze(
        &self,
        repo_url: &str,
        api_title: &str,
        api_version: &str,
        base_url: Option<&str>,
        merge_strategy: MergeStrategy,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<RepoAnalysisResult, RepoError> {
        let start = Instant::now();

        info!(url = %repo_url, title = %api_title, "Starting repository analysis");

        // Parse the repository URL
        let source = RepoSource::parse(repo_url)?;
        debug!(platform = %source.platform, owner = %source.owner, repo = %source.repo, "Parsed repository URL");

        // Determine access method based on repo size
        let (access_method, files) = self
            .fetch_repository_files(&source, token, subdirectory)
            .await?;

        info!(
            method = %access_method,
            files = files.len(),
            "Fetched repository files"
        );

        let warnings = Vec::new();
        let total_files_scanned = files.len();

        // Check for existing OpenAPI spec
        let existing_spec = if self.config.detect_existing_spec {
            self.find_existing_spec(&files).await
        } else {
            None
        };

        if let Some(ref spec_info) = existing_spec {
            info!(
                path = %spec_info.path,
                endpoints = spec_info.endpoints_count,
                "Found existing OpenAPI specification"
            );
        }

        // Filter files to analyze
        let api_files = self.filter_api_files(&files);
        debug!(api_files = api_files.len(), "Filtered to API-related files");

        // Analyze code with LLM
        let extracted = self.analyze_code_files(&api_files).await?;
        let files_analyzed = api_files.len();

        if extracted.is_empty() && existing_spec.is_none() {
            return Err(RepoError::NoEndpointsFound);
        }

        // Build or merge the OpenAPI spec
        let (spec, merged) = self
            .build_spec(
                &extracted,
                &existing_spec,
                merge_strategy,
                api_title,
                api_version,
                base_url,
            )
            .await;

        let endpoints_found = spec.paths.values().map(|p| count_operations(p)).sum();
        let schemas_found = spec
            .components
            .as_ref()
            .map(|c| c.schemas.len())
            .unwrap_or(0);

        let result = RepoAnalysisResult {
            spec,
            source,
            analyzed_files: api_files.iter().map(|(path, _)| path.clone()).collect(),
            endpoints_found,
            schemas_found,
            existing_spec,
            merged_with_existing: merged,
            warnings,
            stats: RepoStats {
                total_files_scanned,
                files_analyzed,
                access_method,
                duration_ms: start.elapsed().as_millis() as u64,
            },
        };

        info!(
            endpoints = result.endpoints_found,
            schemas = result.schemas_found,
            duration_ms = result.stats.duration_ms,
            "Repository analysis complete"
        );

        Ok(result)
    }

    /// Fetch repository files using API or clone method.
    async fn fetch_repository_files(
        &self,
        source: &RepoSource,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<(RepoAccessMethod, Vec<(String, String)>), RepoError> {
        // First, get repository metadata to determine size
        let repo_size = self.get_repo_size(source, token).await?;

        if repo_size > self.config.clone_threshold_bytes {
            debug!(
                size = repo_size,
                threshold = self.config.clone_threshold_bytes,
                "Repository exceeds API threshold, using clone method"
            );
            let files = self.clone_and_read_files(source, token, subdirectory).await?;
            Ok((RepoAccessMethod::Clone, files))
        } else {
            debug!(
                size = repo_size,
                "Repository within API threshold, using API method"
            );
            match self.fetch_files_via_api(source, token, subdirectory).await {
                Ok(files) => Ok((RepoAccessMethod::Api, files)),
                Err(RepoError::RateLimited(_)) => {
                    warn!("Rate limited, falling back to clone method");
                    let files = self.clone_and_read_files(source, token, subdirectory).await?;
                    Ok((RepoAccessMethod::Clone, files))
                }
                Err(e) => Err(e),
            }
        }
    }

    /// Get repository size in bytes.
    async fn get_repo_size(&self, source: &RepoSource, token: Option<&str>) -> Result<u64, RepoError> {
        let url = source.repo_api_url();
        let mut request = self.client.get(&url);

        // Add authentication header
        if let Some(t) = token {
            request = match source.platform {
                RepoPlatform::GitHub => request.header("Authorization", format!("Bearer {}", t)),
                RepoPlatform::GitLab => request.header("PRIVATE-TOKEN", t),
            };
        }

        request = request.header("Accept", "application/json");

        let response = request.send().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        match response.status().as_u16() {
            200 => {
                let body: Value = response.json().await.map_err(|e| RepoError::HttpError(e.to_string()))?;
                // GitHub returns "size" in KB, GitLab returns in bytes
                let size = match source.platform {
                    RepoPlatform::GitHub => body["size"].as_u64().unwrap_or(0) * 1024,
                    RepoPlatform::GitLab => {
                        body["statistics"]["repository_size"].as_u64().unwrap_or(0)
                    }
                };
                Ok(size)
            }
            401 | 403 => Err(RepoError::AccessDenied(source.platform.to_string())),
            404 => Err(RepoError::RepoNotFound(source.url.clone())),
            429 => Err(RepoError::RateLimited(source.platform.to_string())),
            status => Err(RepoError::HttpError(format!("HTTP {}", status))),
        }
    }

    /// Fetch files using the platform's REST API.
    async fn fetch_files_via_api(
        &self,
        source: &RepoSource,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<Vec<(String, String)>, RepoError> {
        match source.platform {
            RepoPlatform::GitHub => self.fetch_github_files(source, token, subdirectory).await,
            RepoPlatform::GitLab => self.fetch_gitlab_files(source, token, subdirectory).await,
        }
    }

    /// Fetch files from GitHub using the Contents API.
    async fn fetch_github_files(
        &self,
        source: &RepoSource,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<Vec<(String, String)>, RepoError> {
        let mut files = Vec::new();
        let ref_param = source.ref_name.as_deref().unwrap_or("HEAD");

        // Get tree recursively
        let tree_url = format!(
            "{}/repos/{}/{}/git/trees/{}?recursive=1",
            source.api_base_url(),
            source.owner,
            source.repo,
            ref_param
        );

        let mut request = self.client.get(&tree_url);
        if let Some(t) = token {
            request = request.header("Authorization", format!("Bearer {}", t));
        }
        request = request.header("Accept", "application/vnd.github+json");

        let response = request.send().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(RepoError::AccessDenied(source.platform.to_string())),
            404 => return Err(RepoError::RepoNotFound(source.url.clone())),
            429 => return Err(RepoError::RateLimited(source.platform.to_string())),
            status => return Err(RepoError::HttpError(format!("HTTP {}", status))),
        }

        let body: Value = response.json().await.map_err(|e| RepoError::HttpError(e.to_string()))?;
        let tree = body["tree"].as_array().ok_or_else(|| {
            RepoError::HttpError("Invalid tree response".to_string())
        })?;

        // Filter files
        let prefix = subdirectory.map(|s| format!("{}/", s.trim_matches('/'))).unwrap_or_default();

        let relevant_files: Vec<&Value> = tree
            .iter()
            .filter(|item| {
                let path = item["path"].as_str().unwrap_or("");
                let item_type = item["type"].as_str().unwrap_or("");

                item_type == "blob"
                    && path.starts_with(&prefix)
                    && self.is_relevant_file(path)
                    && !self.is_excluded(path)
            })
            .take(self.config.max_api_files)
            .collect();

        // Fetch file contents
        for item in relevant_files {
            let path = item["path"].as_str().unwrap_or("");
            let size = item["size"].as_u64().unwrap_or(0);

            if size > self.config.max_file_size as u64 {
                debug!(path = %path, size = size, "Skipping large file");
                continue;
            }

            if let Ok(content) = self.fetch_github_file_content(source, path, token).await {
                files.push((path.to_string(), content));
            }
        }

        Ok(files)
    }

    /// Fetch a single file's content from GitHub.
    async fn fetch_github_file_content(
        &self,
        source: &RepoSource,
        path: &str,
        token: Option<&str>,
    ) -> Result<String, RepoError> {
        let ref_param = source.ref_name.as_deref().unwrap_or("HEAD");
        let url = format!(
            "{}/repos/{}/{}/contents/{}?ref={}",
            source.api_base_url(),
            source.owner,
            source.repo,
            path,
            ref_param
        );

        let mut request = self.client.get(&url);
        if let Some(t) = token {
            request = request.header("Authorization", format!("Bearer {}", t));
        }
        request = request.header("Accept", "application/vnd.github.raw");

        let response = request.send().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        if response.status().is_success() {
            response.text().await.map_err(|e| RepoError::HttpError(e.to_string()))
        } else {
            Err(RepoError::HttpError(format!(
                "Failed to fetch {}: HTTP {}",
                path,
                response.status()
            )))
        }
    }

    /// Fetch files from GitLab using the Repository Files API.
    async fn fetch_gitlab_files(
        &self,
        source: &RepoSource,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<Vec<(String, String)>, RepoError> {
        let mut files = Vec::new();
        let ref_param = source.ref_name.as_deref().unwrap_or("HEAD");
        let project_path = format!("{}/{}", source.owner, source.repo);
        let encoded_project = urlencoding::encode(&project_path);

        // Get repository tree
        let tree_url = format!(
            "{}/projects/{}/repository/tree?recursive=true&ref={}&per_page=100",
            source.api_base_url(),
            encoded_project,
            ref_param
        );

        let mut request = self.client.get(&tree_url);
        if let Some(t) = token {
            request = request.header("PRIVATE-TOKEN", t);
        }

        let response = request.send().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(RepoError::AccessDenied(source.platform.to_string())),
            404 => return Err(RepoError::RepoNotFound(source.url.clone())),
            429 => return Err(RepoError::RateLimited(source.platform.to_string())),
            status => return Err(RepoError::HttpError(format!("HTTP {}", status))),
        }

        let tree: Vec<Value> = response.json().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        let prefix = subdirectory.map(|s| format!("{}/", s.trim_matches('/'))).unwrap_or_default();

        let relevant_files: Vec<&Value> = tree
            .iter()
            .filter(|item| {
                let path = item["path"].as_str().unwrap_or("");
                let item_type = item["type"].as_str().unwrap_or("");

                item_type == "blob"
                    && path.starts_with(&prefix)
                    && self.is_relevant_file(path)
                    && !self.is_excluded(path)
            })
            .take(self.config.max_api_files)
            .collect();

        // Fetch file contents
        for item in relevant_files {
            let path = item["path"].as_str().unwrap_or("");
            if let Ok(content) = self.fetch_gitlab_file_content(source, path, token).await {
                files.push((path.to_string(), content));
            }
        }

        Ok(files)
    }

    /// Fetch a single file's content from GitLab.
    async fn fetch_gitlab_file_content(
        &self,
        source: &RepoSource,
        path: &str,
        token: Option<&str>,
    ) -> Result<String, RepoError> {
        let ref_param = source.ref_name.as_deref().unwrap_or("HEAD");
        let project_path = format!("{}/{}", source.owner, source.repo);
        let encoded_project = urlencoding::encode(&project_path);
        let encoded_path = urlencoding::encode(path);

        let url = format!(
            "{}/projects/{}/repository/files/{}/raw?ref={}",
            source.api_base_url(),
            encoded_project,
            encoded_path,
            ref_param
        );

        let mut request = self.client.get(&url);
        if let Some(t) = token {
            request = request.header("PRIVATE-TOKEN", t);
        }

        let response = request.send().await.map_err(|e| RepoError::HttpError(e.to_string()))?;

        if response.status().is_success() {
            response.text().await.map_err(|e| RepoError::HttpError(e.to_string()))
        } else {
            Err(RepoError::HttpError(format!(
                "Failed to fetch {}: HTTP {}",
                path,
                response.status()
            )))
        }
    }

    /// Clone repository and read files.
    async fn clone_and_read_files(
        &self,
        source: &RepoSource,
        token: Option<&str>,
        subdirectory: Option<&str>,
    ) -> Result<Vec<(String, String)>, RepoError> {
        let temp_dir = TempDir::new().map_err(RepoError::IoError)?;
        let clone_path = temp_dir.path();

        info!(path = %clone_path.display(), "Cloning repository to temp directory");

        let clone_url = source.clone_url(token);
        let ref_name = source.ref_name.as_deref();

        // Build git clone command
        let mut cmd = Command::new("git");
        cmd.arg("clone")
            .arg("--depth=1")
            .arg("--single-branch");

        if let Some(branch) = ref_name {
            cmd.arg("--branch").arg(branch);
        }

        cmd.arg(&clone_url).arg(clone_path);

        let output = cmd
            .output()
            .await
            .map_err(|e| RepoError::CloneFailed(e.to_string()))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Authentication failed") || stderr.contains("could not read") {
                return Err(RepoError::AccessDenied(source.platform.to_string()));
            }
            return Err(RepoError::CloneFailed(stderr.to_string()));
        }

        // Read files from cloned repository
        let base_path = if let Some(subdir) = subdirectory {
            clone_path.join(subdir)
        } else {
            clone_path.to_path_buf()
        };

        self.read_local_files(&base_path, clone_path).await
    }

    /// Read files from a local directory.
    async fn read_local_files(
        &self,
        base_path: &Path,
        repo_root: &Path,
    ) -> Result<Vec<(String, String)>, RepoError> {
        let mut files = Vec::new();
        let mut stack = vec![base_path.to_path_buf()];

        while let Some(current) = stack.pop() {
            let mut entries = fs::read_dir(&current).await?;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let relative_path = path
                    .strip_prefix(repo_root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                if path.is_dir() {
                    if !self.is_excluded(&relative_path) {
                        stack.push(path);
                    }
                } else if path.is_file()
                    && self.is_relevant_file(&relative_path)
                    && !self.is_excluded(&relative_path)
                {
                    let metadata = fs::metadata(&path).await?;
                    if metadata.len() <= self.config.max_file_size as u64 {
                        if let Ok(content) = fs::read_to_string(&path).await {
                            files.push((relative_path, content));
                        }
                    }
                }
            }

            if files.len() >= self.config.max_api_files {
                break;
            }
        }

        Ok(files)
    }

    /// Check if a file is relevant for API analysis.
    fn is_relevant_file(&self, path: &str) -> bool {
        // Check language extensions
        let has_relevant_extension = LANGUAGE_EXTENSIONS
            .iter()
            .any(|(_, exts)| exts.iter().any(|ext| path.ends_with(ext)));

        if !has_relevant_extension {
            return false;
        }

        // Check include patterns if specified
        if !self.config.include_patterns.is_empty() {
            return self.config.include_patterns.iter().any(|pattern| {
                Pattern::new(pattern)
                    .map(|p| p.matches(path))
                    .unwrap_or(false)
            });
        }

        // Check API file patterns
        API_FILE_PATTERNS.iter().any(|pattern| {
            Pattern::new(pattern)
                .map(|p| p.matches(path))
                .unwrap_or(false)
        })
    }

    /// Check if a file should be excluded.
    fn is_excluded(&self, path: &str) -> bool {
        self.config.exclude_patterns.iter().any(|pattern| {
            Pattern::new(pattern)
                .map(|p| p.matches(path))
                .unwrap_or(false)
        })
    }

    /// Filter files to only those likely containing API definitions.
    fn filter_api_files(&self, files: &[(String, String)]) -> Vec<(String, String)> {
        files
            .iter()
            .filter(|(path, _)| {
                // Prioritize files matching API patterns
                API_FILE_PATTERNS.iter().any(|pattern| {
                    Pattern::new(pattern)
                        .map(|p| p.matches(path))
                        .unwrap_or(false)
                })
            })
            .cloned()
            .collect()
    }

    /// Find existing OpenAPI specification in the repository.
    async fn find_existing_spec(&self, files: &[(String, String)]) -> Option<ExistingSpecInfo> {
        for (path, content) in files {
            let lower_path = path.to_lowercase();

            // Check if this looks like an OpenAPI spec file
            let is_spec_file = OPENAPI_SPEC_PATTERNS.iter().any(|pattern| {
                lower_path.ends_with(pattern) || lower_path == *pattern
            });

            if !is_spec_file {
                continue;
            }

            // Try to parse as OpenAPI spec
            let format = if lower_path.ends_with(".json") {
                "json"
            } else {
                "yaml"
            };

            // Try to parse as JSON or YAML
            let parsed: Option<Value> = if format == "json" {
                serde_json::from_str(content).ok()
            } else {
                serde_yaml::from_str(content).ok()
            };

            if let Some(spec) = parsed {
                // Verify it's an OpenAPI spec
                if spec.get("openapi").is_some() || spec.get("swagger").is_some() {
                    let api_title = spec["info"]["title"].as_str().map(String::from);
                    let api_version = spec["info"]["version"].as_str().map(String::from);
                    let endpoints_count = spec["paths"]
                        .as_object()
                        .map(|p| p.len())
                        .unwrap_or(0);

                    return Some(ExistingSpecInfo {
                        path: path.clone(),
                        format: format.to_string(),
                        api_title,
                        api_version,
                        endpoints_count,
                    });
                }
            }
        }

        None
    }

    /// Analyze code files with LLM to extract API endpoints.
    async fn analyze_code_files(
        &self,
        files: &[(String, String)],
    ) -> Result<Vec<ExtractedEndpoint>, RepoError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_endpoints = Vec::new();

        // Batch files by size (~10KB per batch)
        let mut batches: Vec<Vec<(&String, &String)>> = Vec::new();
        let mut current_batch = Vec::new();
        let mut current_size = 0;
        const BATCH_SIZE: usize = 10 * 1024;

        for (path, content) in files {
            if current_size + content.len() > BATCH_SIZE && !current_batch.is_empty() {
                batches.push(current_batch);
                current_batch = Vec::new();
                current_size = 0;
            }
            current_batch.push((path, content));
            current_size += content.len();
        }
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        info!(batches = batches.len(), files = files.len(), "Analyzing code in batches");

        // Process each batch
        for (i, batch) in batches.iter().enumerate() {
            debug!(batch = i + 1, total = batches.len(), "Processing batch");

            match self.analyze_batch(batch).await {
                Ok(endpoints) => {
                    all_endpoints.extend(endpoints);
                }
                Err(e) => {
                    warn!(batch = i + 1, error = %e, "Batch analysis failed");
                }
            }
        }

        // Deduplicate endpoints by path+method
        let mut seen = std::collections::HashSet::new();
        all_endpoints.retain(|ep| {
            let key = format!("{}:{}", ep.method.to_uppercase(), ep.path);
            seen.insert(key)
        });

        Ok(all_endpoints)
    }

    /// Analyze a batch of files with LLM.
    async fn analyze_batch(
        &self,
        files: &[(&String, &String)],
    ) -> Result<Vec<ExtractedEndpoint>, RepoError> {
        let prompt = self.build_analysis_prompt(files);

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: CODE_ANALYSIS_SYSTEM_PROMPT.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: prompt,
            },
        ];

        let response = self
            .llm
            .chat(&messages)
            .await
            .map_err(|e| RepoError::LlmError(e.to_string()))?;

        self.parse_llm_response(&response.text)
    }

    /// Build the LLM prompt for code analysis.
    fn build_analysis_prompt(&self, files: &[(&String, &String)]) -> String {
        let mut prompt = String::from(
            "Analyze the following source code files and extract all API endpoints.\n\n"
        );

        for (path, content) in files {
            let language = detect_language(path);
            prompt.push_str(&format!(
                "--- File: {} (Language: {}) ---\n```{}\n{}\n```\n\n",
                path, language, language.to_lowercase(), content
            ));
        }

        prompt.push_str(
            "Extract ALL API endpoints you can find. Look for:\n\
             - Route definitions and decorators\n\
             - HTTP handler functions\n\
             - Path parameters, query parameters, request bodies\n\
             - Response types and schemas\n\n\
             Return ONLY the JSON object, no explanations."
        );

        prompt
    }

    /// Parse the LLM response to extract endpoints.
    fn parse_llm_response(&self, response: &str) -> Result<Vec<ExtractedEndpoint>, RepoError> {
        // Find JSON in response
        let json_str = if let Some(start) = response.find('{') {
            if let Some(end) = response.rfind('}') {
                &response[start..=end]
            } else {
                response
            }
        } else {
            response
        };

        // Clean markdown formatting
        let cleaned = json_str
            .replace("```json", "")
            .replace("```", "")
            .trim()
            .to_string();

        // Parse the response
        let parsed: Value = serde_json::from_str(&cleaned)
            .map_err(|e| RepoError::LlmError(format!("Failed to parse LLM response: {}", e)))?;

        // Extract endpoints array
        let endpoints_value = if parsed.is_array() {
            parsed
        } else if let Some(eps) = parsed.get("endpoints") {
            eps.clone()
        } else {
            return Ok(Vec::new());
        };

        let endpoints: Vec<ExtractedEndpoint> = serde_json::from_value(endpoints_value)
            .unwrap_or_default();

        Ok(endpoints)
    }

    /// Build the final OpenAPI spec, optionally merging with existing.
    async fn build_spec(
        &self,
        extracted: &[ExtractedEndpoint],
        _existing: &Option<ExistingSpecInfo>,
        _merge_strategy: MergeStrategy,
        api_title: &str,
        api_version: &str,
        base_url: Option<&str>,
    ) -> (OpenApiSpec, bool) {
        let mut spec = OpenApiSpec::new(api_title, api_version);
        let merged = false;

        if let Some(url) = base_url {
            spec.add_server(url, Some("API Server"));
        }

        // TODO: If existing spec found and merge_strategy is Enhance, parse and merge
        // For now, just build from extracted endpoints

        // Add extracted endpoints
        for endpoint in extracted {
            let operation = build_operation_from_extracted(endpoint);
            spec.add_endpoint(&endpoint.path, &endpoint.method, operation);
        }

        // Collect schemas from endpoints
        let schemas = collect_schemas_from_endpoints(extracted);
        if !schemas.is_empty() {
            spec.components = Some(Components { schemas });
        }

        (spec, merged)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// System prompt for code analysis.
const CODE_ANALYSIS_SYSTEM_PROMPT: &str = r#"You are an API endpoint extraction expert. Your task is to analyze source code files and extract API endpoint definitions.

You must identify:
1. HTTP routes/endpoints (method + path)
2. Path parameters (e.g., /users/{id})
3. Query parameters
4. Request body schemas
5. Response schemas
6. Authentication requirements

Analyze code from ANY programming language or framework. Look for:
- Route decorators (@app.route, @GetMapping, @router.get, etc.)
- Handler functions with HTTP method patterns
- Middleware authentication markers
- Request/response type definitions
- API documentation comments

Return ONLY a valid JSON object with this structure:
{
  "endpoints": [
    {
      "method": "GET",
      "path": "/path/{param}",
      "summary": "Brief description",
      "description": "Longer description if available",
      "parameters": [
        {"name": "param", "location": "path", "param_type": "string", "required": true, "description": "..."}
      ],
      "request_body": {"content_type": "application/json", "description": "...", "schema": {...}} | null,
      "response": {"status_code": "200", "content_type": "application/json", "description": "...", "schema": {...}} | null,
      "tags": ["Category"]
    }
  ]
}

Important:
- Use {param} format for path parameters
- Always include the HTTP method (GET, POST, PUT, PATCH, DELETE)
- Extract all endpoints you can find
- Return empty endpoints array if no API endpoints found
"#;

/// Detect language from file path.
fn detect_language(path: &str) -> &'static str {
    for (lang, exts) in LANGUAGE_EXTENSIONS {
        if exts.iter().any(|ext| path.ends_with(ext)) {
            return lang;
        }
    }
    "unknown"
}

/// Count operations in a path item.
fn count_operations(path_item: &super::docgen::PathItem) -> usize {
    let mut count = 0;
    if path_item.get.is_some() { count += 1; }
    if path_item.post.is_some() { count += 1; }
    if path_item.put.is_some() { count += 1; }
    if path_item.patch.is_some() { count += 1; }
    if path_item.delete.is_some() { count += 1; }
    if path_item.head.is_some() { count += 1; }
    if path_item.options.is_some() { count += 1; }
    count
}

/// Build an Operation from an ExtractedEndpoint.
fn build_operation_from_extracted(endpoint: &ExtractedEndpoint) -> Operation {
    let mut operation = Operation::new(endpoint.summary.as_deref().unwrap_or(""));

    operation.description = endpoint.description.clone();
    operation.tags = endpoint.tags.clone();

    // Generate operation ID from method + path
    let path_parts: Vec<&str> = endpoint
        .path
        .split('/')
        .filter(|s| !s.is_empty() && !s.starts_with('{'))
        .collect();
    let method_lower = endpoint.method.to_lowercase();
    operation.operation_id = Some(format!(
        "{}{}",
        method_lower,
        path_parts
            .iter()
            .map(|s| capitalize_first(s))
            .collect::<String>()
    ));

    // Add parameters
    for param in &endpoint.parameters {
        operation.parameters.push(Parameter {
            name: param.name.clone(),
            location: param.location.clone(),
            description: param.description.clone(),
            required: param.required,
            schema: param.param_type.as_ref().map(|t| {
                SchemaRef::Inline(SchemaObject {
                    schema_type: Some(t.clone()),
                    format: None,
                    description: None,
                    properties: None,
                    items: None,
                    required: Vec::new(),
                })
            }),
        });
    }

    // Add request body
    if let Some(body) = &endpoint.request_body {
        let mut content = HashMap::new();
        content.insert(
            body.content_type.clone(),
            MediaType {
                schema: body.schema.as_ref().map(|s| {
                    SchemaRef::Inline(value_to_schema_object(s))
                }),
            },
        );

        operation.request_body = Some(RequestBody {
            description: body.description.clone(),
            content,
            required: true,
        });
    }

    // Add response
    if let Some(resp) = &endpoint.response {
        let mut response_content = None;
        if let Some(ct) = &resp.content_type {
            let mut content = HashMap::new();
            content.insert(
                ct.clone(),
                MediaType {
                    schema: resp.schema.as_ref().map(|s| {
                        SchemaRef::Inline(value_to_schema_object(s))
                    }),
                },
            );
            response_content = Some(content);
        }

        operation.responses.insert(
            resp.status_code.clone(),
            Response {
                description: resp.description.clone().unwrap_or_else(|| "Success".to_string()),
                content: response_content,
            },
        );
    }

    operation
}

/// Convert a JSON Value to a SchemaObject.
fn value_to_schema_object(value: &Value) -> SchemaObject {
    match value {
        Value::Object(obj) => {
            let schema_type = obj.get("type").and_then(|v| v.as_str()).map(String::from);
            let format = obj.get("format").and_then(|v| v.as_str()).map(String::from);
            let description = obj.get("description").and_then(|v| v.as_str()).map(String::from);

            let properties = obj.get("properties").and_then(|v| v.as_object()).map(|props| {
                props
                    .iter()
                    .map(|(k, v)| (k.clone(), value_to_schema_object(v)))
                    .collect()
            });

            let items = obj.get("items").map(|v| Box::new(value_to_schema_object(v)));

            let required = obj
                .get("required")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            SchemaObject {
                schema_type,
                format,
                description,
                properties,
                items,
                required,
            }
        }
        _ => SchemaObject {
            schema_type: Some(json_type_to_schema_type(value)),
            format: None,
            description: None,
            properties: None,
            items: None,
            required: Vec::new(),
        },
    }
}

/// Map JSON value type to OpenAPI schema type.
fn json_type_to_schema_type(value: &Value) -> String {
    match value {
        Value::String(_) => "string".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
        Value::Null => "null".to_string(),
    }
}

/// Collect unique schemas from extracted endpoints.
fn collect_schemas_from_endpoints(endpoints: &[ExtractedEndpoint]) -> HashMap<String, SchemaObject> {
    let mut schemas = HashMap::new();

    for endpoint in endpoints {
        // Extract schemas from request bodies
        if let Some(body) = &endpoint.request_body {
            if let Some(schema) = &body.schema {
                if let Some(name) = extract_schema_name(schema) {
                    schemas.insert(name, value_to_schema_object(schema));
                }
            }
        }

        // Extract schemas from responses
        if let Some(resp) = &endpoint.response {
            if let Some(schema) = &resp.schema {
                if let Some(name) = extract_schema_name(schema) {
                    schemas.insert(name, value_to_schema_object(schema));
                }
            }
        }
    }

    schemas
}

/// Extract schema name from a schema value if it has a title or can be inferred.
fn extract_schema_name(value: &Value) -> Option<String> {
    value
        .as_object()
        .and_then(|obj| obj.get("title"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(chars).collect(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_url() {
        let source = RepoSource::parse("https://github.com/owner/repo").unwrap();
        assert_eq!(source.platform, RepoPlatform::GitHub);
        assert_eq!(source.owner, "owner");
        assert_eq!(source.repo, "repo");
        assert_eq!(source.ref_name, None);
    }

    #[test]
    fn test_parse_github_url_with_branch() {
        let source = RepoSource::parse("https://github.com/owner/repo/tree/main").unwrap();
        assert_eq!(source.platform, RepoPlatform::GitHub);
        assert_eq!(source.owner, "owner");
        assert_eq!(source.repo, "repo");
        assert_eq!(source.ref_name, Some("main".to_string()));
    }

    #[test]
    fn test_parse_github_url_with_nested_branch() {
        let source = RepoSource::parse("https://github.com/owner/repo/tree/feature/my-branch").unwrap();
        assert_eq!(source.ref_name, Some("feature/my-branch".to_string()));
    }

    #[test]
    fn test_parse_gitlab_url() {
        let source = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
        assert_eq!(source.platform, RepoPlatform::GitLab);
        assert_eq!(source.owner, "owner");
        assert_eq!(source.repo, "repo");
    }

    #[test]
    fn test_parse_gitlab_url_with_branch() {
        let source = RepoSource::parse("https://gitlab.com/owner/repo/-/tree/main").unwrap();
        assert_eq!(source.platform, RepoPlatform::GitLab);
        assert_eq!(source.ref_name, Some("main".to_string()));
    }

    #[test]
    fn test_parse_invalid_url() {
        assert!(RepoSource::parse("not-a-url").is_err());
        assert!(RepoSource::parse("https://bitbucket.org/owner/repo").is_err());
    }

    #[test]
    fn test_clone_url_generation() {
        let source = RepoSource::parse("https://github.com/owner/repo").unwrap();
        assert_eq!(source.clone_url(None), "https://github.com/owner/repo.git");
        assert_eq!(
            source.clone_url(Some("token123")),
            "https://token123@github.com/owner/repo.git"
        );

        let gitlab_source = RepoSource::parse("https://gitlab.com/owner/repo").unwrap();
        assert_eq!(gitlab_source.clone_url(None), "https://gitlab.com/owner/repo.git");
        assert_eq!(
            gitlab_source.clone_url(Some("token123")),
            "https://oauth2:token123@gitlab.com/owner/repo.git"
        );
    }

    #[test]
    fn test_api_file_patterns() {
        // Test that API file patterns contain expected patterns
        assert!(API_FILE_PATTERNS.iter().any(|p| p.contains("routes")));
        assert!(API_FILE_PATTERNS.iter().any(|p| p.contains("controller")));
        assert!(API_FILE_PATTERNS.iter().any(|p| p.contains("handler")));
        assert!(API_FILE_PATTERNS.iter().any(|p| p.contains("api")));
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("src/main.rs"), "rust");
        assert_eq!(detect_language("app.py"), "python");
        assert_eq!(detect_language("index.js"), "javascript");
        assert_eq!(detect_language("server.ts"), "typescript");
        assert_eq!(detect_language("Main.java"), "java");
        assert_eq!(detect_language("handler.go"), "go");
        assert_eq!(detect_language("unknown.xyz"), "unknown");
    }

    #[test]
    fn test_merge_strategy_from_str() {
        assert_eq!(MergeStrategy::from_str("enhance"), MergeStrategy::Enhance);
        assert_eq!(MergeStrategy::from_str("replace"), MergeStrategy::Replace);
        assert_eq!(MergeStrategy::from_str("ignore"), MergeStrategy::Ignore);
        assert_eq!(MergeStrategy::from_str("unknown"), MergeStrategy::Enhance);
    }

    #[test]
    fn test_capitalize_first() {
        assert_eq!(capitalize_first("hello"), "Hello");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    #[test]
    fn test_value_to_schema_object() {
        let value = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"}
            },
            "required": ["name"]
        });

        let schema = value_to_schema_object(&value);
        assert_eq!(schema.schema_type, Some("object".to_string()));
        assert!(schema.properties.is_some());
        assert_eq!(schema.required, vec!["name".to_string()]);
    }

    #[test]
    fn test_default_exclude_patterns() {
        let patterns = default_exclude_patterns();
        assert!(patterns.iter().any(|p| p.contains("node_modules")));
        assert!(patterns.iter().any(|p| p.contains("target")));
        assert!(patterns.iter().any(|p| p.contains(".git")));
    }
}
