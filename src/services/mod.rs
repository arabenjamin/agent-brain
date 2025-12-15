pub mod context;
pub mod discovery;
pub mod docgen;
pub mod export;
pub mod healing;
pub mod http;
pub mod llm;
pub mod openapi;
pub mod secrets;

pub use context::{
    ApiContext, ContextError, ContextStore, EndpointSummary, ParameterSummary, SchemaSummary,
};
pub use discovery::{
    DiscoveryCandidate, DiscoveryConfig, DiscoveryError, DiscoveryMethod, DiscoveryResult,
    DiscoveryService,
};
pub use docgen::{DocGenConfig, DocGenError, DocGenResult, DocGenService, OpenApiSpec};
pub use export::{
    ChangeCategory, ChangeType, DiffError, DiffReport, DiffSummary, ExportError, ExportFormat,
    ExportOptions, ExportResult, ExportStats, MarkdownReportGenerator, OpenApiBuilder,
    OpenApiExporter, SpecChange, SpecDiffer,
};
pub use healing::{
    HealingConfig, HealingError, HealingOrchestrator, HealingResult, RequestContext,
};
pub use http::{
    HttpConfig, HttpError, HttpExecutor, HttpResponse, RequestBuilder, ResponseClass,
    parse_header_string, parse_headers,
};
pub use llm::{ChatMessage, ErrorAnalysis, LlmClient, LlmConfig, LlmError, LlmResponse};
pub use openapi::{EndpointWithParams, IngestResult, OpenApiError, OpenApiParser};
pub use secrets::{
    AwsSecretConfig, AwsSecretProvider, BoxedSecretProvider, CredentialManager,
    CredentialManagerConfig, LocalSecretConfig, LocalSecretProvider, SecretError, SecretProvider,
    VaultConfig, VaultSecretProvider,
};
