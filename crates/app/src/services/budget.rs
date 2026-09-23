//! Budget ledger — Phase 3 of `project-docs/PROVIDER_ROUTING_PLAN.md`.
//!
//! Limits act **before** a call, not after a 429. Every model call the brain
//! records goes through [`record_call`] — the single write path for both the
//! DuckDB usage ledger and this in-memory budget — and the routing policy asks
//! [`verdict`] whether a provider/model has headroom before it admits a rung.
//!
//! - **Per provider:** requests in the last 60 s, requests/tokens/spend today
//!   (UTC day) and spend this month, checked against the provider's declared
//!   `limits` and, for a paid provider, its `spend` caps. Seeded at startup from
//!   `model_usage`, so a restart does not hand back a spent quota.
//! - **Per (provider, model):** cooldowns and the last rate-limit headers seen.
//!   On the providers measured, limits and 429s are per model (one Mistral model
//!   was rate-limited while another answered; one OpenRouter `:free` model was
//!   limited upstream while another worked), so a cooldown never benches a
//!   whole provider.
//!
//! Cooldowns come from, most trusted first: a `Retry-After` header, or a
//! `remaining = 0` header with its reset time; a retry hint in the error body
//! (Gemini's `retryDelay`); otherwise an escalating 1 m → 5 m → 1 h backoff that
//! a success resets. A reported limit of `0` benches the model for an hour,
//! after which one call re-probes it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::repository::TelemetryClient;
use crate::services::provider_registry::{self, Billing, ProviderEntry};

/// Escalating cooldowns for consecutive 429s with no retry hint.
const BACKOFF_STEPS_SECS: [i64; 3] = [60, 300, 3600];
/// How long a model the provider reports a limit of 0 for stays benched before
/// one call re-probes it.
const LIMIT_ZERO_COOLDOWN_SECS: i64 = 3600;
/// The sliding window a declared `rpm` is counted over.
const MINUTE_SECS: i64 = 60;
/// Retry hints outside this range are treated as noise.
const MAX_HINT_SECS: f64 = 86_400.0;

#[derive(Debug, Default)]
struct ProviderCounters {
    day: Option<NaiveDate>,
    requests_today: u64,
    tokens_today: u64,
    spend_today: f64,
    month: Option<(i32, u32)>,
    spend_month: f64,
    window: VecDeque<DateTime<Utc>>,
}

impl ProviderCounters {
    /// Start a new day / month when the calendar has moved, and drop requests
    /// that have left the one-minute window.
    fn roll(&mut self, now: DateTime<Utc>) {
        let today = now.date_naive();
        if self.day != Some(today) {
            self.day = Some(today);
            self.requests_today = 0;
            self.tokens_today = 0;
            self.spend_today = 0.0;
        }
        let month = (now.year(), now.month());
        if self.month != Some(month) {
            self.month = Some(month);
            self.spend_month = 0.0;
        }
        let horizon = now - Duration::seconds(MINUTE_SECS);
        while self.window.front().is_some_and(|t| *t <= horizon) {
            self.window.pop_front();
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ModelState {
    cooldown_until: Option<DateTime<Utc>>,
    cooldown_reason: Option<String>,
    backoff_step: usize,
    headers: BTreeMap<String, String>,
    headers_at: Option<DateTime<Utc>>,
}

impl ModelState {
    /// A cooldown only ever lengthens: a short backoff must not cut short a
    /// longer wait the provider asked for.
    fn extend_cooldown(&mut self, until: DateTime<Utc>, reason: String) {
        if self.cooldown_until.is_none_or(|current| until > current) {
            self.cooldown_until = Some(until);
            self.cooldown_reason = Some(reason);
        }
    }
}

/// One recorded model call.
#[derive(Debug, Clone, Copy)]
pub struct CallOutcome<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub success: bool,
    pub tokens: u64,
    pub cost: f64,
    pub error_kind: Option<&'a str>,
    pub error_text: Option<&'a str>,
}

/// A provider's usage at startup, read from `model_usage`.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedRow {
    pub provider: String,
    pub requests_today: u64,
    pub tokens_today: u64,
    pub spend_today: f64,
    pub spend_month: f64,
}

#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    last_minute: u64,
    requests_today: u64,
    tokens_today: u64,
    spend_today: f64,
    spend_month: f64,
}

