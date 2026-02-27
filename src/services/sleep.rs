//! Sleep Service - Manages the "Sleep" cycle for memory consolidation and training data generation.

use anyhow::Result;
use serde_json::json;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use tracing::info;

use crate::repository::TelemetryClient;

/// Service for processing raw experiences into training data.
pub struct SleepService {
    telemetry: TelemetryClient,
    dataset_dir: PathBuf,
}

impl SleepService {
    /// Create a new SleepService.
    pub fn new(telemetry: TelemetryClient, dataset_dir: PathBuf) -> Self {
        Self {
            telemetry,
            dataset_dir,
        }
    }

    /// Run the sleep cycle: Export successful interactions to a JSONL dataset.
    /// Returns the path to the generated dataset and the count of examples.
    pub fn digest_experiences(&self, min_score: Option<i32>) -> Result<(PathBuf, usize)> {
        // Ensure dataset directory exists
        if !self.dataset_dir.exists() {
            fs::create_dir_all(&self.dataset_dir)?;
        }

        // Fetch raw examples from Hippocampus (DuckDB)
        let examples = self.telemetry.get_training_examples(min_score)?;
        let count = examples.len();

        if count == 0 {
            return Ok((self.dataset_dir.clone(), 0));
        }

        // Generate filename with timestamp
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let filename = format!("instruction_tuning_{}.jsonl", timestamp);
        let file_path = self.dataset_dir.join(&filename);
        
        let mut file = File::create(&file_path)?;

        // Convert to ChatML/Instruction format and write to JSONL
        for (prompt, response) in examples {
            // Format: {"messages": [{"role": "user", "content": "..."}, {"role": "assistant", "content": "..."}]}
            let entry = json!({
                "messages": [
                    {
                        "role": "user",
                        "content": prompt
                    },
                    {
                        "role": "assistant",
                        "content": response
                    }
                ]
            });

            writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        }

        info!(
            "Sleep cycle complete. Digested {} experiences into {:?}",
            count, file_path
        );

        Ok((file_path, count))
    }

    /// Report on recent knowledge gaps (where the agent failed).
    pub fn analyze_gaps(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        let gaps = self.telemetry.get_recent_gaps(limit)?;
        
        // Transform into structured objects
        let result = gaps.into_iter().map(|(query, context, gap_type)| {
            json!({
                "query": query,
                "context": if context.is_empty() { None } else { Some(context) },
                "gap_type": gap_type
            })
        }).collect();

        Ok(result)
    }
}
