//! Markdown report generator for diff reports.

use super::differ::{ChangeCategory, ChangeType, DiffReport, SpecChange};

/// Generator for markdown diff reports.
pub struct MarkdownReportGenerator;

impl MarkdownReportGenerator {
    /// Generate a full markdown diff report suitable for documentation or PRs.
    pub fn generate(report: &DiffReport) -> String {
        let mut md = String::new();

        // Header
        md.push_str(&format!("# API Diff Report: {}\n\n", report.api_name));
        md.push_str(&format!(
            "**Report Generated:** {}\n\n",
            report.generated_at.format("%Y-%m-%d %H:%M UTC")
        ));

        // Summary table
        md.push_str("## Summary\n\n");
        md.push_str("| Metric | Count |\n");
        md.push_str("|--------|-------|\n");
        md.push_str(&format!(
            "| Total Changes | {} |\n",
            report.summary.total_changes
        ));
        md.push_str(&format!(
            "| Breaking Changes | {} |\n",
            report.summary.breaking_changes
        ));
        md.push_str(&format!(
            "| Auto-Healed by AI | {} |\n",
            report.summary.healed_by_ai
        ));
        md.push_str(&format!(
            "| Endpoints Modified | {} |\n",
            report.summary.endpoints_modified
        ));
        md.push_str(&format!(
            "| Parameters Modified | {} |\n",
            report.summary.parameters_modified
        ));
        md.push('\n');

        if report.changes.is_empty() {
            md.push_str("*No changes detected. The spec matches the current graph state.*\n");
            return md;
        }

        // Breaking changes section (if any)
        let breaking: Vec<&SpecChange> = report.changes.iter().filter(|c| c.breaking).collect();
        if !breaking.is_empty() {
            md.push_str("## Breaking Changes\n\n");
            for change in &breaking {
                md.push_str(&Self::format_change(change, true));
            }
            md.push('\n');
        }

        // Group remaining changes by category
        md.push_str("## All Changes\n\n");

        // Parameter changes
        let param_changes: Vec<&SpecChange> = report
            .changes
            .iter()
            .filter(|c| c.category == ChangeCategory::Parameter)
            .collect();
        if !param_changes.is_empty() {
            md.push_str("### Parameter Changes\n\n");
            for change in &param_changes {
                md.push_str(&Self::format_change(change, false));
            }
            md.push('\n');
        }

        // Endpoint changes
        let endpoint_changes: Vec<&SpecChange> = report
            .changes
            .iter()
            .filter(|c| c.category == ChangeCategory::Endpoint)
            .collect();
        if !endpoint_changes.is_empty() {
            md.push_str("### Endpoint Changes\n\n");
            for change in &endpoint_changes {
                md.push_str(&Self::format_change(change, false));
            }
            md.push('\n');
        }

        // Schema changes
        let schema_changes: Vec<&SpecChange> = report
            .changes
            .iter()
            .filter(|c| c.category == ChangeCategory::Schema)
            .collect();
        if !schema_changes.is_empty() {
            md.push_str("### Schema Changes\n\n");
            for change in &schema_changes {
                md.push_str(&Self::format_change(change, false));
            }
            md.push('\n');
        }

        // Response changes
        let response_changes: Vec<&SpecChange> = report
            .changes
            .iter()
            .filter(|c| c.category == ChangeCategory::Response)
            .collect();
        if !response_changes.is_empty() {
            md.push_str("### Response Changes\n\n");
            for change in &response_changes {
                md.push_str(&Self::format_change(change, false));
            }
            md.push('\n');
        }

        md
    }