/// Counters and cooldowns. All methods take `now` so the rules are testable;
/// the process-wide instance is reached through the free functions below.
#[derive(Debug, Default)]
pub struct BudgetBook {
    providers: HashMap<String, ProviderCounters>,
    models: HashMap<(String, String), ModelState>,
}

impl BudgetBook {
    /// Count one call and, when it was rate-limited, start or lengthen the
    /// model's cooldown.
    pub fn record(&mut self, o: &CallOutcome, now: DateTime<Utc>) {
        let name = provider_registry::canonical_name(o.provider);
        let c = self.providers.entry(name.clone()).or_default();
        c.roll(now);
        c.requests_today += 1;
        c.tokens_today += o.tokens;
        c.spend_today += o.cost;
        c.spend_month += o.cost;
        c.window.push_back(now);

        let m = self.models.entry((name, o.model.to_string())).or_default();
        if o.success {
            m.backoff_step = 0;
        } else if o.error_kind == Some("rate_limited") {
            let (secs, reason) = match o.error_text.and_then(retry_hint_secs) {
                Some(s) => (
                    s,
                    format!("rate limited; the provider asked to retry in {s}s"),
                ),
                None => {
                    let s = BACKOFF_STEPS_SECS[m.backoff_step.min(BACKOFF_STEPS_SECS.len() - 1)];
                    m.backoff_step += 1;
                    (s, format!("rate limited; backing off {s}s"))
                }
            };
            m.extend_cooldown(now + Duration::seconds(secs), reason);
        }
    }

    /// Keep the rate-limit headers a response carried and act on the ones that
    /// say the model cannot be called again yet.
    pub fn observe(
        &mut self,
        provider: &str,
        model: &str,
        status: u16,
        headers: &[(String, String)],
        now: DateTime<Utc>,
    ) {
        let seen: BTreeMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
            .filter(|(k, _)| k.contains("ratelimit") || k == "retry-after")
            .collect();
        if seen.is_empty() {
            return;
        }
        let key = (
            provider_registry::canonical_name(provider),
            model.to_string(),
        );
        let m = self.models.entry(key).or_default();

        if (status == 429 || status == 503)
            && let Some(v) = seen.get("retry-after")
            && let Some(until) = parse_reset(v, now)
        {
            m.extend_cooldown(until, format!("the provider said retry-after {v}"));
        }

        for (k, v) in &seen {
            // `x-ratelimit-limit-req-minute` names a limit; `x-ratelimit-tokens-
            // query-cost` does not, even though "ratelimit" contains "limit".
            let bare = k.replace("ratelimit", "");
            let is_remaining = bare.contains("remaining");
            let is_reset = bare.contains("reset");
            if !is_remaining && !is_reset && bare.contains("limit") && v == "0" {
                m.extend_cooldown(
                    now + Duration::seconds(LIMIT_ZERO_COOLDOWN_SECS),
                    format!("the provider reports {k}: 0 for this model; re-probed after an hour"),
                );
            }
            if is_remaining && bare.contains("req") && v == "0" {
                let reset = seen
                    .iter()
                    .find(|(rk, _)| rk.contains("reset") && rk.contains("req"))
                    .or_else(|| seen.iter().find(|(rk, _)| rk.contains("reset")))
                    .and_then(|(_, rv)| parse_reset(rv, now));
                let fallback = if bare.contains("day") {
                    3600
                } else {
                    MINUTE_SECS
                };
                m.extend_cooldown(
                    reset.unwrap_or(now + Duration::seconds(fallback)),
                    format!("the provider reports {k}: 0"),
                );
            }
        }

        m.headers = seen;
        m.headers_at = Some(now);
    }

