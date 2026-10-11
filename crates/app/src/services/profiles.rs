//! Agent profiles in the graph (CORE_AND_PLUGINS_PLAN Phase 3b).
//!
//! `(:ContextProfile {name})` holds each profile: `definition` (the profile as
//! JSON), `managed_by` (`yaml` | `runtime`), `plugin` and `source` (the file).
//! Startup seeds it from every loaded bundle's `contexts/`, the way workflows
//! are seeded: absent or yaml-owned → written from the file; runtime-owned →
//! left alone; yaml-owned with no file any more → removed. The HBI writes the
//! graph (runtime-owned), never the repo.
//!
//! A profile's prompt is its `system_prompt` followed by `sections`, each
//! kept only when the tools it `requires` are available to the profile;
//! `generate: tools` renders the tool catalogue. A bundle file with `extends:`
//! adds sections to another profile; those are rebuilt from the loaded
//! bundles at every start, so disabling a plugin removes its prompt text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::repository::Neo4jClient;
use crate::services::context_builder::ContextProfile;

/// One part of a profile's prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromptSection {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Tools the section is about; it is left out unless every one is
    /// available to the profile (its allowlist, intersected with the registry).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// `tools`: the catalogue of the profile's available tools, one line each
    /// from the registry's descriptions. Replaces a hand-written list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<String>,
}

/// A bundle file that adds sections to another bundle's profile.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileExtension {
    pub extends: String,
    #[serde(default)]
    pub description: String,
    pub sections: Vec<PromptSection>,
}

/// Sections other bundles add to profiles: target name → (plugin, section).
pub type Extensions = HashMap<String, Vec<(String, PromptSection)>>;

/// Where a profile came from and who owns it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProfileMeta {
    /// `yaml` (written from its file on every start) or `runtime` (edited in
    /// the HBI; the seeder leaves it alone).
    pub managed_by: Option<String>,
    pub plugin: Option<String>,
    /// The file it is seeded from, relative to `PLUGINS_DIR`.
    pub source: Option<String>,
}

/// What a profile names that does not exist (checked once the registry is final).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProfileAudit {
    /// Allowlist entries with no registered tool ("x (removed; use y)").
    pub missing_tools: Vec<String>,
    /// Removed tools its prompt still names ("x (removed; use y)").
    pub stale_mentions: Vec<String>,
    /// Allowlist entries whose plugin is off ("x (plugin p is off)"): not
    /// missing, and not warned about; they return with the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_off: Vec<String>,
}

/// What the bundles declare: profiles and extensions, each with its plugin
/// and file.
#[derive(Debug, Default)]
pub struct BundleProfiles {
    pub profiles: Vec<(String, PathBuf, ContextProfile)>,
    pub extensions: Vec<(String, PathBuf, ProfileExtension)>,
}

