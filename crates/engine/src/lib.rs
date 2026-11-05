//! The local engine: which server runs the brain's local models, what it can do,
//! and which model has each job.
//!
//! An engine is an entry in `engines:` in `models.yaml`, picked by `LOCAL_ENGINE`.
//! Everything talks to it over the OpenAI wire, so what differs between engines is
//! declared there, not branched on in code (`project-docs/LOCAL_ENGINE_PLAN.md` §4).
//!
//! A missing capability or an `off` model turns one feature off, and [`features`]
//! says which and why. Chat and embeddings are the exceptions: every local step
//! needs the first and retrieval needs the second, so [`require`] refuses to start
//! without them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

/// What an engine's OpenAI wire can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `/v1/chat/completions`.
    Chat,
    /// Tool calls on chat, which an agent loop needs.
    Tools,
    /// `response_format: json_schema` on chat.
    JsonSchema,
    /// `/v1/embeddings`.
    Embeddings,
    /// `logprobs` on chat.
    Logprobs,
    /// `/v1/completions` with a prompt sent exactly as written.
    Completions,
}

impl Capability {
    fn parse(word: &str) -> Option<Self> {
        serde_yaml::from_str(word.trim()).ok()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Tools => "tools",
            Self::JsonSchema => "json_schema",
            Self::Embeddings => "embeddings",
            Self::Logprobs => "logprobs",
            Self::Completions => "completions",
        }
    }
}

/// How to free the engine's memory when the GPU is leased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unload {
    /// The engine has no such call; its owner stops it.
    #[default]
    None,
    /// llama-server's router: `GET /models`, `POST /models/unload`.
    Router,
}

/// A job a local model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Background steps, the router's picker, the last rung of a ladder.
    Generation,
    Embeddings,
    /// A fixed choice with probabilities (`agent_brain_decision`).
    Decision,
    /// Whether a claim is supported by its cited results.
    Grounding,
}

impl Role {
    pub const ALL: [Role; 4] = [
        Role::Generation,
        Role::Embeddings,
        Role::Decision,
        Role::Grounding,
    ];

    /// The env var naming this role's model; `off` turns an optional role off.
    pub fn env(self) -> &'static str {
        match self {
            Self::Generation => "LOCAL_MODEL",
            Self::Embeddings => "LOCAL_EMBED_MODEL",
            Self::Decision => "LOCAL_DECISION_MODEL",
            Self::Grounding => "LOCAL_GROUNDING_MODEL",
        }
    }

    /// The capability the role cannot work without.
    pub fn needs(self) -> Capability {
        match self {
            Self::Generation => Capability::Chat,
            Self::Embeddings => Capability::Embeddings,
            Self::Decision => Capability::Logprobs,
            Self::Grounding => Capability::Completions,
        }
    }

    fn required(self) -> bool {
        matches!(self, Self::Generation | Self::Embeddings)
    }

    fn feature(self) -> &'static str {
        match self {
            Self::Generation => "local generation",
            Self::Embeddings => "embeddings",
            Self::Decision => "decisions (alias judge, API tags, router checker)",
            Self::Grounding => "claim grounding check",
        }
    }
}

/// The models an entry names by default, by role. A section of a router's preset,
/// or nothing for an engine that brings its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleModels {
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub embeddings: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub grounding: Option<String>,
}

impl RoleModels {
    fn get(&self, role: Role) -> Option<&String> {
        match role {
            Role::Generation => self.generation.as_ref(),
            Role::Embeddings => self.embeddings.as_ref(),
            Role::Decision => self.decision.as_ref(),
            Role::Grounding => self.grounding.as_ref(),
        }
    }
}

/// One entry of `engines:` in `models.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineEntry {
    /// The endpoint when `LOCAL_ENGINE_URL` is unset.
    #[serde(default)]
    pub base_url_default: Option<String>,
    #[serde(default)]
    pub unload: Unload,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub models: RoleModels,
}

/// The engine in use: its entry with the environment applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    pub name: String,
    pub url: String,
    pub unload: Unload,
    pub capabilities: BTreeSet<Capability>,
    /// Each role's model, or why it has none.
    roles: BTreeMap<Role, Result<String, String>>,
}

