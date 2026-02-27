use serde::{Deserialize, Serialize};

/// Specification for a registered LLM model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Unique identifier (UUID).
    pub id: String,
    /// Model name / ID as used by the provider (e.g. "claude-haiku-4-5-20251001").
    pub name: String,
    /// Provider name: "ollama", "anthropic", or "gemini".
    pub provider: String,
    /// Input token cost in USD per 1,000 tokens.
    pub cost_per_1k_tokens_input: f64,
    /// Output token cost in USD per 1,000 tokens.
    pub cost_per_1k_tokens_output: f64,
    /// Maximum context window in tokens.
    pub context_window: u32,
    /// Capability tags, e.g. ["reasoning", "code", "fast", "vision"].
    pub capabilities: Vec<String>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}