impl BundleProfiles {
    /// Read the core's `contexts/` and then each plugin's. In the core, two
    /// files with one name: the later (sorted) wins, with a warning. From a
    /// plugin, a name an earlier bundle declared is skipped.
    pub fn read(core: &Path, plugin_dirs: &[(String, PathBuf)]) -> Self {
        let mut out = Self::default();
        let mut dirs: Vec<(String, PathBuf)> = vec![(
            crate::services::plugins::CORE.to_string(),
            core.to_path_buf(),
        )];
        dirs.extend(plugin_dirs.iter().cloned());
        let mut seen: HashMap<String, (String, PathBuf)> = HashMap::new();
        for (plugin, dir) in dirs {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|e| e.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"));
            files.sort();
            for path in files {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                // boot.yaml and init.yaml are protocol files, not profiles.
                if stem == "boot" || stem == "init" {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to read context profile");
                        continue;
                    }
                };
                let raw: Value = match serde_yaml::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to parse context profile");
                        continue;
                    }
                };
                if raw.get("extends").is_some() {
                    match serde_json::from_value::<ProfileExtension>(raw) {
                        Ok(x) => out.extensions.push((plugin.clone(), path, x)),
                        Err(e) => {
                            warn!(path = %path.display(), error = %e, "Failed to parse profile extension")
                        }
                    }
                    continue;
                }
                let profile: ContextProfile = match serde_json::from_value(raw) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to parse context profile");
                        continue;
                    }
                };
                if let Some((first_plugin, first)) = seen.get(&profile.name) {
                    if plugin != crate::services::plugins::CORE
                        || first_plugin != crate::services::plugins::CORE
                    {
                        warn!(
                            name = %profile.name,
                            first = %first.display(),
                            skipped = %path.display(),
                            "A plugin declares a profile an earlier bundle already has — skipped"
                        );
                        continue;
                    }
                    // Two core files, one name. The UI used to write
                    // `<profile.name>.yaml` beside a differently-named file
                    // (2026-09-12: the front door's model was a coin flip per
                    // restart). Sorted, the later wins; the warning gets it fixed.
                    warn!(
                        name = %profile.name,
                        overridden = %first.display(),
                        winner = %path.display(),
                        "DUPLICATE context profile name — two files define it and the later one wins. Delete one."
                    );
                    out.profiles.retain(|(_, _, p)| p.name != profile.name);
                }
                seen.insert(profile.name.clone(), (plugin.clone(), path.clone()));
                out.profiles.push((plugin.clone(), path, profile));
            }
        }
        out
    }

    /// Extension sections per target profile, in bundle order.
    pub fn sections_by_target(&self) -> Extensions {
        let mut m: Extensions = HashMap::new();
        for (plugin, _, x) in &self.extensions {
            m.entry(x.extends.clone())
                .or_default()
                .extend(x.sections.iter().cloned().map(|s| (plugin.clone(), s)));
        }
        m
    }
}

/// `path` relative to `PLUGINS_DIR`, for showing where a profile is defined.
pub(crate) fn source_of(path: &Path) -> String {
    let base = crate::services::plugins::Plugins::global().dir.clone();
    path.strip_prefix(&base)
        .map(|p| format!("plugins/{}", p.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

/// Write the bundles' profiles into the graph (see the module doc). Returns
/// how many were written.
pub async fn seed(neo4j: &Neo4jClient, bundles: &BundleProfiles) -> anyhow::Result<usize> {
    let mut written = 0usize;
    let mut names: Vec<String> = Vec::new();
    for (plugin, path, profile) in &bundles.profiles {
        names.push(profile.name.clone());
        let rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (c:ContextProfile {name: $name}) \
                     RETURN c.managed_by AS managed_by, c.definition IS NOT NULL AS defined",
                )
                .param("name", profile.name.as_str()),
            )
            .await?;
        let runtime = rows.first().is_some_and(|r| {
            r.get::<String>("managed_by").ok().as_deref() == Some("runtime")
                && r.get::<bool>("defined").unwrap_or(false)
        });
        if runtime {
            warn!(
                name = %profile.name,
                file = %path.display(),
                "Profile is runtime-owned — file skipped (POST /api/contexts/{{name}}/reset hands it back)"
            );
            continue;
        }
        write(neo4j, profile, "yaml", Some(plugin), Some(&source_of(path))).await?;
        written += 1;
    }
    // A yaml-owned profile no loaded bundle declares any more is gone: its
    // file was deleted or its plugin disabled.
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:ContextProfile {managed_by: 'yaml'}) \
                 WHERE c.definition IS NOT NULL AND NOT c.name IN $names \
                 WITH c, c.name AS name, c.source AS source DETACH DELETE c \
                 RETURN name, source",
            )
            .param("names", names),
        )
        .await?;
    for r in rows {
        info!(
            name = %r.get::<String>("name").unwrap_or_default(),
            source = %r.get::<String>("source").unwrap_or_default(),
            "Profile removed: no loaded bundle declares it any more"
        );
    }
    Ok(written)
}

