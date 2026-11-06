//! A bundle's DuckDB tables, read from its `schemas/duckdb/<database>.sql` at
//! compile time (CODEBASE_SPLIT_PLAN Phase 1): one reviewable file per
//! database instead of a SQL string inside Rust.
//!
//! The file is run whole with `execute_batch` on every open, so every
//! statement must be idempotent: `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF
//! NOT EXISTS`, and a column added later as `ALTER TABLE … ADD COLUMN IF NOT
//! EXISTS` after its `CREATE`, so an old database gains it and a fresh one
//! gets it the same way.
//! `scripts/check_schemas.sh` checks that.
//!
//! ```ignore
//! conn.execute_batch(duckdb_schema!(core "telemetry"))?;
//! conn.execute_batch(duckdb_schema!(plugin "tracks"))?;
//! ```

/// The calling bundle's `schemas/duckdb/<name>.sql`, as a `&'static str`:
/// `core` from a core crate, `plugin` from a plugin crate. A missing file
/// fails the build.
#[macro_export]
macro_rules! duckdb_schema {
    (core $name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/core/schemas/duckdb/",
            $name,
            ".sql"
        ))
    };
    (plugin $name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schemas/duckdb/",
            $name,
            ".sql"
        ))
    };
}