    /// Whether this provider/model has headroom. `entry` supplies the declared
    /// limits and spend caps.
    pub fn verdict(
        &self,
        provider: &str,
        model: &str,
        entry: &ProviderEntry,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let name = provider_registry::canonical_name(provider);
        if let Some(m) = self.models.get(&(name.clone(), model.to_string()))
            && let Some(until) = m.cooldown_until
            && until > now
        {
            return Err(format!(
                "{model} is cooling down until {} UTC ({})",
                until.format("%H:%M:%S"),
                m.cooldown_reason.as_deref().unwrap_or("rate limited")
            ));
        }
        let u = self.usage(&name, now);
        if let Some(rpm) = entry.limits.rpm
            && u.last_minute >= u64::from(rpm)
        {
            return Err(format!("{name} is at its declared {rpm} requests/minute"));
        }
        if let Some(rpd) = entry.limits.rpd
            && u.requests_today >= u64::from(rpd)
        {
            return Err(format!(
                "{name} has used {} of its declared {rpd} requests today (resets 00:00 UTC)",
                u.requests_today
            ));
        }
        if let Some(tpd) = entry.limits.tpd
            && u.tokens_today >= tpd
        {
            return Err(format!(
                "{name} has used {} of its declared {tpd} tokens today (resets 00:00 UTC)",
                u.tokens_today
            ));
        }
        if entry.billing == Billing::Paid
            && let Some(spend) = &entry.spend
        {
            if let Some(cap) = spend.daily_usd
                && u.spend_today >= cap
            {
                return Err(format!(
                    "{name} has spent ${:.2} of its ${cap:.2} daily cap",
                    u.spend_today
                ));
            }
            if let Some(cap) = spend.monthly_usd
                && u.spend_month >= cap
            {
                return Err(format!(
                    "{name} has spent ${:.2} of its ${cap:.2} monthly cap",
                    u.spend_month
                ));
            }
        }
        Ok(())
    }

    /// Replace a provider's counters with what the ledger holds.
    pub fn seed(&mut self, rows: &[SeedRow], now: DateTime<Utc>) {
        for r in rows {
            let c = self
                .providers
                .entry(provider_registry::canonical_name(&r.provider))
                .or_default();
            c.day = Some(now.date_naive());
            c.month = Some((now.year(), now.month()));
            c.requests_today = r.requests_today;
            c.tokens_today = r.tokens_today;
            c.spend_today = r.spend_today;
            c.spend_month = r.spend_month;
        }
    }

    fn usage(&self, name: &str, now: DateTime<Utc>) -> Usage {
        let Some(c) = self.providers.get(name) else {
            return Usage::default();
        };
        let fresh_day = c.day == Some(now.date_naive());
        let fresh_month = c.month == Some((now.year(), now.month()));
        let horizon = now - Duration::seconds(MINUTE_SECS);
        Usage {
            last_minute: c.window.iter().filter(|t| **t > horizon).count() as u64,
            requests_today: if fresh_day { c.requests_today } else { 0 },
            tokens_today: if fresh_day { c.tokens_today } else { 0 },
            spend_today: if fresh_day { c.spend_today } else { 0.0 },
            spend_month: if fresh_month { c.spend_month } else { 0.0 },
        }
    }

