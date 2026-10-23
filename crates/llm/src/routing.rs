//! Routing policy — Phase 2 of `project-docs/PROVIDER_ROUTING_PLAN.md`.
//!
//! One question, asked everywhere a model is about to be called: *may this work,
//! for this purpose, use this provider?* The answer comes from the provider's
//! registry declaration (enabled, key, billing, data policy, limits) and from
//! the work itself (purpose + data class) — never from a provider type
//! hardcoded at a call site.
//!
//! - [`RoutingPolicy::admit`] judges one ready config: a chat pin, a ladder rung,
//!   a sub-agent's model, a background fallback rung.
//! - [`route`] filters the capability-matched catalog for a routed step,
//!   keeping its order and recording why every skipped model was skipped.
//!
//! **Privacy before budget.** Work is `private` unless it says otherwise, and
//! private work only reaches `no_log` providers — not even as a last fallback —
//! unless the owner names the purpose in that provider's `private_ok_for`.
//! **Paid is off until opted in**: a paid provider needs its billing class in
//! the policy, the purpose in `ROUTING_PAID_FOR`, and a declared spend cap.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::llm::LlmConfig;
use crate::provider_registry::{self, Billing, DataPolicy, ProviderEntry};

/// What kind of caller is about to spend a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// A human `/chat` turn.
    Chat,
    /// A delegated agent loop (`delegate` / `run_agent`): many calls per run.
    Subagent,
    /// A queued job step, or anything not scoped as chat or sub-agent.
    Background,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Chat => "chat",
            Purpose::Subagent => "subagent",
            Purpose::Background => "background",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Purpose::Chat),
            "subagent" | "sub-agent" => Some(Purpose::Subagent),
            "background" => Some(Purpose::Background),
            _ => None,
        }
    }
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who may see the work's prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataClass {
    /// Only `no_log` providers. The default: work that does not say it is
    /// public is treated as private.
    #[default]
    Private,
    /// Any permitted provider, including free tiers that may train on prompts.
    Public,
}

impl DataClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DataClass::Private => "private",
            DataClass::Public => "public",
        }
    }

    /// Parse a stored label; anything unrecognised is private.
    pub fn parse_or_private(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("public") {
            DataClass::Public
        } else {
            DataClass::Private
        }
    }

    /// Private if either side is. Work handed from a private caller to a public
    /// agent stays private: the task text carries the caller's data.
    pub fn stricter(self, other: DataClass) -> DataClass {
        if self == DataClass::Private || other == DataClass::Private {
            DataClass::Private
        } else {
            DataClass::Public
        }
    }
}

impl std::fmt::Display for DataClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who started the work. A paid model runs only on work a person started, or
/// on a workflow the owner approved for paid models after reviewing its
/// results: the brain acting on its own never spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Initiator {
    /// A scheduled or brain-started job, an agent the brain delegated to, a
    /// direct MCP call from a tool or script. The default.
    #[default]
    Brain,
    /// A chat turn, or an HBI action (`x-brain-initiator: owner`).
    Owner,
    /// A queued step of a workflow marked `paid: true`.
    Approved,
}

impl Initiator {
    pub fn as_str(self) -> &'static str {
        match self {
            Initiator::Brain => "brain",
            Initiator::Owner => "owner",
            Initiator::Approved => "approved",
        }
    }
}

/// The purpose and data class of the work running on this task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteContext {
    pub purpose: Purpose,
    pub data_class: DataClass,
    pub initiator: Initiator,
}

tokio::task_local! {
    /// Scoped by the chat turn, `AgentRunner::run` and the queue coordinator, so
    /// every model call below them — including `SharedLlm` calls made by tools —
    /// is judged against the work that caused it.
    pub static ROUTE_CONTEXT: RouteContext;

    /// Set around an HTTP request the HBI sent for the owner
    /// (`x-brain-initiator: owner`). Read only as the initiator of an unscoped
    /// call, so a direct tool call keeps every other unscoped behaviour.
    pub static OWNER_REQUEST: bool;
}

