//! Specification differ for comparing original spec against healed graph state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tracing::{debug, info};

use crate::models::{HealingAction, HealingEvent};
use crate::repository::Neo4jClient;

/// Errors that can occur during diff generation.
#[derive(Debug, Error)]
pub enum DiffError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("API not found: {0}")]
    ApiNotFound(String),

    #[error("No healing history found for API")]
    NoHealingHistory,

    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),
}

/// A single change detected between original and current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecChange {
    /// Unique identifier for this change.
    pub id: String,

    /// What category of change.
    pub category: ChangeCategory,

    /// The specific change details.
    pub change_type: ChangeType,

    /// Path to the changed element (e.g., "/users/{id}.GET.parameters.id").
    pub json_path: String,

    /// Is this a breaking change?
    pub breaking: bool,

    /// Was this change made by AI healing?
    pub healed_by_ai: bool,

    /// When the change was detected/made.
    pub changed_at: Option<DateTime<Utc>>,

    /// The trigger that caused this change (e.g., error message).
    pub trigger: Option<String>,

    /// AI reasoning if healed.
    pub ai_reasoning: Option<String>,
}

/// Categories of changes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCategory {
    /// Parameter changes (name, type, required, location).
    Parameter,
    /// Endpoint changes (path, method, status).
    Endpoint,
    /// Schema changes (fields, types).
    Schema,
    /// Response changes (status codes, content types).
    Response,
    /// Resource/tag changes.
    Resource,
}

impl std::fmt::Display for ChangeCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeCategory::Parameter => write!(f, "Parameter"),
            ChangeCategory::Endpoint => write!(f, "Endpoint"),
            ChangeCategory::Schema => write!(f, "Schema"),
            ChangeCategory::Response => write!(f, "Response"),
            ChangeCategory::Resource => write!(f, "Resource"),
        }
    }
}

/// Specific types of changes detected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "details")]
pub enum ChangeType {
    // Parameter changes
    ParameterRenamed {
        endpoint_path: String,
        method: String,
        old_name: String,
        new_name: String,
    },
    ParameterTypeChanged {
        endpoint_path: String,
        method: String,
        param_name: String,
        old_type: String,
        new_type: String,
    },
    ParameterAdded {
        endpoint_path: String,
        method: String,
        param_name: String,
        location: String,
        required: bool,
    },
    ParameterRemoved {
        endpoint_path: String,
        method: String,
        param_name: String,
    },
    ParameterLocationChanged {
        endpoint_path: String,
        method: String,
        param_name: String,
        old_location: String,
        new_location: String,
    },
    ParameterRequiredChanged {
        endpoint_path: String,
        method: String,
        param_name: String,
        now_required: bool,
    },

    // Endpoint changes
    EndpointPathChanged {
        old_path: String,
        new_path: String,
        method: String,
    },
    EndpointAdded {
        path: String,
        method: String,
    },
    EndpointRemoved {
        path: String,
        method: String,
    },
    EndpointStatusChanged {
        path: String,
        method: String,
        old_status: String,
        new_status: String,
    },

    // Schema changes
    SchemaFieldAdded {
        schema_name: String,
        field_name: String,
        field_type: String,
    },
    SchemaFieldRemoved {
        schema_name: String,
        field_name: String,
    },
    SchemaFieldTypeChanged {
        schema_name: String,
        field_name: String,
        old_type: String,
        new_type: String,
    },

    // Response changes
    ResponseSchemaChanged {
        endpoint_path: String,
        method: String,
        status_code: u16,
        change_summary: String,
    },
}

impl ChangeType {
    /// Get a one-line summary of the change.
    pub fn one_line_summary(&self) -> String {
        match self {
            ChangeType::ParameterRenamed {
                endpoint_path,
                old_name,
                new_name,
                ..
            } => {
                format!("{}: param '{}' -> '{}'", endpoint_path, old_name, new_name)
            }
            ChangeType::ParameterTypeChanged {
                endpoint_path,
                param_name,
                old_type,
                new_type,
                ..
            } => {
                format!(
                    "{}: param '{}' type {} -> {}",
                    endpoint_path, param_name, old_type, new_type
                )
            }
            ChangeType::ParameterAdded {
                endpoint_path,
                param_name,
                required,
                ..
            } => {
                let req = if *required { " (required)" } else { "" };
                format!("{}: added param '{}'{}", endpoint_path, param_name, req)
            }
            ChangeType::ParameterRemoved {
                endpoint_path,
                param_name,
                ..
            } => {
                format!("{}: removed param '{}'", endpoint_path, param_name)
            }
            ChangeType::EndpointPathChanged {
                old_path, new_path, ..
            } => {
                format!("path '{}' -> '{}'", old_path, new_path)
            }
            ChangeType::EndpointStatusChanged {
                path,
                old_status,
                new_status,
                ..
            } => {
                format!("{}: status {} -> {}", path, old_status, new_status)
            }
            ChangeType::SchemaFieldAdded {
                schema_name,
                field_name,
                ..
            } => {
                format!("schema '{}': added field '{}'", schema_name, field_name)
            }
            ChangeType::SchemaFieldRemoved {
                schema_name,
                field_name,
            } => {
                format!("schema '{}': removed field '{}'", schema_name, field_name)
            }
            _ => format!("{:?}", self),
        }
    }
}

