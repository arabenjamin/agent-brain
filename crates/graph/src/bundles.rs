//! Plugin bundles (CORE_AND_PLUGINS_PLAN Phase 3a).
//!
//! A plugin is a folder under `PLUGINS_DIR` (default `./plugins`) with a
//! `plugin.yaml` and any of the seed dirs in [`MERGED_KINDS`]. The core ships
//! as the `core` bundle and is always enabled; others are named in
//! `PLUGINS_ENABLED` (comma-separated). This module finds the enabled bundles
//! and hands each kind's directories to its reader. The compiled-plugin wiring
//! and the ON-CREATE seeding are the app's `services::plugins`, which
//! re-exports this module.
//!
//! A bundle whose manifest is missing or wrong, or whose `requires` are not
//! met, is skipped with the reason; `GET /api/plugins` shows it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// The bundle every brain loads.
pub const CORE: &str = "core";

/// Where `manage_plugin` writes brain-authored bundles (`PLUGINS_RUNTIME_DIR`,
/// default `plugins-runtime`). They load only when `PLUGINS_ENABLED` names them.
pub fn runtime_dir() -> PathBuf {
    PathBuf::from(std::env::var("PLUGINS_RUNTIME_DIR").unwrap_or_else(|_| "plugins-runtime".into()))
}

