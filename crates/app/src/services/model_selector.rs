//! Model selector — picks the cheapest capable registered LLM model.

use crate::models::ModelSpec;
use crate::repository::Neo4jClient;

/// Selects the best registered model for given task requirements.
pub struct ModelSelector {
    neo4j: Neo4jClient,
}

impl ModelSelector {
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
    }

    /// Select the best (cheapest capable) model for the given requirements.
    ///
    /// Selection strategy:
    /// 1. Filter by `provider_hint` if provided.
    /// 2. Require all `required_capabilities` to be present.
    /// 3. Filter by `max_cost_per_1k` (combined input + output) if provided.
    /// 4. Sort by combined cost ascending; break ties with larger context window.
    ///
    /// Returns `None` if no registered model satisfies the constraints.
    pub async fn select(
        &self,
        required_capabilities: &[String],
        provider_hint: Option<&str>,
        max_cost_per_1k: Option<f64>,
    ) -> Option<ModelSpec> {
        let specs = self.neo4j.list_model_specs().await.ok()?;

        let mut candidates: Vec<ModelSpec> = specs
            .into_iter()
            .filter(|spec| {
                // Provider filter.
                if let Some(hint) = provider_hint {
                    if spec.provider != hint {
                        return false;
                    }
                }

                // All required capabilities must be present.
                for cap in required_capabilities {
                    if !spec.capabilities.contains(cap) {
                        return false;
                    }
                }

                // Cost ceiling filter.
                if let Some(max_cost) = max_cost_per_1k {
                    let total = spec.cost_per_1k_tokens_input + spec.cost_per_1k_tokens_output;
                    if total > max_cost {
                        return false;
                    }
                }

                true
            })
            .collect();

        // Cheapest first; tie-break by largest context window.
        candidates.sort_by(|a, b| {
            let cost_a = a.cost_per_1k_tokens_input + a.cost_per_1k_tokens_output;
            let cost_b = b.cost_per_1k_tokens_input + b.cost_per_1k_tokens_output;
            cost_a
                .partial_cmp(&cost_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.context_window.cmp(&a.context_window))
        });

        candidates.into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_spec_cost_sort() {
        let mut specs = vec![
            ModelSpec {
                id: "1".into(),
                name: "gemini-flash".into(),
                provider: "gemini".into(),
                cost_per_1k_tokens_input: 0.001,
                cost_per_1k_tokens_output: 0.002,
                context_window: 100_000,
                capabilities: vec!["fast".into(), "reasoning".into()],
                created_at: "2026-01-01".into(),
            },
            ModelSpec {
                id: "2".into(),
                name: "claude-haiku".into(),
                provider: "anthropic".into(),
                cost_per_1k_tokens_input: 0.0008,
                cost_per_1k_tokens_output: 0.0025,
                context_window: 200_000,
                capabilities: vec!["reasoning".into(), "code".into()],
                created_at: "2026-01-01".into(),
            },
            ModelSpec {
                id: "3".into(),
                name: "local-llama".into(),
                provider: "ollama".into(),
                cost_per_1k_tokens_input: 0.0,
                cost_per_1k_tokens_output: 0.0,
                context_window: 8_000,
                capabilities: vec!["fast".into()],
                created_at: "2026-01-01".into(),
            },
        ];

        specs.sort_by(|a, b| {
            let cost_a = a.cost_per_1k_tokens_input + a.cost_per_1k_tokens_output;
            let cost_b = b.cost_per_1k_tokens_input + b.cost_per_1k_tokens_output;
            cost_a
                .partial_cmp(&cost_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.context_window.cmp(&a.context_window))
        });

        // local-llama (free) → haiku (0.0033) → gemini-flash (0.003)
        // Actually: ollama=0.0, haiku=0.0033, gemini=0.003
        // Sort: ollama first (0.0), then gemini (0.003), then haiku (0.0033)
        assert_eq!(specs[0].name, "local-llama");
        assert_eq!(specs[1].name, "gemini-flash");
        assert_eq!(specs[2].name, "claude-haiku");
    }
}