    /// Every registered provider with its declaration, today's usage, headroom
    /// against declared limits, and per-model cooldowns and headers.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Value {
        let mut providers = match provider_registry::snapshot_json() {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        for p in providers.iter_mut() {
            let Some(name) = p["name"].as_str().map(str::to_string) else {
                continue;
            };
            let u = self.usage(&name, now);
            let limits = p["limits"].clone();
            let mut models: Vec<(&String, &ModelState)> = self
                .models
                .iter()
                .filter(|((pn, _), _)| *pn == name)
                .map(|((_, model), m)| (model, m))
                .collect();
            models.sort_by(|a, b| a.0.cmp(b.0));
            let models: Vec<Value> = models
                .into_iter()
                .map(|(model, m)| {
                    json!({
                        "model": model,
                        "cooling_down": m.cooldown_until.is_some_and(|t| t > now),
                        "cooldown_until": m.cooldown_until.map(|t| t.to_rfc3339()),
                        "cooldown_reason": m.cooldown_reason,
                        "consecutive_rate_limits": m.backoff_step,
                        "rate_limit_headers": m.headers,
                        "headers_seen_at": m.headers_at.map(|t| t.to_rfc3339()),
                    })
                })
                .collect();
            if let Value::Object(o) = p {
                o.insert(
                    "today".into(),
                    json!({
                        "requests": u.requests_today,
                        "tokens": u.tokens_today,
                        "spend_usd": u.spend_today,
                    }),
                );
                o.insert("month_spend_usd".into(), json!(u.spend_month));
                o.insert("last_minute_requests".into(), json!(u.last_minute));
                o.insert(
                    "headroom".into(),
                    json!({
                        "requests_minute": limits["rpm"].as_u64().map(|l| l.saturating_sub(u.last_minute)),
                        "requests_day": limits["rpd"].as_u64().map(|l| l.saturating_sub(u.requests_today)),
                        "tokens_day": limits["tpd"].as_u64().map(|l| l.saturating_sub(u.tokens_today)),
                    }),
                );
                o.insert(
                    "concurrency".into(),
                    crate::services::provider_gate::snapshot_json(&name),
                );
                o.insert("models".into(), Value::Array(models));
            }
        }
        json!({
            "generated_at": now.to_rfc3339(),
            "day_resets_at": "00:00 UTC",
            "providers": providers,
        })
    }
}

/// Seconds to wait from a retry hint in an error body: Gemini's
/// `"retryDelay": "34s"`, "Please retry in 12.5s", "try again in 5 minutes".
fn retry_hint_secs(text: &str) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    for marker in [
        "retrydelay",
        "retry-after",
        "retry after",
        "retry in",
        "try again in",
    ] {
        let Some(pos) = lower.find(marker) else {
            continue;
        };
        let tail: String = lower[pos + marker.len()..].chars().take(24).collect();
        let Some(start) = tail.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let rest = &tail[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let Ok(n) = rest[..end].parse::<f64>() else {
            continue;
        };
        let unit = rest[end..].trim_start();
        let secs = if unit.starts_with("ms") {
            n / 1000.0
        } else if unit.starts_with('m') {
            n * 60.0
        } else if unit.starts_with('h') {
            n * 3600.0
        } else {
            n
        };
        if secs > 0.0 && secs <= MAX_HINT_SECS {
            return Some(secs.ceil().max(1.0) as i64);
        }
    }
    None
}

/// A duration written with units: `1s`, `6m0s`, `1h2m3s`, `59.5s`, `200ms`.
/// A bare number is not a unit duration and returns `None`.
fn parse_unit_duration(v: &str) -> Option<f64> {
    let chars: Vec<char> = v.trim().chars().collect();
    if chars.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        if start == i {
            return None;
        }
        let n: f64 = chars[start..i].iter().collect::<String>().parse().ok()?;
        let unit_start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit: String = chars[unit_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        total += n * match unit.as_str() {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
    }
    Some(total)
}

/// When a limit resets: an RFC 3339 instant (Anthropic), a unit duration
/// (`6m0s`, OpenAI-style), epoch milliseconds (OpenRouter), epoch seconds, or
/// seconds from now (`Retry-After`).
fn parse_reset(v: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let v = v.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Some(t.with_timezone(&Utc));
    }
    if let Some(secs) = parse_unit_duration(v) {
        return Some(now + Duration::milliseconds((secs * 1000.0).ceil() as i64));
    }
    let n: f64 = v.parse().ok()?;
    if n > 1e12 {
        DateTime::from_timestamp_millis(n as i64)
    } else if n > 1e9 {
        DateTime::from_timestamp(n as i64, 0)
    } else if (0.0..=MAX_HINT_SECS).contains(&n) {
        Some(now + Duration::milliseconds((n * 1000.0).ceil() as i64))
    } else {
        None
    }
}