/// Summary statistics for the diff report.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    pub total_changes: usize,
    pub breaking_changes: usize,
    pub healed_by_ai: usize,
    pub changes_by_category: HashMap<String, usize>,
    pub endpoints_modified: usize,
    pub schemas_modified: usize,
    pub parameters_modified: usize,
}

/// Complete diff report comparing original spec to current graph state.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiffReport {
    /// API being compared.
    pub api_name: String,

    /// When this diff was generated.
    pub generated_at: DateTime<Utc>,

    /// All detected changes.
    pub changes: Vec<SpecChange>,

    /// Summary statistics.
    pub summary: DiffSummary,
}

/// Internal struct for healing event with endpoint info.
#[derive(Debug)]
struct HealingEventWithEndpoint {
    event: HealingEvent,
    endpoint_path: String,
    endpoint_method: String,
}

/// Service for generating diff reports.
pub struct SpecDiffer {
    neo4j: Neo4jClient,
}

impl SpecDiffer {
    /// Create a new differ with a Neo4j connection.
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
    }

    /// Generate a diff report comparing original spec to current healed graph state.
    pub async fn generate_diff(&self, api_name: Option<&str>) -> Result<DiffReport, DiffError> {
        info!(api_name = ?api_name, "Generating diff report");

        // Fetch all healing events (these ARE the changes)
        let healing_events = self.fetch_all_healing_events().await?;
        debug!("Found {} healing events", healing_events.len());

        // Convert healing events to SpecChanges
        let mut changes: Vec<SpecChange> = Vec::new();

        for event_data in healing_events {
            if let Some(change) = self.healing_event_to_change(&event_data) {
                changes.push(change);
            }
        }

        // Sort changes by timestamp (newest first)
        changes.sort_by(|a, b| b.changed_at.cmp(&a.changed_at));

        // Calculate summary
        let summary = self.calculate_summary(&changes);

        let report = DiffReport {
            api_name: api_name
                .map(|s| s.to_string())
                .unwrap_or_else(|| "All APIs".to_string()),
            generated_at: Utc::now(),
            changes,
            summary,
        };

        info!(
            total_changes = report.summary.total_changes,
            breaking = report.summary.breaking_changes,
            "Diff report generated"
        );

        Ok(report)
    }

    /// Fetch all verified healing events with their endpoint information.
    async fn fetch_all_healing_events(&self) -> Result<Vec<HealingEventWithEndpoint>, DiffError> {
        // Get all endpoints
        let endpoints = self.neo4j.list_endpoints().await?;
        let mut result = Vec::new();

        for endpoint in endpoints {
            // Get healing history for this endpoint
            let events = self.neo4j.get_healing_history(endpoint.id).await?;

            for event in events {
                // Only include verified healing events
                if event.verified {
                    result.push(HealingEventWithEndpoint {
                        event,
                        endpoint_path: endpoint.path.clone(),
                        endpoint_method: endpoint.method.to_string(),
                    });
                }
            }
        }

        Ok(result)
    }

    /// Convert a HealingEvent into a SpecChange.
    fn healing_event_to_change(&self, event_data: &HealingEventWithEndpoint) -> Option<SpecChange> {
        let event = &event_data.event;
        let endpoint_path = &event_data.endpoint_path;
        let method = &event_data.endpoint_method;

        let (category, change_type, json_path, breaking) = match &event.action {
            HealingAction::RenameParameter {
                old_name, new_name, ..
            } => (
                ChangeCategory::Parameter,
                ChangeType::ParameterRenamed {
                    endpoint_path: endpoint_path.clone(),
                    method: method.clone(),
                    old_name: old_name.clone(),
                    new_name: new_name.clone(),
                },
                format!("{}.{}.parameters.{}", endpoint_path, method, new_name),
                true, // Parameter renames are breaking
            ),
            HealingAction::ChangeParameterType {
                param_name,
                old_type,
                new_type,
            } => (
                ChangeCategory::Parameter,
                ChangeType::ParameterTypeChanged {
                    endpoint_path: endpoint_path.clone(),
                    method: method.clone(),
                    param_name: param_name.clone(),
                    old_type: old_type.clone(),
                    new_type: new_type.clone(),
                },
                format!(
                    "{}.{}.parameters.{}.type",
                    endpoint_path, method, param_name
                ),
                true, // Type changes are often breaking
            ),
            HealingAction::AddMissingParameter {
                param_name,
                required,
                ..
            } => (
                ChangeCategory::Parameter,
                ChangeType::ParameterAdded {
                    endpoint_path: endpoint_path.clone(),
                    method: method.clone(),
                    param_name: param_name.clone(),
                    location: "query".to_string(), // Default, could be enhanced
                    required: *required,
                },
                format!("{}.{}.parameters.{}", endpoint_path, method, param_name),
                *required, // Only breaking if required
            ),
            HealingAction::UpdateEndpointPath { old_path, new_path } => (
                ChangeCategory::Endpoint,
                ChangeType::EndpointPathChanged {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    method: method.clone(),
                },
                format!("{}.{}", new_path, method),
                true, // Path changes are always breaking
            ),
            HealingAction::UpdateResponseSchema {
                status_code,
                diff_summary,
            } => (
                ChangeCategory::Response,
                ChangeType::ResponseSchemaChanged {
                    endpoint_path: endpoint_path.clone(),
                    method: method.clone(),
                    status_code: *status_code,
                    change_summary: diff_summary.clone(),
                },
                format!("{}.{}.responses.{}", endpoint_path, method, status_code),
                false, // Response changes may not be breaking
            ),
        };

        Some(SpecChange {
            id: event.id.to_string(),
            category,
            change_type,
            json_path,
            breaking,
            healed_by_ai: true,
            changed_at: Some(event.timestamp),
            trigger: Some(event.trigger_error.clone()),
            ai_reasoning: Some(event.ai_reasoning.clone()),
        })
    }

    /// Calculate summary statistics from changes.
    fn calculate_summary(&self, changes: &[SpecChange]) -> DiffSummary {
        let mut summary = DiffSummary::default();
        let mut modified_endpoints: HashSet<String> = HashSet::new();
        let mut modified_schemas: HashSet<String> = HashSet::new();

        for change in changes {
            summary.total_changes += 1;

            if change.breaking {
                summary.breaking_changes += 1;
            }
            if change.healed_by_ai {
                summary.healed_by_ai += 1;
            }

            let category_key = change.category.to_string();
            *summary.changes_by_category.entry(category_key).or_insert(0) += 1;

            // Track modified endpoints/schemas
            match &change.change_type {
                ChangeType::ParameterRenamed { endpoint_path, .. }
                | ChangeType::ParameterTypeChanged { endpoint_path, .. }
                | ChangeType::ParameterAdded { endpoint_path, .. }
                | ChangeType::ParameterRemoved { endpoint_path, .. }
                | ChangeType::ParameterLocationChanged { endpoint_path, .. }
                | ChangeType::ParameterRequiredChanged { endpoint_path, .. } => {
                    modified_endpoints.insert(endpoint_path.clone());
                    summary.parameters_modified += 1;
                }
                ChangeType::EndpointPathChanged { new_path, .. } => {
                    modified_endpoints.insert(new_path.clone());
                }
                ChangeType::EndpointStatusChanged { path, .. } => {
                    modified_endpoints.insert(path.clone());
                }
                ChangeType::ResponseSchemaChanged { endpoint_path, .. } => {
                    modified_endpoints.insert(endpoint_path.clone());
                }
                ChangeType::SchemaFieldAdded { schema_name, .. }
                | ChangeType::SchemaFieldRemoved { schema_name, .. }
                | ChangeType::SchemaFieldTypeChanged { schema_name, .. } => {
                    modified_schemas.insert(schema_name.clone());
                }
                _ => {}
            }
        }

        summary.endpoints_modified = modified_endpoints.len();
        summary.schemas_modified = modified_schemas.len();
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_category_display() {
        assert_eq!(ChangeCategory::Parameter.to_string(), "Parameter");
        assert_eq!(ChangeCategory::Endpoint.to_string(), "Endpoint");
        assert_eq!(ChangeCategory::Schema.to_string(), "Schema");
    }

    #[test]
    fn test_change_type_one_line_summary() {
        let change = ChangeType::ParameterRenamed {
            endpoint_path: "/users/{id}".to_string(),
            method: "GET".to_string(),
            old_name: "id".to_string(),
            new_name: "user_id".to_string(),
        };

        let summary = change.one_line_summary();
        assert!(summary.contains("/users/{id}"));
        assert!(summary.contains("id"));
        assert!(summary.contains("user_id"));
    }

    #[test]
    fn test_diff_summary_default() {
        let summary = DiffSummary::default();
        assert_eq!(summary.total_changes, 0);
        assert_eq!(summary.breaking_changes, 0);
        assert_eq!(summary.healed_by_ai, 0);
    }

    #[test]
    fn test_spec_change_serialization() {
        let change = SpecChange {
            id: "test-id".to_string(),
            category: ChangeCategory::Parameter,
            change_type: ChangeType::ParameterRenamed {
                endpoint_path: "/users".to_string(),
                method: "GET".to_string(),
                old_name: "old".to_string(),
                new_name: "new".to_string(),
            },
            json_path: "/users.GET.parameters.new".to_string(),
            breaking: true,
            healed_by_ai: true,
            changed_at: Some(Utc::now()),
            trigger: Some("API error".to_string()),
            ai_reasoning: Some("Fixed param name".to_string()),
        };

        let json = serde_json::to_string(&change).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("parameter"));
    }

    #[test]
    fn test_diff_report_serialization() {
        let report = DiffReport {
            api_name: "Test API".to_string(),
            generated_at: Utc::now(),
            changes: vec![],
            summary: DiffSummary::default(),
        };

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("Test API"));
        assert!(json.contains("generated_at"));
    }
}