impl RouteContext {
    /// A chat turn is the owner's; anything else starts as the brain's.
    pub fn new(purpose: Purpose, data_class: DataClass) -> Self {
        Self {
            purpose,
            data_class,
            initiator: if purpose == Purpose::Chat {
                Initiator::Owner
            } else {
                Initiator::Brain
            },
        }
    }

    pub fn with_initiator(mut self, initiator: Initiator) -> Self {
        self.initiator = initiator;
        self
    }

    /// The scoped context, or background/private when nothing scoped one (a
    /// direct MCP tool call, a startup protocol step) — the cautious reading.
    /// An unscoped call inside an owner's HBI request is the owner's.
    pub fn current() -> Self {
        ROUTE_CONTEXT.try_with(|c| *c).unwrap_or_else(|_| {
            let owner = OWNER_REQUEST.try_with(|o| *o).unwrap_or(false);
            Self::new(Purpose::Background, DataClass::Private).with_initiator(if owner {
                Initiator::Owner
            } else {
                Initiator::Brain
            })
        })
    }
}

/// Declared limits below which a provider cannot carry a multi-round agent loop.
/// A sub-agent makes one call per tool round (up to 8), so 5 requests/minute
/// (Gemini's free tier) stalls mid-run and 50 requests/day (OpenRouter `:free`)
/// is spent by a handful of runs. This guards the loop shape; the quota itself is
/// guarded by live budgets in `services/budget.rs`.
const LOOP_MIN_RPM: u32 = 20;
const LOOP_MIN_RPD: u32 = 500;

fn billing_str(b: Billing) -> &'static str {
    match b {
        Billing::Local => "local",
        Billing::Free => "free",
        Billing::Paid => "paid",
    }
}

fn policy_str(p: DataPolicy) -> &'static str {
    match p {
        DataPolicy::NoLog => "no_log",
        DataPolicy::MayLog => "may_log",
        DataPolicy::MayTrain => "may_train",
    }
}

/// Which billing classes may be used at all, and which purposes may spend money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicy {
    pub billing: Vec<Billing>,
    pub paid_for: Vec<Purpose>,
    /// Where `billing` came from (`ROUTING_BILLING` or `CLOUD_TIER=N`), for
    /// rejection reasons and `/api/models`.
    pub source: String,
    /// Set while a GPU lease is held (`services/gpu_lease.rs`): why every
    /// `billing: local` provider is refused.
    pub gpu_lease: Option<String>,
}

impl RoutingPolicy {
    /// Read the policy from the environment. Cheap; read per decision so an env
    /// change needs no cache invalidation.
    pub fn from_env() -> Self {
        let mut policy = Self::from_values(
            std::env::var("ROUTING_BILLING").ok().as_deref(),
            std::env::var("CLOUD_TIER").ok().as_deref(),
            std::env::var("ROUTING_PAID_FOR").ok().as_deref(),
        );
        policy.gpu_lease = crate::gpu_lease::refusal();
        policy
    }