// ── Process-wide ledger ─────────────────────────────────────────────────────

fn book() -> MutexGuard<'static, BudgetBook> {
    static BOOK: OnceLock<Mutex<BudgetBook>> = OnceLock::new();
    BOOK.get_or_init(|| Mutex::new(BudgetBook::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Record one model call in the usage ledger (when telemetry is available) and
/// the budget (always). `error_text` feeds retry hints for rate-limited calls.
#[allow(clippy::too_many_arguments)]
pub fn record_call(
    telemetry: Option<&TelemetryClient>,
    provider: &str,
    model: &str,
    tool: Option<&str>,
    success: bool,
    duration_ms: Option<i64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    error_kind: Option<&str>,
    error_text: Option<&str>,
) {
    let cost = telemetry
        .and_then(|tc| {
            tc.record_model_usage(
                model,
                Some(provider),
                tool,
                success,
                duration_ms,
                tokens_in,
                tokens_out,
                error_kind,
            )
            .ok()
        })
        .flatten()
        .unwrap_or(0.0);
    let tokens = (tokens_in.unwrap_or(0).max(0) + tokens_out.unwrap_or(0).max(0)) as u64;
    book().record(
        &CallOutcome {
            provider,
            model,
            success,
            tokens,
            cost,
            error_kind,
            error_text,
        },
        Utc::now(),
    );
}

/// Feed a response's rate-limit headers into the budget. Called where the
/// response arrives, before its body is read.
pub fn observe_headers(
    provider: &str,
    model: &str,
    status: u16,
    headers: &reqwest::header::HeaderMap,
) {
    let pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    book().observe(provider, model, status, &pairs, Utc::now());
}

/// Whether a provider/model has headroom right now.
pub fn verdict(provider: &str, model: &str, entry: &ProviderEntry) -> Result<(), String> {
    let result = book().verdict(provider, model, entry, Utc::now());
    if let Err(ref reason) = result
        && entry.billing == Billing::Paid
        && reason.contains(" cap")
    {
        // The signal that more paid capacity is being asked for than allowed.
        warn!(provider, model, reason = %reason, "spend cap blocks a paid provider");
    }
    result
}

/// Seed today's and this month's counters from `model_usage`.
pub fn seed_from_ledger(tc: &TelemetryClient) {
    let now = Utc::now();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    let month_start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
        .expect("the first of the month exists")
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc();
    match tc.budget_usage_since(&day_start.to_rfc3339(), &month_start.to_rfc3339()) {
        Ok(rows) => {
            let rows: Vec<SeedRow> = rows
                .into_iter()
                .map(
                    |(provider, requests, tokens, spend_today, spend_month)| SeedRow {
                        provider,
                        requests_today: requests.max(0) as u64,
                        tokens_today: tokens.max(0) as u64,
                        spend_today,
                        spend_month,
                    },
                )
                .collect();
            let n = rows.len();
            book().seed(&rows, now);
            info!(providers = n, "Seeded the budget ledger from model_usage");
        }
        Err(e) => warn!(
            error = %e,
            "Could not seed the budget ledger from model_usage — counting from zero"
        ),
    }
}

/// The budget as JSON for `get_llm_budget` and `/api/llm/budget`.
pub fn snapshot_json() -> Value {
    book().snapshot(Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 15, h, m, s).unwrap()
    }

    fn entry(yaml: &str) -> ProviderEntry {
        serde_yaml::from_str(yaml).expect("entry parses")
    }

    fn call<'a>(provider: &'a str, model: &'a str) -> CallOutcome<'a> {
        CallOutcome {
            provider,
            model,
            success: true,
            tokens: 0,
            cost: 0.0,
            error_kind: None,
            error_text: None,
        }
    }

    fn rate_limited<'a>(
        provider: &'a str,
        model: &'a str,
        text: Option<&'a str>,
    ) -> CallOutcome<'a> {
        CallOutcome {
            success: false,
            error_kind: Some("rate_limited"),
            error_text: text,
            ..call(provider, model)
        }
    }

    #[test]
    fn a_declared_daily_request_limit_refuses_until_the_utc_day_turns() {
        let e =
            entry("kind: openai_compat\nbilling: free\nbase_url_default: x\nlimits: { rpd: 2 }");
        let mut b = BudgetBook::default();
        b.record(&call("openrouter", "a"), at(1, 0, 0));
        assert!(b.verdict("openrouter", "a", &e, at(1, 5, 0)).is_ok());
        b.record(&call("openrouter", "b"), at(2, 0, 0));
        let err = b.verdict("openrouter", "a", &e, at(3, 0, 0)).unwrap_err();
        assert!(err.contains("2 of its declared 2 requests today"), "{err}");
        let tomorrow = Utc.with_ymd_and_hms(2026, 9, 16, 0, 0, 1).unwrap();
        assert!(b.verdict("openrouter", "a", &e, tomorrow).is_ok());
    }

    #[test]
    fn the_per_minute_window_slides() {
        let e = entry("kind: gemini\nbilling: free\nlimits: { rpm: 2 }");
        let mut b = BudgetBook::default();
        b.record(&call("gemini", "flash"), at(12, 0, 0));
        b.record(&call("gemini", "flash"), at(12, 0, 30));
        assert!(b.verdict("gemini", "flash", &e, at(12, 0, 45)).is_err());
        assert!(b.verdict("gemini", "flash", &e, at(12, 1, 1)).is_ok());
    }

    #[test]
    fn a_daily_token_limit_counts_both_directions() {
        let e =
            entry("kind: openai_compat\nbilling: free\nbase_url_default: x\nlimits: { tpd: 1000 }");
        let mut b = BudgetBook::default();
        b.record(
            &CallOutcome {
                tokens: 1000,
                ..call("mistral", "small")
            },
            at(9, 0, 0),
        );
        assert!(
            b.verdict("mistral", "small", &e, at(9, 1, 0))
                .unwrap_err()
                .contains("tokens today")
        );
    }

    #[test]
    fn spend_caps_bind_paid_providers_only() {
        let paid = entry(
            "kind: anthropic\nbilling: paid\ndata_policy: no_log\n\
             spend: { daily_usd: 1.0, monthly_usd: 3.0 }",
        );
        let mut b = BudgetBook::default();
        b.record(
            &CallOutcome {
                cost: 1.2,
                ..call("anthropic", "sonnet")
            },
            at(9, 0, 0),
        );
        let err = b
            .verdict("anthropic", "sonnet", &paid, at(10, 0, 0))
            .unwrap_err();
        assert!(err.contains("daily cap"), "{err}");
        // The monthly cap still binds on a new day.
        b.record(
            &CallOutcome {
                cost: 2.0,
                ..call("anthropic", "sonnet")
            },
            at(11, 0, 0),
        );
        let next_day = Utc.with_ymd_and_hms(2026, 9, 16, 9, 0, 0).unwrap();
        let err = b
            .verdict("anthropic", "sonnet", &paid, next_day)
            .unwrap_err();
        assert!(err.contains("monthly cap"), "{err}");

        let free = entry("kind: gemini\nbilling: free\nspend: { daily_usd: 0.01 }");
        b.record(
            &CallOutcome {
                cost: 5.0,
                ..call("gemini", "flash")
            },
            at(9, 0, 0),
        );
        assert!(b.verdict("gemini", "flash", &free, at(9, 1, 0)).is_ok());
    }

    #[test]
    fn repeated_rate_limits_escalate_and_a_success_resets_the_backoff() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.record(&rate_limited("mistral", "small", None), at(12, 0, 0));
        assert!(b.verdict("mistral", "small", &e, at(12, 0, 59)).is_err());
        assert!(b.verdict("mistral", "small", &e, at(12, 1, 1)).is_ok());
        b.record(&rate_limited("mistral", "small", None), at(12, 1, 1));
        assert!(
            b.verdict("mistral", "small", &e, at(12, 5, 0)).is_err(),
            "second 429 waits 5 minutes"
        );
        assert!(b.verdict("mistral", "small", &e, at(12, 6, 2)).is_ok());
        b.record(&call("mistral", "small"), at(12, 6, 2));
        b.record(&rate_limited("mistral", "small", None), at(12, 7, 0));
        assert!(
            b.verdict("mistral", "small", &e, at(12, 8, 1)).is_ok(),
            "back to one minute"
        );
    }

    #[test]
    fn a_cooldown_benches_the_model_not_the_provider() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.record(
            &rate_limited("mistral", "mistral-small-latest", None),
            at(12, 0, 0),
        );
        assert!(
            b.verdict("mistral", "mistral-small-latest", &e, at(12, 0, 10))
                .is_err()
        );
        assert!(
            b.verdict("mistral", "codestral-latest", &e, at(12, 0, 10))
                .is_ok()
        );
    }

    #[test]
    fn a_retry_hint_in_the_error_body_sets_the_cooldown() {
        let e = entry("kind: gemini\nbilling: free");
        let mut b = BudgetBook::default();
        let body =
            r#"Gemini API Error (Status 429): {"error": {"details": [{"retryDelay": "34s"}]}}"#;
        b.record(&rate_limited("gemini", "flash", Some(body)), at(12, 0, 0));
        assert!(b.verdict("gemini", "flash", &e, at(12, 0, 33)).is_err());
        assert!(b.verdict("gemini", "flash", &e, at(12, 0, 35)).is_ok());

        assert_eq!(retry_hint_secs("Please retry in 12.5s."), Some(13));
        assert_eq!(retry_hint_secs("try again in 5 minutes"), Some(300));
        assert_eq!(retry_hint_secs("Rate limit exceeded"), None);
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn retry_after_on_a_429_sets_the_cooldown() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.observe(
            "openrouter",
            "m",
            429,
            &headers(&[("Retry-After", "20")]),
            at(12, 0, 0),
        );
        assert!(b.verdict("openrouter", "m", &e, at(12, 0, 19)).is_err());
        assert!(b.verdict("openrouter", "m", &e, at(12, 0, 21)).is_ok());
    }

    #[test]
    fn remaining_requests_at_zero_wait_for_the_reported_reset() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.observe(
            "some-openai-api",
            "m",
            200,
            &headers(&[
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "6m0s"),
            ]),
            at(12, 0, 0),
        );
        assert!(
            b.verdict("some-openai-api", "m", &e, at(12, 5, 59))
                .is_err()
        );
        assert!(b.verdict("some-openai-api", "m", &e, at(12, 6, 1)).is_ok());

        // OpenRouter reports its reset as epoch milliseconds.
        let reset_ms = at(12, 30, 0).timestamp_millis().to_string();
        b.observe(
            "openrouter",
            "free",
            429,
            &headers(&[
                ("X-RateLimit-Remaining", "0"),
                ("X-RateLimit-Reset", &reset_ms),
            ]),
            at(12, 0, 0),
        );
        // No "req" in these names, so the remaining rule does not apply; the
        // 429 itself is what the recorded call turns into a cooldown.
        assert!(b.verdict("openrouter", "free", &e, at(12, 0, 1)).is_ok());
        assert_eq!(parse_reset(&reset_ms, at(12, 0, 0)), Some(at(12, 30, 0)));
    }

    #[test]
    fn a_reported_limit_of_zero_benches_the_model_for_an_hour() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.observe(
            "mistral",
            "mistral-medium-latest",
            200,
            &headers(&[
                ("x-ratelimit-limit-req-minute", "0"),
                ("x-ratelimit-tokens-query-cost", "0"),
            ]),
            at(12, 0, 0),
        );
        let err = b
            .verdict("mistral", "mistral-medium-latest", &e, at(12, 59, 0))
            .unwrap_err();
        assert!(err.contains("x-ratelimit-limit-req-minute"), "{err}");
        assert!(
            b.verdict("mistral", "mistral-medium-latest", &e, at(13, 0, 1))
                .is_ok()
        );
    }

    #[test]
    fn healthy_headers_are_kept_without_a_cooldown() {
        let e = entry("kind: openai_compat\nbilling: free\nbase_url_default: x");
        let mut b = BudgetBook::default();
        b.observe(
            "mistral",
            "codestral-latest",
            200,
            &headers(&[
                ("x-ratelimit-limit-req-minute", "125"),
                ("x-ratelimit-remaining-req-minute", "124"),
                ("content-type", "application/json"),
            ]),
            at(12, 0, 0),
        );
        assert!(
            b.verdict("mistral", "codestral-latest", &e, at(12, 0, 1))
                .is_ok()
        );
        let state = &b.models[&("mistral".to_string(), "codestral-latest".to_string())];
        assert_eq!(state.headers.len(), 2, "only rate-limit headers are kept");
    }

    #[test]
    fn unit_durations_and_rfc3339_resets_parse() {
        assert_eq!(parse_unit_duration("6m0s"), Some(360.0));
        assert_eq!(parse_unit_duration("1h2m3s"), Some(3723.0));
        assert_eq!(parse_unit_duration("200ms"), Some(0.2));
        assert_eq!(parse_unit_duration("42"), None);
        assert_eq!(
            parse_reset("2026-09-15T12:10:00Z", at(12, 0, 0)),
            Some(at(12, 10, 0))
        );
    }

    #[test]
    fn seeding_from_the_ledger_restores_todays_counts() {
        let tc = TelemetryClient::new(":memory:").unwrap();
        // A catalog row lets a pre-Phase-1 usage row (no provider) be attributed.
        tc.upsert_model(
            "gemma4-e4b",
            "llamacpp",
            "gemma4-e4b",
            16384,
            0.0,
            0.0,
            "[]",
            None,
            None,
            None,
            None,
            Some(1),
        )
        .unwrap();
        tc.record_model_usage(
            "m",
            Some("openrouter"),
            None,
            true,
            Some(1),
            Some(10),
            Some(5),
            None,
        )
        .unwrap();
        tc.record_model_usage(
            "m",
            Some("openrouter"),
            None,
            false,
            Some(1),
            None,
            None,
            Some("rate_limited"),
        )
        .unwrap();
        tc.record_model_usage(
            "gemma4-e4b",
            None,
            None,
            true,
            Some(1),
            Some(1),
            Some(1),
            None,
        )
        .unwrap();

        let now = Utc::now();
        let day = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339();
        let month = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339();
        let mut rows = tc.budget_usage_since(&day, &month).unwrap();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1, rows[0].2),
            ("llamacpp", 1, 2)
        );
        assert_eq!(
            (rows[1].0.as_str(), rows[1].1, rows[1].2),
            ("openrouter", 2, 15)
        );

        let e =
            entry("kind: openai_compat\nbilling: free\nbase_url_default: x\nlimits: { rpd: 2 }");
        let mut b = BudgetBook::default();
        b.seed(
            &rows
                .into_iter()
                .map(|(provider, r, t, s, m)| SeedRow {
                    provider,
                    requests_today: r as u64,
                    tokens_today: t as u64,
                    spend_today: s,
                    spend_month: m,
                })
                .collect::<Vec<_>>(),
            now,
        );
        assert!(
            b.verdict("openrouter", "m", &e, now).is_err(),
            "a restart must not hand back spent requests"
        );
    }

    #[test]
    fn the_snapshot_reports_usage_and_never_a_key() {
        let mut b = BudgetBook::default();
        b.record(
            &rate_limited("ollama-cloud", "gpt-oss:120b-cloud", None),
            Utc::now(),
        );
        let snap = b.snapshot(Utc::now());
        let text = snap.to_string();
        assert!(!text.contains("\"api_key\":"));
        let cloud = snap["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "ollama-cloud")
            .expect("ollama-cloud listed");
        assert_eq!(cloud["today"]["requests"], 1);
        assert_eq!(cloud["models"][0]["cooling_down"], true);
    }
}