/// Store a profile's definition and ownership.
pub async fn write(
    neo4j: &Neo4jClient,
    profile: &ContextProfile,
    managed_by: &str,
    plugin: Option<&str>,
    source: Option<&str>,
) -> anyhow::Result<()> {
    let definition = serde_json::to_string(profile)?;
    neo4j
        .run(
            neo4rs::query(
                "MERGE (c:ContextProfile {name: $name}) \
                 SET c.definition = $definition, c.managed_by = $managed_by, \
                     c.plugin = $plugin, c.source = $source, c.description = $description, \
                     c.updated_at = datetime()",
            )
            .param("name", profile.name.as_str())
            .param("definition", definition)
            .param("managed_by", managed_by)
            .param("plugin", plugin.unwrap_or_default())
            .param("source", source.unwrap_or_default())
            .param("description", profile.description.as_str()),
        )
        .await?;
    Ok(())
}

/// Every stored profile with its ownership. A definition that no longer
/// parses is skipped with a warning.
pub async fn load(neo4j: &Neo4jClient) -> anyhow::Result<Vec<(ContextProfile, ProfileMeta)>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (c:ContextProfile) WHERE c.definition IS NOT NULL \
             RETURN c.name AS name, c.definition AS definition, c.managed_by AS managed_by, \
                    c.plugin AS plugin, c.source AS source ORDER BY name",
        ))
        .await?;
    let mut out = Vec::new();
    let some = |s: String| (!s.is_empty()).then_some(s);
    for r in rows {
        let name = r.get::<String>("name").unwrap_or_default();
        let definition = r.get::<String>("definition").unwrap_or_default();
        match serde_json::from_str::<ContextProfile>(&definition) {
            Ok(p) => out.push((
                p,
                ProfileMeta {
                    managed_by: r.get::<String>("managed_by").ok().and_then(some),
                    plugin: r.get::<String>("plugin").ok().and_then(some),
                    source: r.get::<String>("source").ok().and_then(some),
                },
            )),
            Err(e) => warn!(%name, error = %e, "Stored profile does not parse — skipped"),
        }
    }
    Ok(out)
}

/// Remove a profile from the graph.
pub async fn delete(neo4j: &Neo4jClient, name: &str) -> anyhow::Result<()> {
    neo4j
        .run(
            neo4rs::query("MATCH (c:ContextProfile {name: $name}) DETACH DELETE c")
                .param("name", name),
        )
        .await?;
    Ok(())
}

/// The tools a profile can use: its allowlist (every registered tool when it
/// has none) that the registry holds. An empty registry means "not known
/// yet", and the allowlist is taken as it is.
pub fn available_tools(profile: &ContextProfile, registry: &[(String, String)]) -> Vec<String> {
    if registry.is_empty() {
        return profile.tools.clone();
    }
    if profile.tools.is_empty() {
        return registry.iter().map(|(n, _)| n.clone()).collect();
    }
    profile
        .tools
        .iter()
        .filter(|t| registry.iter().any(|(n, _)| n == *t))
        .cloned()
        .collect()
}