    /// `ROUTING_BILLING` (comma list of `local,free,paid`) wins when non-empty.
    /// Otherwise the legacy `CLOUD_TIER` maps `0 → local`, `1 → local,free`
    /// (the default), `2 → local,free,paid`. `ROUTING_PAID_FOR` lists purposes
    /// allowed to spend; unset means none. Unknown words are ignored.
    pub fn from_values(
        routing_billing: Option<&str>,
        cloud_tier: Option<&str>,
        paid_for: Option<&str>,
    ) -> Self {
        let explicit = routing_billing.map(str::trim).filter(|s| !s.is_empty());
        let (billing, source) = match explicit {
            Some(list) => {
                let mut billing = Vec::new();
                for word in list.split(',') {
                    let b = match word.trim().to_ascii_lowercase().as_str() {
                        "local" => Billing::Local,
                        "free" => Billing::Free,
                        "paid" => Billing::Paid,
                        _ => continue,
                    };
                    if !billing.contains(&b) {
                        billing.push(b);
                    }
                }
                (billing, format!("ROUTING_BILLING={list}"))
            }
            None => {
                let tier = cloud_tier
                    .and_then(|v| v.trim().parse::<u8>().ok())
                    .map(|t| t.min(2))
                    .unwrap_or(1);
                let billing = match tier {
                    0 => vec![Billing::Local],
                    1 => vec![Billing::Local, Billing::Free],
                    _ => vec![Billing::Local, Billing::Free, Billing::Paid],
                };
                (billing, format!("CLOUD_TIER={tier}"))
            }
        };
        let mut purposes = Vec::new();
        for word in paid_for.unwrap_or("").split(',') {
            if let Some(p) = Purpose::parse(word)
                && !purposes.contains(&p)
            {
                purposes.push(p);
            }
        }
        Self {
            billing,
            paid_for: purposes,
            source,
            gpu_lease: None,
        }
    }

    /// Judge one provider for one piece of work. Filters run in the plan's
    /// order, and the first failure is the reason reported.
    pub fn check_entry(
        &self,
        name: &str,
        entry: Option<&ProviderEntry>,
        key_present: bool,
        ctx: RouteContext,
    ) -> Result<(), String> {
        let Some(e) = entry else {
            return Err(format!(
                "provider '{name}' is not in the provider registry, so its billing and data \
                 policy are unknown"
            ));
        };
        if !e.enabled {
            return Err(format!("provider '{name}' is disabled"));
        }
        if e.billing == Billing::Local
            && let Some(reason) = &self.gpu_lease
        {
            return Err(reason.clone());
        }
        if !key_present {
            return Err(format!(
                "{} is not set",
                e.api_key_env.as_deref().unwrap_or("its API key")
            ));
        }
        if !self.billing.contains(&e.billing) {
            return Err(format!(
                "billing '{}' is not allowed ({})",
                billing_str(e.billing),
                self.source
            ));
        }
        if e.billing == Billing::Paid {
            if !self.paid_for.contains(&ctx.purpose) {
                let list = self
                    .paid_for
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                return Err(format!(
                    "{} work may not spend money (ROUTING_PAID_FOR={})",
                    ctx.purpose,
                    if list.is_empty() { "unset" } else { &list }
                ));
            }
            if ctx.initiator == Initiator::Brain {
                return Err(format!(
                    "paid provider '{name}' runs only on work the owner started (chat, the \
                     HBI) or on a workflow marked `paid: true`; this is the brain's own {} work",
                    ctx.purpose
                ));
            }
            let capped = e
                .spend
                .as_ref()
                .is_some_and(|s| s.daily_usd.is_some() || s.monthly_usd.is_some());
            if !capped {
                return Err(format!("paid provider '{name}' declares no spend cap"));
            }
        }
        if ctx.data_class == DataClass::Private
            && e.data_policy != DataPolicy::NoLog
            && !e.private_ok_for.contains(&ctx.purpose)
        {
            return Err(format!(
                "private {} work may not use a {} provider (models.yaml `private_ok_for` \
                 does not name {})",
                ctx.purpose,
                policy_str(e.data_policy),
                ctx.purpose
            ));
        }
        loop_capacity(&e.limits, ctx)
    }

    /// [`Self::check_entry`] plus the model's own declared limits
    /// (`models.yaml` `limits:`), which the sub-agent loop floor must also clear:
    /// a model limited to 5 rpm cannot carry a loop even when its provider
    /// declares nothing.
    pub fn check_model(
        &self,
        name: &str,
        entry: Option<&ProviderEntry>,
        key_present: bool,
        model_limits: Option<&provider_registry::Limits>,
        ctx: RouteContext,
    ) -> Result<(), String> {
        self.check_entry(name, entry, key_present, ctx)?;
        match model_limits {
            Some(l) => loop_capacity(l, ctx),
            None => Ok(()),
        }
    }