/// `PLUGINS_EXTRA_DIRS`: colon-separated folders of bundles kept outside the
/// repo (the owner's private repo, mounted read-only). Searched after
/// `PLUGINS_DIR`; a bundle there loads when `PLUGINS_ENABLED` names it.
pub fn extra_dirs() -> Vec<PathBuf> {
    std::env::var("PLUGINS_EXTRA_DIRS")
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Seed dirs read from every enabled bundle.
pub const MERGED_KINDS: &[&str] = &[
    "workflows",
    "contexts",
    "sources",
    "sources-media",
    "mcp-servers",
    "schemas",
    "tools",
    "apis",
    "topics",
    "search-engines",
];

/// Seed dirs read from the core bundle only, for now: each builds one global
/// set (the entity rules, the core's own architecture) that has no merge rule
/// yet. A plugin shipping one is reported, never silently ignored. (`topics/`
/// merged in Phase 4: one topic per file, and `_scope.yaml` from the core
/// only. The relation vocabulary left with the epistemics plugin, as its
/// bundle's `relations.yaml`.)
pub const CORE_ONLY_KINDS: &[&str] = &["entities", "architecture"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires: Requires,
    /// `brain` for a bundle `manage_plugin` wrote (`PLUGINS_RUNTIME_DIR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Env vars that must be set and non-empty.
    #[serde(default)]
    pub env: Vec<String>,
    /// Other plugins that must be loaded.
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum Status {
    Loaded,
    /// Present in `PLUGINS_DIR` but not named in `PLUGINS_ENABLED`.
    NotEnabled,
    /// Enabled but not loaded, with why.
    Skipped(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Bundle {
    pub name: String,
    pub root: PathBuf,
    pub manifest: Option<Manifest>,
    #[serde(flatten)]
    pub status: Status,
    /// Things worth knowing about a loaded bundle (a core-only kind it ships).
    pub warnings: Vec<String>,
    /// In `PLUGINS_RUNTIME_DIR`: written by the brain, not shipped in git.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub runtime: bool,
    /// In one of `PLUGINS_EXTRA_DIRS` (the owner's private repo).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plugins {
    pub dir: PathBuf,
    /// Folders of bundles kept outside the repo (`PLUGINS_EXTRA_DIRS`: the
    /// owner's private repo), searched after `dir`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_dirs: Vec<PathBuf>,
    /// Where brain-written bundles live (`manage_plugin`).
    pub runtime_dir: Option<PathBuf>,
    pub bundles: Vec<Bundle>,
}

impl Plugins {
    /// The bundles under `dir`, `core` first, then `enabled` in order, then
    /// any others present (as `NotEnabled`). `env` answers `requires.env`.
    pub fn load(dir: &Path, enabled: &[String], env: &dyn Fn(&str) -> Option<String>) -> Self {
        Self::load_from(dir, &[], None, enabled, env)
    }

    /// As [`Self::load`], also finding bundles in `runtime` (brain-written).
    /// A name in both is the shipped one. A runtime bundle loads only when
    /// `enabled` names it, like any other.
    pub fn load_from(
        dir: &Path,
        extra: &[PathBuf],
        runtime: Option<&Path>,
        enabled: &[String],
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        // Where a bundle named `name` is: shipped, then an extra dir, then the
        // runtime dir. (root, runtime?, external?)
        let root_of = |name: &str| -> (PathBuf, bool, bool) {
            let shipped = dir.join(name);
            if shipped.join("plugin.yaml").is_file() {
                return (shipped, false, false);
            }
            if let Some(e) = extra
                .iter()
                .map(|d| d.join(name))
                .find(|r| r.join("plugin.yaml").is_file())
            {
                return (e, false, true);
            }
            match runtime {
                Some(r) if r.join(name).join("plugin.yaml").is_file() => {
                    (r.join(name), true, false)
                }
                _ => (shipped, false, false),
            }
        };
        let mut wanted: Vec<String> = vec![CORE.to_string()];
        for n in enabled {
            let n = n.trim();
            if !n.is_empty() && !wanted.iter().any(|w| w == n) {
                wanted.push(n.to_string());
            }
        }
        let mut bundles: Vec<Bundle> = Vec::new();
        // A plugin may require one listed after it, so resolve to a fixpoint.
        let mut pending: Vec<(String, Result<Manifest, String>)> = wanted
            .iter()
            .map(|n| (n.clone(), read_manifest(&root_of(n).0, n)))
            .collect();
        loop {
            let before = pending.len();
            let mut still = Vec::new();
            for (name, m) in pending {
                match check(&name, &m, &bundles, env) {
                    Check::Ready => bundles.push(Bundle {
                        root: root_of(&name).0,
                        warnings: core_only_warnings(&root_of(&name).0, &name),
                        runtime: root_of(&name).1,
                        external: root_of(&name).2,
                        manifest: m.ok(),
                        name,
                        status: Status::Loaded,
                    }),
                    Check::Fail(reason) => bundles.push(Bundle {
                        root: root_of(&name).0,
                        runtime: root_of(&name).1,
                        external: root_of(&name).2,
                        manifest: m.ok(),
                        name,
                        status: Status::Skipped(reason),
                        warnings: vec![],
                    }),
                    Check::Wait(_) => still.push((name, m)),
                }
            }
            if still.is_empty() {
                break;
            }
            if still.len() == before {
                for (name, m) in still {
                    let reason = match check(&name, &m, &bundles, env) {
                        Check::Wait(r) | Check::Fail(r) => r,
                        Check::Ready => unreachable!("a ready bundle is never left pending"),
                    };
                    bundles.push(Bundle {
                        root: root_of(&name).0,
                        runtime: root_of(&name).1,
                        external: root_of(&name).2,
                        manifest: m.ok(),
                        name,
                        status: Status::Skipped(reason),
                        warnings: vec![],
                    });
                }
                break;
            }
            pending = still;
        }
        // Keep the requested order for everything that resolved.
        bundles.sort_by_key(|b| wanted.iter().position(|w| *w == b.name));
        let mut others: Vec<String> = std::iter::once(dir)
            .chain(extra.iter().map(PathBuf::as_path))
            .chain(runtime)
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flat_map(|e| e.flatten())
            .filter(|e| e.path().join("plugin.yaml").is_file())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| !wanted.contains(n))
            .collect();
        others.sort();
        others.dedup();
        for name in others {
            let (root, runtime, external) = root_of(&name);
            bundles.push(Bundle {
                manifest: read_manifest(&root, &name).ok(),
                root,
                runtime,
                external,
                name,
                status: Status::NotEnabled,
                warnings: vec![],
            });
        }
        Self {
            dir: dir.to_path_buf(),
            extra_dirs: extra.to_vec(),
            runtime_dir: runtime.map(Path::to_path_buf),
            bundles,
        }
    }

    /// From `PLUGINS_DIR` and `PLUGINS_ENABLED`, once per process.
    pub fn global() -> Arc<Plugins> {
        static P: OnceLock<Arc<Plugins>> = OnceLock::new();
        P.get_or_init(|| {
            let dir =
                PathBuf::from(std::env::var("PLUGINS_DIR").unwrap_or_else(|_| "plugins".into()));
            let enabled: Vec<String> = std::env::var("PLUGINS_ENABLED")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let p = Plugins::load_from(&dir, &extra_dirs(), Some(&runtime_dir()), &enabled, &|k| {
                std::env::var(k).ok()
            });
            for b in &p.bundles {
                match &b.status {
                    Status::Loaded => info!(plugin = %b.name, "Plugin loaded"),
                    Status::NotEnabled => debug!(plugin = %b.name, "Plugin present, not enabled"),
                    Status::Skipped(r) => warn!(plugin = %b.name, reason = %r, "Plugin skipped"),
                }
                for w in &b.warnings {
                    warn!(plugin = %b.name, "{w}");
                }
            }
            Arc::new(p)
        })
        .clone()
    }

    pub fn loaded(&self) -> impl Iterator<Item = &Bundle> {
        self.bundles.iter().filter(|b| b.status == Status::Loaded)
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded().any(|b| b.name == name)
    }

    /// Whether the core bundle loaded. Without it the brain must not start.
    pub fn core_loaded(&self) -> Result<(), String> {
        match self
            .bundles
            .iter()
            .find(|b| b.name == CORE)
            .map(|b| &b.status)
        {
            Some(Status::Loaded) => Ok(()),
            Some(Status::Skipped(r)) => Err(r.clone()),
            _ => Err(format!("no core bundle in {}", self.dir.display())),
        }
    }

    /// `(plugin, dir)` for each loaded bundle that has `kind/`, core first.
    pub fn dirs(&self, kind: &str) -> Vec<(String, PathBuf)> {
        self.loaded()
            .map(|b| (b.name.clone(), b.root.join(kind)))
            .filter(|(_, d)| d.is_dir())
            .collect()
    }

    /// `(plugin, file)` for every `*.<ext>` in `kind/` of each loaded bundle,
    /// core first, sorted by name within a bundle.
    pub fn files(&self, kind: &str, ext: &str) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for (plugin, dir) in self.dirs(kind) {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|e| e.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.retain(|p| p.extension().and_then(|e| e.to_str()) == Some(ext));
            files.sort();
            out.extend(files.into_iter().map(|f| (plugin.clone(), f)));
        }
        out
    }

    /// The core bundle's `kind/` dir (whether or not it exists).
    pub fn core_dir(&self, kind: &str) -> PathBuf {
        self.dir.join(CORE).join(kind)
    }
}

enum Check {
    Ready,
    Wait(String),
    Fail(String),
}

fn check(
    name: &str,
    m: &Result<Manifest, String>,
    loaded: &[Bundle],
    env: &dyn Fn(&str) -> Option<String>,
) -> Check {
    let m = match m {
        Ok(m) => m,
        Err(e) => return Check::Fail(e.clone()),
    };
    let unset: Vec<&str> = m
        .requires
        .env
        .iter()
        .filter(|k| env(k).is_none_or(|v| v.trim().is_empty()))
        .map(String::as_str)
        .collect();
    if !unset.is_empty() {
        return Check::Fail(format!("needs env {}", unset.join(", ")));
    }
    for p in &m.requires.plugins {
        match loaded.iter().find(|b| b.name == *p).map(|b| &b.status) {
            Some(Status::Loaded) => {}
            Some(_) => return Check::Fail(format!("needs plugin '{p}', which did not load")),
            None if p == name => return Check::Fail("requires itself".into()),
            None => return Check::Wait(format!("needs plugin '{p}', which is not enabled")),
        }
    }
    Check::Ready
}

pub fn read_manifest(root: &Path, name: &str) -> Result<Manifest, String> {
    let path = root.join("plugin.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let m: Manifest = serde_yaml::from_str(&text).map_err(|e| format!("plugin.yaml: {e}"))?;
    if m.name != name {
        return Err(format!(
            "plugin.yaml names '{}' but the folder is '{name}'",
            m.name
        ));
    }
    Ok(m)
}

fn core_only_warnings(root: &Path, name: &str) -> Vec<String> {
    if name == CORE {
        return vec![];
    }
    CORE_ONLY_KINDS
        .iter()
        .filter(|k| root.join(k).is_dir())
        .map(|k| {
            format!("{k}/ is read from the core bundle only; this plugin's {k}/ is not loaded")
        })
        .collect()
}

// ── Plugin code status (what each bundle's code and `tools/` contributed) ──

/// What a bundle's Rust code contributed, or why it contributed nothing.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CodeStatus {
    pub tools: Vec<String>,
    pub error: Option<String>,
}

fn code_cell() -> &'static std::sync::RwLock<HashMap<String, CodeStatus>> {
    static C: OnceLock<std::sync::RwLock<HashMap<String, CodeStatus>>> = OnceLock::new();
    C.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

fn declared_cell() -> &'static std::sync::RwLock<HashMap<String, CodeStatus>> {
    static C: OnceLock<std::sync::RwLock<HashMap<String, CodeStatus>>> = OnceLock::new();
    C.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

/// Record what each bundle's `tools/` contributed (skills/bundle_tools.rs).
pub fn set_declared_status(status: HashMap<String, CodeStatus>) {
    *declared_cell().write().unwrap_or_else(|e| e.into_inner()) = status;
}

/// What each bundle's `tools/` contributed, or why not.
pub fn declared_status() -> HashMap<String, CodeStatus> {
    declared_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The code status recorded at startup, by plugin (for `GET /api/plugins`).
pub fn code_status() -> HashMap<String, CodeStatus> {
    code_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Record the code status at startup (`services::plugins::plugin_skills`).
pub fn set_code_status(status: HashMap<String, CodeStatus>) {
    *code_cell().write().unwrap_or_else(|e| e.into_inner()) = status;
}

/// A plugin's `start` hook failed: its tools stay, the error is shown.
pub fn record_code_error(plugin: &str, error: String) {
    if let Some(st) = code_cell()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(plugin)
    {
        st.error = Some(error);
    }
}
