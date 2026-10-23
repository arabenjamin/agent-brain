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

use crate::context_builder::ContextProfile;
use agent_brain_repository::Neo4jClient;

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
    /// A regex (case-insensitive): the section is left out of a turn whose
    /// message (an agent run: its task) does not match, so text that serves
    /// one kind of turn is not paid on every other (HARNESS2_PLAN Phase 4).
    /// A render with no turn in hand (a preview) keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
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
    /// What the validator refuses on save, found in a stored profile (one
    /// seeded from a file, or saved before the validator existed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// The validator's advice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
            agent_brain_graph::bundles::CORE.to_string(),
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
                    if plugin != agent_brain_graph::bundles::CORE
                        || first_plugin != agent_brain_graph::bundles::CORE
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
pub fn source_of(path: &Path) -> String {
    let base = agent_brain_graph::bundles::Plugins::global().dir.clone();
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
    render_for(profile, registry, extensions, None)
}

/// Whether a section with this `when` belongs in a turn: no `when`, no turn
/// in hand, or the turn matches. A pattern that does not compile keeps the
/// section (the validator refuses it on save).
pub fn section_applies(when: Option<&str>, turn: Option<&str>) -> bool {
    let (Some(pat), Some(turn)) = (when.map(str::trim).filter(|w| !w.is_empty()), turn) else {
        return true;
    };
    regex::RegexBuilder::new(pat)
        .case_insensitive(true)
        .build()
        .map_or(true, |re| re.is_match(turn))
}

/// [`render`] for one turn: a section whose `when` the turn does not match is
/// left out.
pub fn render_for(
    profile: &ContextProfile,
    registry: &[(String, String)],
    extensions: &[(String, PromptSection)],
    turn: Option<&str>,
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
        if !s.requires.iter().all(|t| available.contains(t))
            || !section_applies(s.when.as_deref(), turn)
        {
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

// ── The profile validator: a profile save is a form ──────────────────────

/// The field names serde reads for a struct `T`, asked of serde itself so the
/// list cannot drift from the struct: a probe deserializer records the
/// `fields` serde hands `deserialize_struct`, then stops.
pub fn struct_fields<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    use serde::de::{Error as _, Visitor};
    #[derive(Debug)]
    struct Stop;
    impl std::fmt::Display for Stop {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("field probe")
        }
    }
    impl std::error::Error for Stop {}
    impl serde::de::Error for Stop {
        fn custom<M: std::fmt::Display>(_: M) -> Self {
            Stop
        }
    }
    struct Probe<'a>(&'a mut Option<&'static [&'static str]>);
    impl<'de> serde::Deserializer<'de> for Probe<'_> {
        type Error = Stop;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop::custom("not a struct"))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Stop> {
            *self.0 = Some(fields);
            Err(Stop)
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
    let mut out = None;
    let _ = T::deserialize(Probe(&mut out));
    out.unwrap_or(&[])
}

/// What a profile is checked against. An empty list disables that check
/// (fail-open on our own wiring, as for workflows).
pub struct ProfileChecks<'a> {
    /// Live tool names.
    pub registry: &'a [String],
    /// Tools whose plugin is off: not missing, they return with the plugin.
    pub plugin_off: &'a HashMap<String, String>,
    pub catalog: &'a [agent_brain_llm::model_router::CatalogModel],
    /// Every stored profile, for the router's targets.
    pub profiles: &'a [ContextProfile],
}

/// Refused on save (`hard`) and advice (`soft`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProfileFindings {
    pub hard: Vec<String>,
    pub soft: Vec<String>,
}

/// A local small-model prompt is a size budget (CLAUDE.md): past this the
/// local E4B was measured to stop delegating.
const LOCAL_PROMPT_BUDGET: usize = 3_000;

