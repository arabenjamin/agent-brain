//! Core's closed lists (CODEBASE_SPLIT_PLAN Phase 1): each fixed set of values
//! core's code owns is written to `plugins/core/schemas/lists/<name>.json`, and
//! every hand-written copy is checked against it. The tool schemas take their
//! enums from the same constants through `"${NAME}"` placeholders, and the HBI
//! takes them from `hbi-frontend/src/lib/lists.generated.ts`
//! (`scripts/gen_lists.sh`, checked by `scripts/check_schemas.sh`).
//!
//! The Rust constant is the source. After changing one, run this test with
//! `SCHEMA_REGENERATE=1`, then `scripts/gen_lists.sh`, and commit both.

use agent_brain_protocol::seed_schema_check::{ClosedList, check_lists, repo_root};
use serde_json::Value;

fn list(name: &str, description: &str, values: &[&str]) -> ClosedList {
    ClosedList {
        name: name.into(),
        description: description.into(),
        values: values.iter().map(ToString::to_string).collect(),
        file: format!("plugins/core/schemas/lists/{name}.json"),
    }
}

fn core_lists() -> Vec<ClosedList> {
    use crate::programs::{PROGRAM_KINDS, PROGRAM_STATUSES};
    use agent_brain_repository::{TODO_ORIGINS, TODO_STATUSES, WORKFLOW_KINDS};
    vec![
        list(
            "note-types",
            "A :Note's note_type. store_note offers all but code-written-note-types.",
            crate::validation::VALID_NOTE_TYPES,
        ),
        list(
            "code-written-note-types",
            "Note types only the brain's own code writes; store_note does not offer them.",
            crate::validation::CODE_WRITTEN_NOTE_TYPES,
        ),
        list("todo-statuses", "A :Todo's status.", TODO_STATUSES),
        list(
            "todo-origins",
            "The origin_kind a caller of the items API may give a :Todo; plugins also write their own (plugin, deep_research).",
            TODO_ORIGINS,
        ),
        list("program-kinds", "A :Program's kind.", PROGRAM_KINDS),
        list("program-statuses", "A :Program's status.", PROGRAM_STATUSES),
        list(
            "data-classes",
            "Who may see a run's prompts: private (only no_log providers, the default) or public.",
            &agent_brain_llm::routing::DataClass::names(),
        ),
        list(
            "workflow-checks",
            "The checks a workflow may opt into (checks).",
            crate::workflow_seeder::CHECKS,
        ),
        list(
            "workflow-kinds",
            "A :Workflow's kind (blank or null for none).",
            WORKFLOW_KINDS,
        ),
    ]
}

/// The string values of the enum at `pointer` in a hand-written schema file,
/// leaving out the blank `""` a nullable choice also takes.
fn enum_strings(root: &std::path::Path, file: &str, pointer: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
    v.pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{file}: no enum at {pointer}"))
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn core_lists_match_their_constants() {
    let root = repo_root(env!("CARGO_MANIFEST_DIR"));
    check_lists(&root, &core_lists()).unwrap_or_else(|e| panic!("{e}"));
}

/// The hand-written schemas (`schemas/program.json`, `schemas/workflow.json`)
/// are files other code serves as they are, so they hold the values
/// themselves; this keeps them equal to the list.
#[test]
fn hand_schemas_hold_the_whole_lists() {
    let root = repo_root(env!("CARGO_MANIFEST_DIR"));
    let copies = [
        (
            "plugins/core/schemas/program.json",
            "/properties/kind/enum",
            crate::programs::PROGRAM_KINDS,
        ),
        (
            "plugins/core/schemas/program.json",
            "/properties/status/enum",
            crate::programs::PROGRAM_STATUSES,
        ),
        (
            "plugins/core/schemas/workflow.json",
            "/properties/kind/enum",
            agent_brain_repository::WORKFLOW_KINDS,
        ),
        (
            "plugins/core/schemas/workflow.json",
            "/properties/checks/items/enum",
            crate::workflow_seeder::CHECKS,
        ),
        (
            "plugins/core/schemas/workflow.json",
            "/properties/data_class/enum",
            &agent_brain_llm::routing::DataClass::names(),
        ),
        (
            "plugins/core/schemas/workflow.json",
            "/$defs/step/properties/data_class/enum",
            &agent_brain_llm::routing::DataClass::names(),
        ),
    ];
    for (file, pointer, values) in copies {
        assert_eq!(
            enum_strings(&root, file, pointer),
            values.to_vec(),
            "{file} {pointer} differs from its closed list"
        );
    }
}
