# Refactor: Skill Implementation Consolidation

This plan aims to reduce codebase bloat by extracting repetitive boilerplate from skill implementations into the `agent-brain-protocol` crate.

## 🎯 Goals
- Reduce LOC in `crates/app/src/skills/*.rs`.
- Standardize how tool arguments are parsed and responses are returned.
- Eliminate redundant `default_x()` functions and local input structs where common patterns exist.

## 🛠 Implementation Steps

### 1. Protocol Crate Enhancements (`crates/protocol/`)
- [ ] **`ToolCallResult` Extensions**: Add `success_json(Value)` helper to handle `serde_json::to_string_pretty` and wrap it in `success_text`.
- [ ] **Argument Parsing**: Move `parse_args<<TT>(arguments: Option<<ValueValue>) -> Result<<TT, ToolCallResult>` to the protocol crate.
- [ ] **Common Parameters**: Define reusable types for common tool inputs:
    - `LimitParam` (with default 5)
    - `PaginationParam` (limit/offset)
    - `SearchOptions` (limit, graph_hops, entity_expansion)
- [ ] **Tool Definition Helpers**: Add a helper to `ToolDefinition` to reduce boilerplate when defining common schemas.

### 2. Skill Migration (`crates/app/src/skills/`)
For each skill file (starting with `knowledge.rs` as a pilot):
- [ ] **Replace Parsing logic**: Swap local `parse_args` calls for the protocol implementation.
- [ ] **Simplify Responses**: Replace manual `to_string_pretty` calls with `ToolCallResult::success_json()`.
- [ ] **Reuse Common Parameters**: Replace local `Input` struct fields with `protocol` common types where applicable.
- [ ] **Cleanup**: Remove local `default_x()` functions that are now handled by protocol types.

### 3. Verification
- [ ] Run `cargo build` to ensure no breaking changes in trait implementations.
- [ ] Run `cargo test` to verify that tool call outputs remain identical.
- [ ] Validate that the JSON schemas of tools are unchanged (to avoid breaking LLM integration).

## 📉 Expected Impact
- **LOC Reduction**: Estimated 5-10% reduction in total skill code.
- **Maintainability**: Changes to response formatting or parsing logic now happen in one place.
- **Consistency**: Uniform behavior across all 85+ tools.