/// Check one profile. `body`, when given, is what a save sent: its keys are
/// checked against the profile's fields (serde drops an unknown one, so a
/// typo such as `spawnabel` saved as if it were not there) and its tools
/// against the registry. Without a body (the startup audit of stored
/// profiles) tools are left to [`audit`], which already reports them.
pub fn validate(body: Option<&Value>, p: &ContextProfile, c: &ProfileChecks) -> ProfileFindings {
    use agent_brain_llm::model_router::find_model;
    let mut f = ProfileFindings::default();

    if let Some(obj) = body.and_then(Value::as_object) {
        let known: Vec<String> = struct_fields::<ContextProfile>()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for k in obj.keys().filter(|k| !known.contains(k)) {
            let hint = crate::validation::closest(k, &known)
                .map(|m| format!(" (did you mean `{m}`?)"))
                .unwrap_or_default();
            f.hard.push(format!("`{k}` is not a profile field{hint}"));
        }
        if !c.registry.is_empty() {
            for t in &p.tools {
                if !c.registry.contains(t) && !c.plugin_off.contains_key(t) {
                    f.hard.push(format!("tool `{t}` does not exist"));
                }
            }
        }
    }

    let n = &p.name;
    let ok_name = n.len() <= 64
        && n.chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        && n.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
    if !ok_name {
        f.hard.push(format!(
            "name `{n}` must be letters, digits and '-', start with a letter or digit, at most 64 characters"
        ));
    }

    let pref = p
        .model_preference
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let hint = p
        .provider_hint
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());
    if !c.catalog.is_empty() {
        if let Some(m) = pref
            && find_model(c.catalog, m).is_none()
        {
            match hint {
                None => f.hard.push(format!(
                    "model_preference `{m}` is not in the model catalog (models.yaml); with no \
                     provider_hint it can never resolve, and the profile would run on the chat default"
                )),
                Some(h) => f.soft.push(format!(
                    "model_preference `{m}` is not in the model catalog; it runs only if provider `{h}` serves it"
                )),
            }
        }
        for m in &p.fallback_models {
            if find_model(c.catalog, m).is_none() {
                f.hard.push(format!(
                    "fallback model `{m}` is not in the model catalog; that rung would be skipped"
                ));
            }
        }
    }

    if let Some(s) = p.side_effects.as_deref().map(str::trim)
        && !matches!(s, "read_only" | "writes")
    {
        f.hard.push(format!(
            "side_effects `{s}` must be read_only or writes (anything else asks first, every time)"
        ));
    }

    if let Some(r) = &p.routing {
        for (role, target) in [("stored", &r.stored), ("web", &r.web)] {
            match c.profiles.iter().find(|q| q.name == *target) {
                None if !c.profiles.is_empty() => {
                    f.hard.push(format!("routing.{role} names `{target}`, which is not a profile"))
                }
                Some(q) if !q.spawnable => f.hard.push(format!(
                    "routing.{role} names `{target}`, which cannot run as a sub-agent (spawnable: false)"
                )),
                _ => {}
            }
        }
        for ch in &r.choices {
            if let Some(tool) = ch.name.strip_prefix("self:")
                && !p.tools.iter().any(|t| t == tool)
            {
                f.hard.push(format!(
                    "routing choice `{}` names a tool this profile does not hold",
                    ch.name
                ));
            }
        }
        let mut patterns = vec![("routing.web_words".to_string(), r.web_words.clone())];
        for b in &r.behaviors {
            patterns.push((format!("behavior `{}`.when", b.name), b.when.clone()));
            patterns.push((format!("behavior `{}`.unless", b.name), b.unless.clone()));
        }
        for (what, pat) in patterns.into_iter().filter(|(_, p)| !p.trim().is_empty()) {
            if let Err(e) = regex::RegexBuilder::new(&pat)
                .case_insensitive(true)
                .build()
            {
                f.hard.push(format!("{what} is not a valid regex: {e}"));
            }
        }
    }

    for s in &p.sections {
        if let Some(w) = s.when.as_deref().filter(|w| !w.trim().is_empty())
            && let Err(e) = regex::RegexBuilder::new(w).case_insensitive(true).build()
        {
            f.hard.push(format!(
                "section `{}`.when is not a valid regex: {e}",
                s.name
            ));
        }
    }

    // A sub-agent's instructions are its prompt; `description` is what the
    // roster says about it. On 2026-10-23 a drafted bundle put a composer's
    // whole instructions in `description` and gave a reflector the prompt
    // "(same as description)": both would have run with no instructions.
    let prompt_chars = p
        .system_prompt
        .split_whitespace()
        .map(str::len)
        .sum::<usize>()
        + p.sections
            .iter()
            .map(|s| s.text.trim().len())
            .sum::<usize>();
    if p.spawnable && prompt_chars < 40 {
        let msg = if prompt_chars == 0 {
            "spawnable with no system_prompt: as a sub-agent it runs with no instructions (`description` is the roster's line, never the agent's prompt)".to_string()
        } else {
            format!(
                "spawnable with a {prompt_chars}-character system_prompt: as a sub-agent that is all it is told (a placeholder such as \"(same as description)\" is not a prompt)"
            )
        };
        if body.is_some() {
            f.hard.push(msg);
        } else {
            f.soft.push(msg);
        }
    }

    if p.spawnable && p.specialty.as_deref().is_none_or(|s| s.trim().is_empty()) {
        f.soft.push(
            "spawnable with no specialty: the delegate roster describes it by its description"
                .into(),
        );
    }
    let local = pref
        .and_then(|m| find_model(c.catalog, m))
        .is_some_and(|row| matches!(row.provider.as_str(), "llamacpp" | "ollama"));
    if local {
        let size = p.system_prompt.len() + p.sections.iter().map(|s| s.text.len()).sum::<usize>();
        if size > LOCAL_PROMPT_BUDGET {
            f.soft.push(format!(
                "pinned to a local model with a {size}-character prompt: a local small-model prompt \
                 is a size budget (keep it near 2-3k characters)"
            ));
        }
    }
    f
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
        ..Default::default()
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
    fn a_section_with_when_renders_only_for_a_matching_turn() {
        let p = profile(json!({
            "tools": ["search_web"],
            "system_prompt": "BASE",
            "sections": [{"name": "link", "text": "LINK RULE", "when": "https?://"}]
        }));
        let r = reg(&["search_web"]);
        assert_eq!(
            render_for(&p, &r, &[], Some("watch HTTPS://youtu.be/x")),
            "BASE\n\nLINK RULE"
        );
        assert_eq!(
            render_for(&p, &r, &[], Some("what did we learn today?")),
            "BASE"
        );
        // A preview (no turn) shows everything the profile can say.
        assert_eq!(render(&p, &r, &[]), "BASE\n\nLINK RULE");
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
                    when: None,
                },
            ),
            (
                "personal".to_string(),
                PromptSection {
                    name: "x".into(),
                    text: "NEEDS WEB".into(),
                    requires: vec!["search_web".into()],
                    generate: None,
                    when: None,
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

    fn catalog() -> Vec<agent_brain_llm::model_router::CatalogModel> {
        use agent_brain_llm::model_config::ModelCatalog;
        let c =
            ModelCatalog::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models.yaml"))
                .expect("models.yaml parses");
        c.models
            .iter()
            .map(|(k, m)| agent_brain_llm::model_router::CatalogModel {
                name: k.clone(),
                provider: m.provider.clone(),
                model: m.model.clone(),
            })
            .collect()
    }

    fn checks<'a>(
        registry: &'a [String],
        off: &'a HashMap<String, String>,
        cat: &'a [agent_brain_llm::model_router::CatalogModel],
        profiles: &'a [ContextProfile],
    ) -> ProfileChecks<'a> {
        ProfileChecks {
            registry,
            plugin_off: off,
            catalog: cat,
            profiles,
        }
    }

    #[test]
    fn struct_fields_come_from_serde() {
        let f = struct_fields::<ContextProfile>();
        for k in [
            "name",
            "tools",
            "spawnable",
            "routing",
            "explicit_only",
            "fallback_models",
        ] {
            assert!(f.contains(&k), "{k} missing from {f:?}");
        }
    }

    /// The silent drop on save: a misspelled key saved as if it were not there.
    #[test]
    fn an_unknown_key_and_a_missing_tool_are_refused_on_save() {
        let body = json!({"name": "p", "description": "d", "spawnabel": true, "tools": ["search_notes", "no_such_tool"]});
        let p = overlay(None, &body).unwrap();
        let reg = vec!["search_notes".to_string()];
        let off = HashMap::new();
        let f = validate(Some(&body), &p, &checks(&reg, &off, &[], &[]));
        assert!(
            f.hard
                .iter()
                .any(|m| m.contains("`spawnabel`") && m.contains("did you mean `spawnable`")),
            "{f:?}"
        );
        assert!(
            f.hard
                .iter()
                .any(|m| m.contains("tool `no_such_tool` does not exist")),
            "{f:?}"
        );
    }

    #[test]
    fn a_spawnable_profile_needs_a_prompt_of_its_own() {
        let off = HashMap::new();
        let body = json!({"name": "c", "description": "Compares three runs and decides.", "spawnable": true, "specialty": "x"});
        let p = overlay(None, &body).unwrap();
        let f = validate(Some(&body), &p, &checks(&[], &off, &[], &[]));
        assert!(
            f.hard.iter().any(|m| m.contains("no system_prompt")),
            "{f:?}"
        );
        let body = json!({"name": "r", "description": "d", "spawnable": true, "specialty": "x", "system_prompt": "(same as description)"});
        let p = overlay(None, &body).unwrap();
        let f = validate(Some(&body), &p, &checks(&[], &off, &[], &[]));
        assert!(f.hard.iter().any(|m| m.contains("placeholder")), "{f:?}");
        // The startup audit of a stored profile only warns.
        let f = validate(None, &p, &checks(&[], &off, &[], &[]));
        assert!(f.hard.is_empty() && f.soft.iter().any(|m| m.contains("placeholder")));
        // A section's `when` must compile.
        let body = json!({"name": "s", "description": "d", "system_prompt": "Do the job well and report it.",
                          "sections": [{"name": "x", "text": "y", "when": "(open"}]});
        let p = overlay(None, &body).unwrap();
        let f = validate(Some(&body), &p, &checks(&[], &off, &[], &[]));
        assert!(f.hard.iter().any(|m| m.contains("`x`.when")), "{f:?}");
    }

    #[test]
    fn a_plugin_off_tool_is_not_missing() {
        let body = json!({"name": "p", "description": "d", "tools": ["ingest_media"]});
        let p = overlay(None, &body).unwrap();
        let reg = vec!["search_notes".to_string()];
        let off = HashMap::from([("ingest_media".to_string(), "media".to_string())]);
        assert!(
            validate(Some(&body), &p, &checks(&reg, &off, &[], &[]))
                .hard
                .is_empty()
        );
    }

    #[test]
    fn names_models_side_effects_and_router_targets_are_checked() {
        let cat = catalog();
        let off = HashMap::new();
        let worker = profile(json!({"name": "w", "spawnable": true, "specialty": "x"}));
        let persona = profile(json!({"name": "persona"}));
        let p = profile(json!({
            "name": "../bad name",
            "model_preference": "gpt-9000",
            "fallback_models": ["also-not-a-model"],
            "side_effects": "sometimes",
            "routing": {"rules": "r", "stored": "w", "web": "persona", "web_words": "(unclosed",
                        "choices": [{"name": "self:manage_todo", "says": "x"}]}
        }));
        let all = vec![worker, persona, p.clone()];
        let f = validate(None, &p, &checks(&[], &off, &cat, &all));
        let has = |s: &str| f.hard.iter().any(|m| m.contains(s));
        assert!(has("name `../bad name`"), "{f:?}");
        assert!(has("model_preference `gpt-9000`"), "{f:?}");
        assert!(has("fallback model `also-not-a-model`"), "{f:?}");
        assert!(has("side_effects `sometimes`"), "{f:?}");
        assert!(
            has("routing.web names `persona`, which cannot run as a sub-agent"),
            "{f:?}"
        );
        assert!(!has("routing.stored"), "{f:?}");
        assert!(has("routing choice `self:manage_todo`"), "{f:?}");
        assert!(has("routing.web_words is not a valid regex"), "{f:?}");
    }

    #[test]
    fn a_model_outside_the_catalog_with_a_provider_hint_is_advice() {
        let cat = catalog();
        let off = HashMap::new();
        let p =
            profile(json!({"model_preference": "some-ollama-model:7b", "provider_hint": "ollama"}));
        let f = validate(None, &p, &checks(&[], &off, &cat, &[]));
        assert!(f.hard.is_empty(), "{f:?}");
        assert!(
            f.soft
                .iter()
                .any(|m| m.contains("only if provider `ollama` serves it")),
            "{f:?}"
        );
    }

    #[test]
    fn a_spawnable_without_specialty_and_a_big_local_prompt_are_advice() {
        let cat = catalog();
        let local = cat
            .iter()
            .find(|m| m.provider == "llamacpp")
            .map(|m| m.name.clone())
            .expect("a llamacpp model in models.yaml");
        let off = HashMap::new();
        let p = profile(
            json!({"spawnable": true, "model_preference": local, "system_prompt": "x".repeat(4000)}),
        );
        let f = validate(None, &p, &checks(&[], &off, &cat, &[]));
        assert!(f.hard.is_empty(), "{f:?}");
        assert!(f.soft.iter().any(|m| m.contains("no specialty")), "{f:?}");
        assert!(
            f.soft.iter().any(|m| m.contains("4000-character prompt")),
            "{f:?}"
        );
    }

    /// Every profile we ship passes the validator: what the startup audit
    /// reports as a problem is never our own file.
    #[test]
    fn every_shipped_profile_passes_the_validator() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let dirs: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|d| d.join("contexts").is_dir() && !d.ends_with("core"))
            .map(|d| {
                (
                    d.file_name().unwrap().to_string_lossy().into_owned(),
                    d.join("contexts"),
                )
            })
            .collect();
        let b = BundleProfiles::read(&root.join("core/contexts"), &dirs);
        let all: Vec<ContextProfile> = b.profiles.iter().map(|(_, _, p)| p.clone()).collect();
        let cat = catalog();
        let off = HashMap::new();
        for p in &all {
            let f = validate(None, p, &checks(&[], &off, &cat, &all));
            assert!(f.hard.is_empty(), "{}: {:?}", p.name, f.hard);
        }
        assert!(all.len() >= 10, "{} profiles", all.len());
    }

    #[test]
    fn the_shipped_profiles_and_their_sections_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/contexts");
        let b = BundleProfiles::read(&dir, &[]);
        // 8 core profiles since the 2026-10-16 consolidation.
        assert!(b.profiles.len() >= 8, "{} profiles", b.profiles.len());
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