    /// Format a single change as a markdown list item.
    fn format_change(change: &SpecChange, include_reasoning: bool) -> String {
        let mut line = String::new();

        // Icon based on type
        let icon = if change.healed_by_ai { "🤖" } else { "📝" };
        let breaking_icon = if change.breaking { " ⚠️" } else { "" };

        line.push_str(&format!("- {}{} ", icon, breaking_icon));

        // Format based on change type
        match &change.change_type {
            ChangeType::ParameterRenamed {
                endpoint_path,
                method,
                old_name,
                new_name,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Parameter renamed `{}` -> `{}`",
                    method, endpoint_path, old_name, new_name
                ));
            }
            ChangeType::ParameterTypeChanged {
                endpoint_path,
                method,
                param_name,
                old_type,
                new_type,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Parameter `{}` type changed `{}` -> `{}`",
                    method, endpoint_path, param_name, old_type, new_type
                ));
            }
            ChangeType::ParameterAdded {
                endpoint_path,
                method,
                param_name,
                required,
                ..
            } => {
                let req = if *required { " (required)" } else { "" };
                line.push_str(&format!(
                    "`{} {}`: Added parameter `{}`{}",
                    method, endpoint_path, param_name, req
                ));
            }
            ChangeType::ParameterRemoved {
                endpoint_path,
                method,
                param_name,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Removed parameter `{}`",
                    method, endpoint_path, param_name
                ));
            }
            ChangeType::ParameterLocationChanged {
                endpoint_path,
                method,
                param_name,
                old_location,
                new_location,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Parameter `{}` location changed `{}` -> `{}`",
                    method, endpoint_path, param_name, old_location, new_location
                ));
            }
            ChangeType::ParameterRequiredChanged {
                endpoint_path,
                method,
                param_name,
                now_required,
            } => {
                let status = if *now_required {
                    "now required"
                } else {
                    "now optional"
                };
                line.push_str(&format!(
                    "`{} {}`: Parameter `{}` is {}",
                    method, endpoint_path, param_name, status
                ));
            }
            ChangeType::EndpointPathChanged {
                old_path,
                new_path,
                method,
            } => {
                line.push_str(&format!(
                    "`{}`: Path changed `{}` -> `{}`",
                    method, old_path, new_path
                ));
            }
            ChangeType::EndpointAdded { path, method } => {
                line.push_str(&format!("`{} {}`: Endpoint added", method, path));
            }
            ChangeType::EndpointRemoved { path, method } => {
                line.push_str(&format!("`{} {}`: Endpoint removed", method, path));
            }
            ChangeType::EndpointStatusChanged {
                path,
                method,
                old_status,
                new_status,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Status changed `{}` -> `{}`",
                    method, path, old_status, new_status
                ));
            }
            ChangeType::SchemaFieldAdded {
                schema_name,
                field_name,
                field_type,
            } => {
                line.push_str(&format!(
                    "Schema `{}`: Added field `{}` ({})",
                    schema_name, field_name, field_type
                ));
            }
            ChangeType::SchemaFieldRemoved {
                schema_name,
                field_name,
            } => {
                line.push_str(&format!(
                    "Schema `{}`: Removed field `{}`",
                    schema_name, field_name
                ));
            }
            ChangeType::SchemaFieldTypeChanged {
                schema_name,
                field_name,
                old_type,
                new_type,
            } => {
                line.push_str(&format!(
                    "Schema `{}`: Field `{}` type changed `{}` -> `{}`",
                    schema_name, field_name, old_type, new_type
                ));
            }
            ChangeType::ResponseSchemaChanged {
                endpoint_path,
                method,
                status_code,
                change_summary,
            } => {
                line.push_str(&format!(
                    "`{} {}`: Response {} schema changed: {}",
                    method, endpoint_path, status_code, change_summary
                ));
            }
        }

        line.push('\n');

        // Add AI reasoning if requested and available
        if include_reasoning {
            if let Some(ref trigger) = change.trigger {
                line.push_str(&format!("  - **Trigger:** {}\n", trigger));
            }
            if let Some(ref reasoning) = change.ai_reasoning {
                line.push_str(&format!("  - **AI Reasoning:** {}\n", reasoning));
            }
        }

        line
    }

    /// Generate a compact changelog suitable for git commit messages.
    pub fn generate_changelog(report: &DiffReport) -> String {
        let mut log = String::new();

        log.push_str(&format!(
            "chore(api): sync {} spec with reality\n\n",
            report.api_name
        ));

        if report.summary.healed_by_ai > 0 {
            log.push_str(&format!(
                "Auto-healed {} documentation issues:\n\n",
                report.summary.healed_by_ai
            ));
        }

        for change in report.changes.iter().take(10) {
            log.push_str(&format!("- {}\n", change.change_type.one_line_summary()));
        }

        if report.changes.len() > 10 {
            log.push_str(&format!(
                "\n...and {} more changes\n",
                report.changes.len() - 10
            ));
        }

        log
    }

    /// Generate a JSON summary for programmatic use.
    pub fn generate_json(report: &DiffReport) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::export::differ::DiffSummary;
    use chrono::Utc;

    fn create_test_report() -> DiffReport {
        DiffReport {
            api_name: "Test API".to_string(),
            generated_at: Utc::now(),
            changes: vec![
                SpecChange {
                    id: "1".to_string(),
                    category: ChangeCategory::Parameter,
                    change_type: ChangeType::ParameterRenamed {
                        endpoint_path: "/users/{id}".to_string(),
                        method: "GET".to_string(),
                        old_name: "id".to_string(),
                        new_name: "user_id".to_string(),
                    },
                    json_path: "/users/{id}.GET.parameters.user_id".to_string(),
                    breaking: true,
                    healed_by_ai: true,
                    changed_at: Some(Utc::now()),
                    trigger: Some("400 Bad Request: Missing user_id".to_string()),
                    ai_reasoning: Some("API expects user_id not id".to_string()),
                },
                SpecChange {
                    id: "2".to_string(),
                    category: ChangeCategory::Parameter,
                    change_type: ChangeType::ParameterAdded {
                        endpoint_path: "/orders".to_string(),
                        method: "POST".to_string(),
                        param_name: "customer_id".to_string(),
                        location: "query".to_string(),
                        required: true,
                    },
                    json_path: "/orders.POST.parameters.customer_id".to_string(),
                    breaking: true,
                    healed_by_ai: true,
                    changed_at: Some(Utc::now()),
                    trigger: Some("422: customer_id is required".to_string()),
                    ai_reasoning: Some("Missing required parameter".to_string()),
                },
            ],
            summary: DiffSummary {
                total_changes: 2,
                breaking_changes: 2,
                healed_by_ai: 2,
                changes_by_category: [("Parameter".to_string(), 2)].into_iter().collect(),
                endpoints_modified: 2,
                schemas_modified: 0,
                parameters_modified: 2,
            },
        }
    }

    #[test]
    fn test_generate_report() {
        let report = create_test_report();
        let markdown = MarkdownReportGenerator::generate(&report);

        assert!(markdown.contains("# API Diff Report: Test API"));
        assert!(markdown.contains("## Summary"));
        assert!(markdown.contains("Total Changes | 2"));
        assert!(markdown.contains("Breaking Changes | 2"));
        assert!(markdown.contains("## Breaking Changes"));
        assert!(markdown.contains("Parameter renamed"));
    }

    #[test]
    fn test_generate_empty_report() {
        let report = DiffReport {
            api_name: "Empty API".to_string(),
            generated_at: Utc::now(),
            changes: vec![],
            summary: DiffSummary::default(),
        };

        let markdown = MarkdownReportGenerator::generate(&report);

        assert!(markdown.contains("No changes detected"));
    }

    #[test]
    fn test_generate_changelog() {
        let report = create_test_report();
        let changelog = MarkdownReportGenerator::generate_changelog(&report);

        assert!(changelog.contains("chore(api): sync Test API spec with reality"));
        assert!(changelog.contains("Auto-healed 2 documentation issues"));
        assert!(changelog.contains("/users/{id}"));
    }

    #[test]
    fn test_generate_json() {
        let report = create_test_report();
        let json = MarkdownReportGenerator::generate_json(&report).unwrap();

        assert!(json.contains("Test API"));
        assert!(json.contains("total_changes"));
        assert!(json.contains("ParameterRenamed"));
    }

    #[test]
    fn test_format_parameter_renamed() {
        let change = SpecChange {
            id: "1".to_string(),
            category: ChangeCategory::Parameter,
            change_type: ChangeType::ParameterRenamed {
                endpoint_path: "/users".to_string(),
                method: "GET".to_string(),
                old_name: "old".to_string(),
                new_name: "new".to_string(),
            },
            json_path: "test".to_string(),
            breaking: true,
            healed_by_ai: true,
            changed_at: None,
            trigger: None,
            ai_reasoning: None,
        };

        let formatted = MarkdownReportGenerator::format_change(&change, false);

        assert!(formatted.contains("🤖"));
        assert!(formatted.contains("⚠️"));
        assert!(formatted.contains("Parameter renamed"));
        assert!(formatted.contains("`old`"));
        assert!(formatted.contains("`new`"));
    }

    #[test]
    fn test_format_with_reasoning() {
        let change = SpecChange {
            id: "1".to_string(),
            category: ChangeCategory::Parameter,
            change_type: ChangeType::ParameterAdded {
                endpoint_path: "/test".to_string(),
                method: "POST".to_string(),
                param_name: "new_param".to_string(),
                location: "query".to_string(),
                required: false,
            },
            json_path: "test".to_string(),
            breaking: false,
            healed_by_ai: true,
            changed_at: None,
            trigger: Some("Test trigger".to_string()),
            ai_reasoning: Some("Test reasoning".to_string()),
        };

        let formatted = MarkdownReportGenerator::format_change(&change, true);

        assert!(formatted.contains("**Trigger:**"));
        assert!(formatted.contains("Test trigger"));
        assert!(formatted.contains("**AI Reasoning:**"));
        assert!(formatted.contains("Test reasoning"));
    }
}
