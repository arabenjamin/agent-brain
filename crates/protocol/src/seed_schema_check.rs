//! The shared check behind every crate's seed-schema test
//! (CODEBASE_SPLIT_PLAN Phase 1). A seed kind's JSON Schema is generated from
//! the Rust type that parses it (`schemars`, a dev-dependency) and committed
//! as `plugins/<bundle>/schemas/seeds/<kind>.json`, so the shape of every seed
//! file can be read, diffed and used by an editor without reading Rust.
//!
//! [`check`] fails when the committed file differs from a fresh generation
//! (rerun with `SCHEMA_REGENERATE=1` to rewrite it), or when a shipped seed
//! file does not validate against it, which is also how a generated schema
//! that misdescribes its type gets caught.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// One seed kind: its generated schema, where the schema is committed, and the
/// seed files it must accept.
pub struct SeedKind {
    /// For messages: `views`, `plugin.yaml`, …
    pub kind: String,
    /// `serde_json::to_value(schemars::schema_for!(T))`.
    pub schema: Value,
    /// Repo-relative, e.g. `plugins/core/schemas/seeds/views.json`.
    pub schema_file: String,
    /// Seed files to validate: repo-relative, or absolute for a bundle kept
    /// outside the repo (the private one).
    pub seeds: Vec<PathBuf>,
    /// The repo ships none of this kind (the private bundle holds them), so
    /// an empty `seeds` is expected, not a sign the directory name is wrong.
    pub none_shipped: bool,
}

/// The repo root, from a crate's `CARGO_MANIFEST_DIR`: `crates/<c>` or
/// `plugins/<p>/crate`.
pub fn repo_root(manifest_dir: &str) -> PathBuf {
    let d = Path::new(manifest_dir);
    if d.file_name().is_some_and(|n| n == "crate") {
        d.join("../../..")
    } else {
        d.join("../..")
    }
}

/// The bundle directories: the repo's `plugins/*`, then (absolute) every
/// bundle of the private repo when it is checked out beside this one
/// (`../agent-brain-personal/<bundle>/plugin.yaml`). CI has no private repo,
/// so there only the repo's bundles are checked.
fn bundles(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root.join("plugins"))
        .map(|d| d.flatten().map(|b| b.path()).collect())
        .unwrap_or_default();
    if let Ok(private) = std::fs::read_dir(root.join("../agent-brain-personal")) {
        out.extend(
            private
                .flatten()
                .map(|b| b.path())
                .filter(|b| b.join("plugin.yaml").is_file())
                .filter_map(|b| b.canonicalize().ok()),
        );
    }
    out.sort();
    out
}

/// Every bundle's seed files of a kind (`<bundle>/<dir>/*.yaml`), leaving out
/// `_`-prefixed meta files unless `meta` names one (`_kinds.yaml`).
/// Repo-relative for the repo's bundles, absolute for private ones.
pub fn seed_files(root: &Path, dir: &str, meta: Option<&str>) -> Vec<PathBuf> {
    let mut out = vec![];
    for b in bundles(root) {
        let Ok(entries) = std::fs::read_dir(b.join(dir)) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let yaml = name.ends_with(".yaml") || name.ends_with(".yml");
            let wanted = match meta {
                Some(m) => name == m,
                None => !name.starts_with('_'),
            };
            if yaml && wanted {
                out.push(p.strip_prefix(root).unwrap_or(&p).to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Every bundle's file of one name at the bundle root (`plugin.yaml`,
/// `read-rules.yaml`), private bundles included as in [`seed_files`].
pub fn bundle_files(root: &Path, file: &str) -> Vec<PathBuf> {
    bundles(root)
        .into_iter()
        .map(|b| b.join(file))
        .filter(|p| p.is_file())
        .map(|p| p.strip_prefix(root).map(Path::to_path_buf).unwrap_or(p))
        .collect()
}

/// Compare (or, under `SCHEMA_REGENERATE=1`, write) each kind's committed
/// schema, then validate its seed files. `Err` lists every problem.
pub fn check(root: &Path, kinds: &[SeedKind]) -> Result<(), String> {
    let regenerate = std::env::var_os("SCHEMA_REGENERATE").is_some();
    let mut problems = vec![];
    for k in kinds {
        let text = serde_json::to_string_pretty(&k.schema).expect("a schema serializes") + "\n";
        let path = root.join(&k.schema_file);
        if regenerate {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("create schemas/seeds");
            }
            std::fs::write(&path, &text).expect("write the schema file");
        } else {
            match std::fs::read_to_string(&path) {
                Ok(committed) if committed == text => {}
                Ok(_) => problems.push(format!(
                    "{} is stale for {}: its Rust type changed. Rerun this test with SCHEMA_REGENERATE=1 and commit the file.",
                    k.schema_file, k.kind
                )),
                Err(_) => problems.push(format!(
                    "{} is missing for {}: rerun this test with SCHEMA_REGENERATE=1 and commit it.",
                    k.schema_file, k.kind
                )),
            }
        }
        let validator = match jsonschema::validator_for(&k.schema) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!(
                    "{}: the generated schema is not valid: {e}",
                    k.kind
                ));
                continue;
            }
        };
        if k.seeds.is_empty() && !k.none_shipped {
            problems.push(format!("{}: no seed files found to validate", k.kind));
        }
        for seed in &k.seeds {
            let text = match std::fs::read_to_string(root.join(seed)) {
                Ok(t) => t,
                Err(e) => {
                    problems.push(format!("{}: cannot read: {e}", seed.display()));
                    continue;
                }
            };
            let value: Value = match serde_yaml::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    problems.push(format!("{}: not YAML: {e}", seed.display()));
                    continue;
                }
            };
            let errors: Vec<String> = validator
                .iter_errors(&value)
                .take(3)
                .map(|e| format!("{} at {}", e, e.instance_path))
                .collect();
            if !errors.is_empty() {
                problems.push(format!(
                    "{} does not fit the {} schema: {}",
                    seed.display(),
                    k.kind,
                    errors.join("; ")
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}
