//! Export module for generating OpenAPI specs from the knowledge graph.
//!
//! This module provides functionality to:
//! - Export the healed knowledge graph back to OpenAPI 3.0 specifications
//! - Compare original specs against current graph state (diff)
//! - Generate markdown reports of documentation drift

mod builder;
mod differ;
mod exporter;
mod report;

pub use builder::OpenApiBuilder;
pub use differ::{
    ChangeCategory, ChangeType, DiffError, DiffReport, DiffSummary, SpecChange, SpecDiffer,
};
pub use exporter::{
    ExportError, ExportFormat, ExportOptions, ExportResult, ExportStats, OpenApiExporter,
};
pub use report::MarkdownReportGenerator;
