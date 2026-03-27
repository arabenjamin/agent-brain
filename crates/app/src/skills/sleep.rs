//! Sleep Skill - Provides tools for memory consolidation and training data export.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::info;

use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use crate::repository::TelemetryClient;
use crate::services::SleepService;
use crate::skills::Skill;

/// Sleep Skill implementation.
pub struct SleepSkill {
    service: SleepService,
}

impl SleepSkill {
    /// Create a new sleep skill.
    pub fn new(telemetry: TelemetryClient, dataset_dir: PathBuf) -> Self {
        let service = SleepService::new(telemetry, dataset_dir);
        Self { service }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn digest_experiences_def() -> ToolDefinition {
        ToolDefinition {
            name: "digest_experiences".to_string(),
            description: "Run the agent's 'Sleep' cycle: Export successful interactions from the \
                         Hippocampus (telemetry) into a JSONL dataset for future fine-tuning. \
                         This should be run periodically (e.g., daily) to create training data."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "min_score": {
                        "type": "integer",
                        "description": "Optional minimum feedback score (1-5) to include. If omitted, all successful interactions are exported."
                    }
                }
            }),
        }
    }

    fn analyze_gaps_def() -> ToolDefinition {
        ToolDefinition {
            name: "analyze_gaps".to_string(),
            description: "Analyze recent knowledge gaps (where the agent failed to answer or lacked tools). \
                         Use this to identify learning opportunities or missing capabilities."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Number of recent gaps to analyze (default: 20)"
                    }
                }
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_digest_experiences(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DigestExperiencesInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!("Starting sleep cycle (digest_experiences)");

        // Blocking call (file I/O + DB query) within async context
        // For heavy workloads, this should be offloaded to spawn_blocking, 
        // but for <10k rows it's negligible.
        let result = self.service.digest_experiences(input.min_score);

        match result {
            Ok((path, count)) => {
                let response = json!({
                    "success": true,
                    "exported_count": count,
                    "dataset_path": path.to_string_lossy(),
                    "message": format!("Sleep cycle complete. Digested {} experiences.", count)
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Sleep cycle failed: {}", e)),
        }
    }

    async fn handle_analyze_gaps(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: AnalyzeGapsInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let limit = input.limit.unwrap_or(20);
        info!(limit = limit, "Analyzing knowledge gaps");

        match self.service.analyze_gaps(limit) {
            Ok(gaps) => {
                let count = gaps.len();
                let response = json!({
                    "count": count,
                    "gaps": gaps
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Gap analysis failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for SleepSkill {
    fn name(&self) -> &str {
        "Sleep / Training Data Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::digest_experiences_def(),
            Self::analyze_gaps_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "digest_experiences" => Some(self.handle_digest_experiences(arguments).await),
            "analyze_gaps" => Some(self.handle_analyze_gaps(arguments).await),
            _ => None,
        }
    }
}

// Input structs
#[derive(Debug, Deserialize)]
struct DigestExperiencesInput {
    #[serde(default)]
    min_score: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct AnalyzeGapsInput {
    #[serde(default)]
    limit: Option<usize>,
}

fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Option<Value>,
) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