    /// Judge a ready-to-call config against the live registry.
    pub fn admit(&self, cfg: &LlmConfig, ctx: RouteContext) -> Result<(), String> {
        let name = provider_registry::canonical_name(&cfg.provider_label());
        let entry = provider_registry::get(&name);
        // A key supplied some other way (CHAT_API_KEY, use_model) counts: the
        // question is whether this config can authenticate, not whether the
        // registry's env var happens to be the place it came from.
        let key_present = entry.as_ref().is_some_and(|e| {
            e.key_configured() || cfg.api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
        });
        let model_limits = provider_registry::model_limits(&name, &cfg.model);
        self.check_model(
            &name,
            entry.as_ref(),
            key_present,
            model_limits.as_ref(),
            ctx,
        )?;
        // Budget last: only a provider the policy would otherwise use is worth
        // judging for headroom (`services/budget.rs`).
        match entry.as_ref() {
            Some(e) => crate::budget::verdict(&name, &cfg.model, e),
            None => Ok(()),
        }
    }

    /// The policy as JSON for `/api/models`.
    pub fn describe(&self) -> Value {
        json!({
            "billing": self.billing.iter().map(|b| billing_str(*b)).collect::<Vec<_>>(),
            "paid_for": self.paid_for.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "source": self.source,
            "subagent_min_rpm": LOOP_MIN_RPM,
            "subagent_min_rpd": LOOP_MIN_RPD,
            "gpu_lease": self.gpu_lease,
        })
    }
}

/// A catalog model the policy admits, in routing order.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub selection_rank: i64,
    /// Index in the ordered catalog rows, so a log can name what outranked it.
    pub position: usize,
}

/// A catalog model the policy refused, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub name: String,
    pub provider: String,
    pub reason: String,
    pub position: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoutePlan {
    pub candidates: Vec<Candidate>,
    pub rejected: Vec<Rejection>,
}