/// The first sentence of a tool description, at most `max` chars.
fn gist(description: &str, max: usize) -> String {
    let d = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = d.find(". ").map(|i| i + 1).unwrap_or(d.len());
    let first = &d[..end];
    if first.chars().count() <= max {
        first.to_string()
    } else {
        let cut: String = first.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The prompt a profile runs with: `system_prompt`, then its sections and the
/// extensions' sections whose required tools are all available.
pub fn render(
    profile: &ContextProfile,
    registry: &[(String, String)],
    extensions: &[(String, PromptSection)],
) -> String {
    let available = available_tools(profile, registry);
    let mut parts: Vec<String> = Vec::new();
    if !profile.system_prompt.trim().is_empty() {
        parts.push(profile.system_prompt.trim_end().to_string());
    }
    let sections = profile
        .sections
        .iter()
        .chain(extensions.iter().map(|(_, s)| s));
    for s in sections {
        if !s.requires.iter().all(|t| available.contains(t)) {
            continue;
        }
        let mut text = s.text.trim_end().to_string();
        if s.generate.as_deref() == Some("tools") {
            let lines: Vec<String> = available
                .iter()
                .map(|t| {
                    let d = registry
                        .iter()
                        .find(|(n, _)| n == t)
                        .map(|(_, d)| gist(d, 100))
                        .unwrap_or_default();
                    if d.is_empty() {
                        format!("- {t}")
                    } else {
                        format!("- {t} — {d}")
                    }
                })
                .collect();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&lines.join("\n"));
        }
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    parts.join("\n\n")
}

/// Check a profile against the registry and the tombstones of removed tools.
pub fn audit(
    profile: &ContextProfile,
    registry: &[String],
    removed: &HashMap<String, Option<String>>,
    extensions: &[(String, PromptSection)],
    plugin_off: &HashMap<String, String>,
) -> ProfileAudit {
    let label = |t: &str| match removed.get(t) {
        Some(Some(by)) => format!("{t} (removed; use {by})"),
        Some(None) => format!("{t} (removed)"),
        None => t.to_string(),
    };
    let missing_tools = if registry.is_empty() {
        Vec::new()
    } else {
        profile
            .tools
            .iter()
            .filter(|t| !registry.contains(t) && !plugin_off.contains_key(*t))
            .map(|t| label(t))
            .collect()
    };
    let off: Vec<String> = profile
        .tools
        .iter()
        .filter(|t| !registry.contains(t))
        .filter_map(|t| {
            plugin_off
                .get(t)
                .map(|p| format!("{t} (plugin {p} is off)"))
        })
        .collect();
    let mut prose = profile.system_prompt.clone();
    for s in profile
        .sections
        .iter()
        .chain(extensions.iter().map(|(_, s)| s))
    {
        prose.push('\n');
        prose.push_str(&s.text);
        for r in &s.requires {
            prose.push(' ');
            prose.push_str(r);
        }
    }
    let mut stale_mentions: Vec<String> = removed
        .keys()
        .filter(|t| names_tool(&prose, t))
        .map(|t| label(t))
        .collect();
    stale_mentions.sort();
    ProfileAudit {
        missing_tools,
        stale_mentions,
        plugin_off: off,
    }
}

/// Whether `text` names the tool `name`. A name with `_` is an identifier
/// wherever it stands; a plain word (`context`) counts only as a call,
/// `context(`: in backticks it is as often an argument name, and in prose
/// an ordinary word.
fn names_tool(text: &str, name: &str) -> bool {
    if name.contains('_') {
        names_word(text, name)
    } else {
        text.contains(&format!("{name}("))
    }
}

/// Whether `word` occurs in `text` as a whole identifier.
fn names_word(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// Lay `body` over `stored` (both profile JSON): a key the body sends wins,
/// a key it leaves out keeps its stored value. A form that does not know a
/// field cannot erase it.
pub fn overlay(stored: Option<&ContextProfile>, body: &Value) -> anyhow::Result<ContextProfile> {
    let mut merged = match stored {
        Some(p) => serde_json::to_value(p)?,
        None => Value::Object(Default::default()),
    };
    if let (Some(m), Some(b)) = (merged.as_object_mut(), body.as_object()) {
        for (k, v) in b {
            m.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(merged).map_err(|e| anyhow::anyhow!("profile: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(v: Value) -> ContextProfile {
        let mut base = json!({"name": "p", "description": "d"});
        for (k, x) in v.as_object().unwrap() {
            base[k] = x.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn reg(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|n| (n.to_string(), format!("Does {n}. More detail here.")))
            .collect()
    }

    #[test]
    fn sections_render_only_when_their_tools_are_available() {
        let p = profile(json!({
            "tools": ["search_web", "mcp_call", "gone"],
            "system_prompt": "BASE",
            "sections": [
                {"name": "a", "text": "ALWAYS"},
                {"name": "b", "text": "NEEDS MCP", "requires": ["mcp_call"]},
                {"name": "c", "text": "NEEDS EXEC", "requires": ["execute_code"]},
                {"name": "t", "text": "Tools:", "generate": "tools"}
            ]
        }));
        let out = render(&p, &reg(&["search_web", "mcp_call", "execute_code"]), &[]);
        assert_eq!(
            out,
            "BASE\n\nALWAYS\n\nNEEDS MCP\n\nTools:\n- search_web — Does search_web.\n- mcp_call — Does mcp_call."
        );
        // Not in the allowlist ⇒ not available, even though registered.
        assert!(!out.contains("NEEDS EXEC"));
        // The same profile on a brain without the MCP client.
        let out = render(&p, &reg(&["search_web"]), &[]);
        assert!(!out.contains("NEEDS MCP") && !out.contains("mcp_call —"));
    }

    #[test]
    fn extensions_follow_the_profile_and_obey_requires() {
        let p = profile(json!({"tools": ["mcp_call"], "system_prompt": "BASE"}));
        let ext = vec![
            (
                "personal".to_string(),
                PromptSection {
                    name: "job".into(),
                    text: "JOB RULE".into(),
                    requires: vec!["mcp_call".into()],
                    generate: None,
                },
            ),
            (
                "personal".to_string(),
                PromptSection {
                    name: "x".into(),
                    text: "NEEDS WEB".into(),
                    requires: vec!["search_web".into()],
                    generate: None,
                },
            ),
        ];
        assert_eq!(
            render(&p, &reg(&["mcp_call", "search_web"]), &ext),
            "BASE\n\nJOB RULE"
        );
    }

    #[test]
    fn a_tool_whose_plugin_is_off_is_not_missing() {
        let p = profile(json!({ "tools": ["search_web", "get_git_log", "gone_tool"] }));
        let off: HashMap<String, String> =
            [("get_git_log".to_string(), "developer".to_string())].into();
        let a = audit(&p, &["search_web".into()], &HashMap::new(), &[], &off);
        assert_eq!(a.missing_tools, ["gone_tool"]);
        assert_eq!(a.plugin_off, ["get_git_log (plugin developer is off)"]);
    }

    #[test]
    fn audit_names_missing_tools_and_prose_about_removed_ones() {
        let p = profile(json!({
            "tools": ["search_web", "list_notes"],
            "system_prompt": "Use list_notes, not list_notes_v2; pass `context`: x; call context(x).",
        }));
        let removed: HashMap<String, Option<String>> = [
            ("list_notes".to_string(), Some("search_notes".to_string())),
            ("context".to_string(), None),
            ("fetch_transcript".to_string(), None),
        ]
        .into();
        let a = audit(&p, &["search_web".into()], &removed, &[], &HashMap::new());
        assert_eq!(a.missing_tools, ["list_notes (removed; use search_notes)"]);
        assert_eq!(
            a.stale_mentions,
            [
                "context (removed)",
                "list_notes (removed; use search_notes)"
            ]
        );
    }

    #[test]
    fn a_save_keeps_what_the_form_does_not_send() {
        let stored = profile(json!({
            "tools": ["a"], "system_prompt": "old", "explicit_only": true,
            "data_class": "public", "sections": [{"name": "s", "text": "kept"}]
        }));
        let saved = overlay(Some(&stored), &json!({"name": "p", "system_prompt": "new"})).unwrap();
        assert_eq!(saved.system_prompt, "new");
        assert!(
            saved.explicit_only,
            "a form that never sends explicit_only must not clear it"
        );
        assert_eq!(saved.sections.len(), 1);
        assert_eq!(saved.tools, ["a"]);
    }

    #[test]
    fn the_shipped_profiles_and_their_sections_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/contexts");
        let b = BundleProfiles::read(&dir, &[]);
        assert!(b.profiles.len() >= 15, "{} profiles", b.profiles.len());
        for (_, path, p) in &b.profiles {
            for s in &p.sections {
                assert!(
                    s.generate.is_none() || s.generate.as_deref() == Some("tools"),
                    "{}: unknown generate in section {}",
                    path.display(),
                    s.name
                );
            }
        }
    }

    #[test]
    fn names_word_matches_whole_identifiers_only() {
        assert!(names_word("call list_notes now", "list_notes"));
        assert!(!names_word("call list_notes_v2", "list_notes"));
        assert!(!names_word("contextual", "context"));
        assert!(names_word("(context)", "context"));
        assert!(!names_tool("pass context: x", "context"));
        assert!(!names_tool("needs `context` naming a step", "context"));
        assert!(names_tool("call context(x)", "context"));
    }
}