/// One feature and whether it is on, for `/health`, the system report and the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Feature {
    pub feature: String,
    pub on: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl Engine {
    /// `entry` as `name`, reading the environment through `env`. `Err` when it
    /// cannot run at all: no endpoint, or a capability list that does not parse.
    pub fn resolve(
        name: &str,
        entry: &EngineEntry,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let read = |key: &str| {
            env(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let url = read("LOCAL_ENGINE_URL")
            .or_else(|| entry.base_url_default.clone())
            .ok_or_else(|| format!("engine '{name}' needs LOCAL_ENGINE_URL"))?
            .trim_end_matches('/')
            .to_string();
        let capabilities: BTreeSet<Capability> = match read("LOCAL_ENGINE_CAPABILITIES") {
            Some(list) => list
                .split(',')
                .map(|w| {
                    Capability::parse(w).ok_or_else(|| {
                        format!(
                            "LOCAL_ENGINE_CAPABILITIES: '{}' is not a capability",
                            w.trim()
                        )
                    })
                })
                .collect::<Result<_, _>>()?,
            None => entry.capabilities.iter().copied().collect(),
        };
        let roles = Role::ALL
            .into_iter()
            .map(|role| {
                let chosen = read(role.env()).or_else(|| role_default(entry, role));
                let model = match chosen {
                    Some(m) if m.eq_ignore_ascii_case("off") => Err(format!("{}=off", role.env())),
                    Some(_) if !capabilities.contains(&role.needs()) => Err(format!(
                        "engine '{name}' does not declare {}",
                        role.needs().name()
                    )),
                    Some(m) => Ok(m),
                    None => Err(format!(
                        "{} is unset and engine '{name}' names no default",
                        role.env()
                    )),
                };
                (role, model)
            })
            .collect();
        Ok(Self {
            name: name.to_string(),
            url,
            unload: entry.unload,
            capabilities,
            roles,
        })
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The role's model, or `None` when the role is off.
    pub fn model(&self, role: Role) -> Option<&str> {
        self.roles.get(&role)?.as_deref().ok()
    }

    /// `Err` naming what stops the brain running on this engine.
    pub fn check(&self) -> Result<(), String> {
        for role in Role::ALL.into_iter().filter(|r| r.required()) {
            if let Some(Err(why)) = self.roles.get(&role) {
                return Err(format!(
                    "the brain cannot start without {}: {why}",
                    role.feature()
                ));
            }
        }
        Ok(())
    }

    /// Every feature that depends on the engine, on or off.
    pub fn features(&self) -> Vec<Feature> {
        let mut out: Vec<Feature> = Role::ALL
            .into_iter()
            .map(|role| match self.roles.get(&role) {
                Some(Ok(m)) => Feature {
                    feature: role.feature().to_string(),
                    on: true,
                    model: Some(m.clone()),
                    why: None,
                },
                Some(Err(why)) => off(role.feature(), why.clone()),
                None => off(role.feature(), "no such role".to_string()),
            })
            .collect();
        for (feature, needs) in [
            ("agent tools on the local model", Capability::Tools),
            ("chat router picker", Capability::JsonSchema),
        ] {
            out.push(if self.has(needs) {
                Feature {
                    feature: feature.to_string(),
                    on: true,
                    model: None,
                    why: None,
                }
            } else {
                off(
                    feature,
                    format!("engine '{}' does not declare {}", self.name, needs.name()),
                )
            });
        }
        out
    }

    /// The engine as `/health` and the system report show it: no endpoint, because
    /// `/health` needs no key.
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": self.name,
            "capabilities": self.capabilities.iter().map(|c| c.name()).collect::<Vec<_>>(),
            "features": self.features(),
        })
    }
}

fn off(feature: &str, why: String) -> Feature {
    Feature {
        feature: feature.to_string(),
        on: false,
        model: None,
        why: Some(why),
    }
}

fn role_default(entry: &EngineEntry, role: Role) -> Option<String> {
    entry.models.get(role).cloned()
}

/// The entry a process without `models.yaml` runs on, as unit tests do. Deployments
/// declare theirs in `models.yaml`, which replaces this table.
const BUILTIN: &str = r#"
llamacpp:
  base_url_default: "http://localhost:8080"
  unload: router
  capabilities: [chat, tools, json_schema, embeddings, logprobs, completions]
  models: { generation: gemma4-e4b, embeddings: bge-m3, decision: nimble, grounding: guardian }