impl RoutePlan {
    /// Rejections that ranked ahead of `position` — the ones that explain why
    /// the winner won.
    pub fn rejected_before(&self, position: usize) -> String {
        let parts: Vec<String> = self
            .rejected
            .iter()
            .filter(|r| r.position < position)
            .map(|r| format!("{} ({})", r.name, r.reason))
            .collect();
        if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Filter catalog rows — already capability-matched and ordered by
/// `select_models` (`cost ASC, selection_rank ASC, context_window DESC`) —
/// through the policy, preserving order. `unavailable` lists models observed
/// returning `subscription_required` recently. `lookup`, `key_present` and `budget` (headroom, `services/budget.rs`) are
/// injected so tests can judge a catalog without touching the environment.
pub fn route(
    rows: &[Value],
    ctx: RouteContext,
    policy: &RoutingPolicy,
    lookup: impl Fn(&str) -> Option<ProviderEntry>,
    key_present: impl Fn(&ProviderEntry) -> bool,
    budget: impl Fn(&str, &str, &ProviderEntry) -> Result<(), String>,
    unavailable: &[String],
) -> RoutePlan {
    let mut plan = RoutePlan::default();
    for (position, row) in rows.iter().enumerate() {
        let model = row["model"].as_str().unwrap_or("").to_string();
        if model.is_empty() {
            continue;
        }
        let name = row["name"].as_str().unwrap_or(&model).to_string();
        let provider = provider_registry::canonical_name(row["provider"].as_str().unwrap_or(""));
        let entry = lookup(&provider);
        let key = entry.as_ref().is_some_and(&key_present);
        let model_limits = provider_registry::model_limits(&provider, &model);
        let verdict = policy
            .check_model(&provider, entry.as_ref(), key, model_limits.as_ref(), ctx)
            .and_then(|()| match entry.as_ref() {
                Some(e) => budget(&provider, &model, e),
                None => Ok(()),
            })
            .and_then(|()| {
                if unavailable.contains(&model) {
                    Err("returned subscription_required in the last 7 days".to_string())
                } else {
                    Ok(())
                }
            });
        match verdict {
            Ok(()) => plan.candidates.push(Candidate {
                name,
                provider,
                model,
                selection_rank: row["selection_rank"].as_i64().unwrap_or(-1),
                position,
            }),
            Err(reason) => plan.rejected.push(Rejection {
                name,
                provider,
                reason,
                position,
            }),
        }
    }
    plan
}

/// Refuse a sub-agent loop on declared limits too small to carry one.
fn loop_capacity(limits: &provider_registry::Limits, ctx: RouteContext) -> Result<(), String> {
    if ctx.purpose != Purpose::Subagent {
        return Ok(());
    }
    if let Some(rpm) = limits.rpm
        && rpm < LOOP_MIN_RPM
    {
        return Err(format!(
            "declared {rpm} requests/minute cannot carry a multi-round agent loop \
             (needs {LOOP_MIN_RPM})"
        ));
    }
    if let Some(rpd) = limits.rpd
        && rpd < LOOP_MIN_RPD
    {
        return Err(format!(
            "declared {rpd} requests/day cannot carry agent loops (needs {LOOP_MIN_RPD})"
        ));
    }
    Ok(())
}

/// Stamp a chain or schedule file's top-level `data_class` onto every step that
/// does not declare its own, so the class travels inside the stored step JSON.
pub fn stamp_data_class(steps: &mut [Value], class: DataClass) {
    for step in steps {
        if let Value::Object(m) = step {
            m.entry("data_class")
                .or_insert_with(|| Value::String(class.as_str().to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(yaml: &str) -> ProviderEntry {
        serde_yaml::from_str(yaml).expect("entry parses")
    }

    fn registry() -> HashMap<String, ProviderEntry> {
        let mut m = HashMap::new();
        m.insert(
            "llamacpp".into(),
            entry("kind: llamacpp\nbilling: local\ndata_policy: no_log"),
        );
        m.insert(
            "ollama-cloud".into(),
            entry(
                "kind: openai_compat\nbilling: free\ndata_policy: no_log\n\
                 base_url_default: https://ollama.com",
            ),
        );
        m.insert(
            "gemini".into(),
            entry("kind: gemini\nbilling: free\ndata_policy: may_train\nlimits: { rpm: 5 }"),
        );
        m.insert(
            "openrouter".into(),
            entry(
                "kind: openai_compat\nbilling: free\ndata_policy: may_train\n\
                 base_url_default: https://openrouter.ai/api\nlimits: { rpm: 20, rpd: 50 }",
            ),
        );
        m.insert(
            "anthropic".into(),
            entry(
                "kind: anthropic\nbilling: paid\ndata_policy: no_log\n\
                 spend: { daily_usd: 1.0 }",
            ),
        );
        m.insert(
            "uncapped".into(),
            entry("kind: anthropic\nbilling: paid\ndata_policy: no_log"),
        );
        m
    }

    fn check(
        policy: &RoutingPolicy,
        name: &str,
        purpose: Purpose,
        class: DataClass,
    ) -> Result<(), String> {
        let r = registry();
        policy.check_entry(name, r.get(name), true, RouteContext::new(purpose, class))
    }

    fn tier(t: &str) -> RoutingPolicy {
        RoutingPolicy::from_values(None, Some(t), None)
    }

    #[test]
    fn cloud_tier_maps_onto_billing_classes() {
        assert_eq!(tier("0").billing, vec![Billing::Local]);
        assert_eq!(tier("1").billing, vec![Billing::Local, Billing::Free]);
        assert_eq!(
            tier("2").billing,
            vec![Billing::Local, Billing::Free, Billing::Paid]
        );
        assert_eq!(
            RoutingPolicy::from_values(None, None, None).billing,
            vec![Billing::Local, Billing::Free],
            "unset CLOUD_TIER keeps the old default of 1"
        );
    }

    #[test]
    fn routing_billing_overrides_cloud_tier_and_blank_does_not() {
        let p = RoutingPolicy::from_values(Some("local, paid"), Some("0"), None);
        assert_eq!(p.billing, vec![Billing::Local, Billing::Paid]);
        let blank = RoutingPolicy::from_values(Some("  "), Some("0"), None);
        assert_eq!(blank.billing, vec![Billing::Local]);
    }

    #[test]
    fn nothing_may_spend_until_roufting_paid_for_names_it() {
        let p = tier("2");
        assert!(p.paid_for.is_empty());
        let err = check(&p, "anthropic", Purpose::Chat, DataClass::Private).unwrap_err();
        assert!(err.contains("ROUTING_PAID_FOR=unset"), "{err}");

        let chat = RoutingPolicy::from_values(None, Some("2"), Some("chat"));
        assert!(check(&chat, "anthropic", Purpose::Chat, DataClass::Private).is_ok());
        assert!(check(&chat, "anthropic", Purpose::Subagent, DataClass::Private).is_err());
    }

    #[test]
    fn paid_runs_only_on_work_the_owner_started_or_approved() {
        let p = RoutingPolicy::from_values(None, Some("2"), Some("chat,subagent,background"));
        let r = registry();
        let at = |purpose, initiator| {
            p.check_entry(
                "anthropic",
                r.get("anthropic"),
                true,
                RouteContext::new(purpose, DataClass::Private).with_initiator(initiator),
            )
        };
        // A chat turn is the owner's without saying so.
        assert!(check(&p, "anthropic", Purpose::Chat, DataClass::Private).is_ok());
        for purpose in [Purpose::Subagent, Purpose::Background] {
            let err = at(purpose, Initiator::Brain).unwrap_err();
            assert!(err.contains("owner started"), "{err}");
            assert!(at(purpose, Initiator::Owner).is_ok());
            assert!(at(purpose, Initiator::Approved).is_ok());
        }
        // Free and local providers do not care who started the work.
        assert!(check(&p, "ollama-cloud", Purpose::Background, DataClass::Private).is_ok());
    }

    #[test]
    fn an_unscoped_call_is_the_owners_only_inside_an_owner_request() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(RouteContext::current().initiator, Initiator::Brain);
        let got = rt.block_on(OWNER_REQUEST.scope(true, async { RouteContext::current() }));
        assert_eq!(got.initiator, Initiator::Owner);
        assert_eq!(got.purpose, Purpose::Background);
    }

    #[test]
    fn a_paid_provider_without_a_spend_cap_is_never_routable() {
        let p = RoutingPolicy::from_values(None, Some("2"), Some("chat,subagent,background"));
        let err = check(&p, "uncapped", Purpose::Chat, DataClass::Public).unwrap_err();
        assert!(err.contains("spend cap"), "{err}");
    }

    #[test]
    fn paid_billing_outside_the_policy_is_refused_before_purpose() {
        let p = RoutingPolicy::from_values(None, Some("1"), Some("chat"));
        let err = check(&p, "anthropic", Purpose::Chat, DataClass::Public).unwrap_err();
        assert!(err.contains("billing 'paid'"), "{err}");
    }

    #[test]
    fn private_work_only_reaches_no_log_providers() {
        let p = tier("2");
        for provider in ["gemini", "openrouter"] {
            let err = check(&p, provider, Purpose::Background, DataClass::Private).unwrap_err();
            assert!(err.contains("may_train"), "{provider}: {err}");
            assert!(check(&p, provider, Purpose::Background, DataClass::Public).is_ok());
        }
        assert!(check(&p, "ollama-cloud", Purpose::Background, DataClass::Private).is_ok());
        assert!(check(&p, "llamacpp", Purpose::Chat, DataClass::Private).is_ok());
    }

    #[test]
    fn private_ok_for_opens_a_training_provider_to_the_named_purpose_only() {
        let p = tier("2");
        let gemini = entry(
            "kind: gemini\nbilling: free\ndata_policy: may_train\nlimits: { rpm: 5 }\n\
             private_ok_for: [chat]",
        );
        let at = |purpose| {
            p.check_entry(
                "gemini",
                Some(&gemini),
                true,
                RouteContext::new(purpose, DataClass::Private),
            )
        };
        assert!(at(Purpose::Chat).is_ok());
        let err = at(Purpose::Background).unwrap_err();
        assert!(err.contains("private_ok_for"), "{err}");
        // The override lifts the privacy filter, not the loop-capacity one.
        assert!(at(Purpose::Subagent).is_err());
    }

    #[test]
    fn a_models_own_rpm_keeps_it_out_of_subagent_loops() {
        let p = tier("2");
        let bare = entry("kind: gemini\nbilling: free\ndata_policy: no_log");
        let five = provider_registry::Limits {
            rpm: Some(5),
            ..Default::default()
        };
        let sub = RouteContext::new(Purpose::Subagent, DataClass::Public);
        assert!(
            p.check_model("gemini", Some(&bare), true, None, sub)
                .is_ok()
        );
        let err = p
            .check_model("gemini", Some(&bare), true, Some(&five), sub)
            .unwrap_err();
        assert!(err.contains("5 requests/minute"), "{err}");
        let chat = RouteContext::new(Purpose::Chat, DataClass::Public);
        assert!(
            p.check_model("gemini", Some(&bare), true, Some(&five), chat)
                .is_ok()
        );
    }

    #[test]
    fn tier_zero_admits_only_local() {
        let p = tier("0");
        assert!(check(&p, "llamacpp", Purpose::Background, DataClass::Public).is_ok());
        assert!(check(&p, "ollama-cloud", Purpose::Background, DataClass::Public).is_err());
    }

    #[test]
    fn a_rate_limited_free_tier_cannot_carry_a_sub_agent_loop() {
        let p = tier("2");
        let err = check(&p, "gemini", Purpose::Subagent, DataClass::Public).unwrap_err();
        assert!(err.contains("requests/minute"), "{err}");
        let err = check(&p, "openrouter", Purpose::Subagent, DataClass::Public).unwrap_err();
        assert!(err.contains("requests/day"), "{err}");
        // The same providers remain fine for a single public chat or background call.
        assert!(check(&p, "gemini", Purpose::Chat, DataClass::Public).is_ok());
    }

    #[test]
    fn disabled_unkeyed_and_unknown_providers_are_refused() {
        let p = tier("2");
        let mut e =
            entry("kind: openai_compat\nbilling: free\ndata_policy: no_log\nbase_url_default: x");
        e.enabled = false;
        let ctx = RouteContext::new(Purpose::Chat, DataClass::Public);
        assert!(
            p.check_entry("x", Some(&e), true, ctx)
                .unwrap_err()
                .contains("disabled")
        );
        e.enabled = true;
        e.api_key_env = Some("SOME_KEY".into());
        assert!(
            p.check_entry("x", Some(&e), false, ctx)
                .unwrap_err()
                .contains("SOME_KEY")
        );
        assert!(
            p.check_entry("vllm", None, true, ctx)
                .unwrap_err()
                .contains("not in the provider registry")
        );
    }

    #[test]
    fn private_is_the_default_and_wins_any_combination() {
        assert_eq!(DataClass::default(), DataClass::Private);
        assert_eq!(DataClass::parse_or_private("PUBLIC"), DataClass::Public);
        assert_eq!(DataClass::parse_or_private("secret"), DataClass::Private);
        assert_eq!(
            DataClass::Public.stricter(DataClass::Private),
            DataClass::Private
        );
        assert_eq!(
            DataClass::Private.stricter(DataClass::Public),
            DataClass::Private
        );
        assert_eq!(
            DataClass::Public.stricter(DataClass::Public),
            DataClass::Public
        );
    }

    #[test]
    fn outside_any_scope_work_is_background_and_private() {
        assert_eq!(
            RouteContext::current(),
            RouteContext::new(Purpose::Background, DataClass::Private)
        );
    }

    fn rows(models: &[(&str, &str)]) -> Vec<Value> {
        models
            .iter()
            .enumerate()
            .map(|(i, (model, provider))| {
                json!({"name": model, "model": model, "provider": provider, "selection_rank": i})
            })
            .collect()
    }

    /// The plan's named failure mode: private work whose only capable models
    /// sit on training providers must get NO candidate, not a fallback onto one.
    #[test]
    fn private_work_with_only_training_providers_gets_an_empty_plan() {
        let r = registry();
        let plan = route(
            &rows(&[("gemini-2.5-flash", "gemini"), ("gemma-free", "openrouter")]),
            RouteContext::new(Purpose::Background, DataClass::Private),
            &tier("2"),
            |n| r.get(n).cloned(),
            |_| true,
            |_, _, _| Ok(()),
            &[],
        );
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.rejected.len(), 2);
    }

    #[test]
    fn route_keeps_catalog_order_and_explains_what_outranked_the_winner() {
        let r = registry();
        let plan = route(
            &rows(&[
                ("gemini-2.5-flash", "gemini"),
                ("gpt-oss:120b-cloud", "ollama-cloud"),
                ("dead-model", "ollama-cloud"),
                ("gemma4-e4b", "llamacpp"),
            ]),
            RouteContext::new(Purpose::Background, DataClass::Private),
            &tier("2"),
            |n| r.get(n).cloned(),
            |_| true,
            |_, _, _| Ok(()),
            &["dead-model".to_string()],
        );
        let names: Vec<_> = plan.candidates.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(names, ["gpt-oss:120b-cloud", "gemma4-e4b"]);
        let why = plan.rejected_before(plan.candidates[0].position);
        assert!(
            why.contains("gemini-2.5-flash") && why.contains("may_train"),
            "{why}"
        );
        assert!(
            plan.rejected
                .iter()
                .any(|x| x.reason.contains("subscription_required"))
        );
    }

    #[test]
    fn a_file_level_class_fills_steps_without_overriding_them() {
        let mut steps = vec![
            json!({"tool_name": "search_web"}),
            json!({"tool_name": "reason", "data_class": "private"}),
        ];
        stamp_data_class(&mut steps, DataClass::Public);
        assert_eq!(steps[0]["data_class"], "public");
        assert_eq!(steps[1]["data_class"], "private");
    }

    #[test]
    fn a_model_without_budget_headroom_is_skipped_for_the_next() {
        let r = registry();
        let plan = route(
            &rows(&[
                ("gpt-oss:120b-cloud", "ollama-cloud"),
                ("gemma4-e4b", "llamacpp"),
            ]),
            RouteContext::new(Purpose::Background, DataClass::Private),
            &tier("2"),
            |n| r.get(n).cloned(),
            |_| true,
            |_, model, _| {
                if model == "gpt-oss:120b-cloud" {
                    Err("cooling down".to_string())
                } else {
                    Ok(())
                }
            },
            &[],
        );
        let names: Vec<_> = plan.candidates.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(names, ["gemma4-e4b"]);
        assert!(plan.rejected[0].reason.contains("cooling down"));
    }

    #[test]
    fn a_gpu_lease_refuses_local_providers_only() {
        let mut p = tier("2");
        p.gpu_lease = Some("the GPU is leased to finetune".into());
        let err = check(&p, "llamacpp", Purpose::Chat, DataClass::Private).unwrap_err();
        assert!(err.contains("leased to finetune"), "{err}");
        assert!(check(&p, "ollama-cloud", Purpose::Background, DataClass::Private).is_ok());
    }
}