"#;

/// The engine `LOCAL_ENGINE` names when it is unset.
const DEFAULT_ENGINE: &str = "llamacpp";

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn chosen(entries: &BTreeMap<String, EngineEntry>) -> Result<Engine, String> {
    let name = process_env("LOCAL_ENGINE")
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_ENGINE.to_string());
    let entry = entries.get(&name).ok_or_else(|| {
        format!(
            "LOCAL_ENGINE '{name}' is not in `engines:` ({})",
            entries.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;
    Engine::resolve(&name, entry, process_env)
}

fn builtin() -> BTreeMap<String, EngineEntry> {
    serde_yaml::from_str(BUILTIN).expect("built-in engine table must parse")
}

struct Slot {
    engine: Arc<Engine>,
    /// Why the last install failed; the engine above is then the previous one.
    error: Option<String>,
}

fn slot() -> &'static RwLock<Slot> {
    static SLOT: OnceLock<RwLock<Slot>> = OnceLock::new();
    SLOT.get_or_init(|| {
        let table = builtin();
        let (engine, error) = match chosen(&table) {
            Ok(e) => (e, None),
            Err(e) => (fallback(&table), Some(e)),
        };
        RwLock::new(Slot {
            engine: Arc::new(engine),
            error,
        })
    })
}

/// The built-in entry with nothing from the environment, to hold the place of an
/// engine that failed to resolve until [`require`] reports it.
fn fallback(table: &BTreeMap<String, EngineEntry>) -> Engine {
    Engine::resolve(DEFAULT_ENGINE, &table[DEFAULT_ENGINE], |_| None)
        .expect("the built-in engine resolves")
}

/// Install `engines:` from `models.yaml`, at startup and on `reload_models`. An
/// empty section keeps the built-in table.
pub fn install(entries: &BTreeMap<String, EngineEntry>) {
    let table = if entries.is_empty() {
        builtin()
    } else {
        entries.clone()
    };
    let mut slot = slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match chosen(&table) {
        Ok(engine) => {
            slot.engine = Arc::new(engine);
            slot.error = None;
        }
        Err(e) => {
            tracing::error!(error = %e, "local engine not installed");
            slot.error = Some(e);
        }
    }
}

/// The engine in use.
pub fn current() -> Arc<Engine> {
    slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .engine
        .clone()
}

/// The role's model on the current engine, or `None` when the role is off.
pub fn model(role: Role) -> Option<String> {
    current().model(role).map(str::to_string)
}

/// `Err` when the brain must not start: the engine did not resolve, or it lacks
/// chat or embeddings. Logs every feature that is off.
pub fn require() -> Result<Arc<Engine>, String> {
    if let Some(e) = &slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .error
    {
        return Err(e.clone());
    }
    let engine = current();
    engine.check()?;
    // install.sh records its choice; `.env` drifting away from it should be seen.
    if let Some(recorded) = process_env("LOCAL_ENGINE_INSTALLED")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && *v != engine.name)
    {
        tracing::warn!(
            engine = %engine.name,
            installed = %recorded,
            "LOCAL_ENGINE differs from what install.sh set up; the models and compose \
             files it chose may not match"
        );
    }
    for f in engine.features().iter().filter(|f| !f.on) {
        tracing::warn!(
            feature = %f.feature,
            why = f.why.as_deref().unwrap_or(""),
            "local engine: feature off"
        );
    }
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml: &str) -> EngineEntry {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn resolve(e: &EngineEntry, vars: &[(&str, &str)]) -> Result<Engine, String> {
        Engine::resolve("t", e, |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    fn full() -> EngineEntry {
        entry(
            "base_url_default: http://e:1/\nunload: router\n\
             capabilities: [chat, tools, json_schema, embeddings, logprobs, completions]\n\
             models: {generation: g, embeddings: b, decision: n, grounding: h}",
        )
    }

    #[test]
    fn an_entry_gives_its_endpoint_and_models() {
        let e = resolve(&full(), &[]).unwrap();
        assert_eq!(e.url, "http://e:1");
        assert_eq!(e.model(Role::Decision), Some("n"));
        assert!(e.check().is_ok());
        assert!(e.features().iter().all(|f| f.on));
    }

    #[test]
    fn the_environment_wins_over_the_entry() {
        let e = resolve(
            &full(),
            &[("LOCAL_ENGINE_URL", "http://other:2"), ("LOCAL_MODEL", "x")],
        )
        .unwrap();
        assert_eq!(e.url, "http://other:2");
        assert_eq!(e.model(Role::Generation), Some("x"));
    }

    /// Compose passes `LOCAL_MODEL=${LOCAL_MODEL:-}`, so unset arrives as empty.
    #[test]
    fn an_empty_variable_counts_as_unset() {
        let e = resolve(&full(), &[("LOCAL_DECISION_MODEL", " ")]).unwrap();
        assert_eq!(e.model(Role::Decision), Some("n"));
    }

    #[test]
    fn off_turns_one_role_off_and_says_so() {
        let e = resolve(&full(), &[("LOCAL_GROUNDING_MODEL", "OFF")]).unwrap();
        assert_eq!(e.model(Role::Grounding), None);
        assert!(e.check().is_ok(), "grounding is optional");
        let f = e
            .features()
            .into_iter()
            .find(|f| f.feature.contains("grounding"))
            .unwrap();
        assert!(!f.on);
        assert_eq!(f.why.as_deref(), Some("LOCAL_GROUNDING_MODEL=off"));
    }

    #[test]
    fn a_missing_capability_turns_its_feature_off_and_names_it() {
        let e = resolve(
            &full(),
            &[("LOCAL_ENGINE_CAPABILITIES", "chat, tools, embeddings")],
        )
        .unwrap();
        assert_eq!(e.model(Role::Decision), None);
        assert_eq!(e.model(Role::Grounding), None);
        assert!(e.check().is_ok());
        let off: Vec<_> = e.features().into_iter().filter(|f| !f.on).collect();
        assert_eq!(off.len(), 3, "{off:?}");
        assert!(off[0].why.as_deref().unwrap().contains("logprobs"));
        assert!(off[2].feature.contains("picker"));
    }

    #[test]
    fn no_embeddings_refuses_to_start() {
        let e = resolve(
            &full(),
            &[("LOCAL_ENGINE_CAPABILITIES", "chat,tools,logprobs")],
        )
        .unwrap();
        let why = e.check().unwrap_err();
        assert!(why.contains("embeddings"), "{why}");
    }

    #[test]
    fn a_required_role_cannot_be_off() {
        let e = resolve(&full(), &[("LOCAL_EMBED_MODEL", "off")]).unwrap();
        assert!(e.check().unwrap_err().contains("LOCAL_EMBED_MODEL=off"));
        let e = resolve(&full(), &[("LOCAL_MODEL", "off")]).unwrap();
        assert!(e.check().unwrap_err().contains("LOCAL_MODEL=off"));
    }

    #[test]
    fn bring_your_own_needs_a_url_and_its_models() {
        let byo = entry("capabilities: [chat, tools, embeddings]");
        assert!(resolve(&byo, &[]).unwrap_err().contains("LOCAL_ENGINE_URL"));
        let e = resolve(&byo, &[("LOCAL_ENGINE_URL", "http://h:1")]).unwrap();
        assert!(e.check().unwrap_err().contains("LOCAL_MODEL is unset"));
        let e = resolve(
            &byo,
            &[
                ("LOCAL_ENGINE_URL", "http://h:1"),
                ("LOCAL_MODEL", "m"),
                ("LOCAL_EMBED_MODEL", "b"),
            ],
        )
        .unwrap();
        assert!(e.check().is_ok());
        assert_eq!(e.unload, Unload::None);
    }

    #[test]
    fn a_misspelled_capability_is_an_error_not_a_silent_loss() {
        let why = resolve(&full(), &[("LOCAL_ENGINE_CAPABILITIES", "chat,logprob")]).unwrap_err();
        assert!(why.contains("logprob"), "{why}");
    }

    #[test]
    fn the_report_never_carries_the_endpoint() {
        let e = resolve(&full(), &[("LOCAL_ENGINE_URL", "http://secret-host:2")]).unwrap();
        assert!(!e.report().to_string().contains("secret-host"));
    }

    #[test]
    fn the_builtin_table_parses_and_runs() {
        let t = builtin();
        let e = Engine::resolve(DEFAULT_ENGINE, &t[DEFAULT_ENGINE], |_| None).unwrap();
        assert!(e.check().is_ok());
    }
}
