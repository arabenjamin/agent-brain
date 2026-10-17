//! Search rotation: which engine a `search_web` call tries first.
//!
//! The ladder alone sends every search to its first rung until that rung
//! fails, so one engine carried ~70% of all searches (Exa, 1,359 of ~1,960
//! from 2026-10-02 to 10-16, at $0.007 each: its whole $10 monthly credit)
//! while Tavily's 1,000/month and Serper's 2,500 sat at single digits.
//!
//! An engine whose YAML declares a `budget` is in the rotation. Each call goes
//! first to the budgeted engine that is *on pace* (within what its allowance
//! permits by now) with the fewest calls today relative to its `weight`, so
//! the load spreads evenly and no engine is overdrawn; the rest of the ladder
//! stays behind it as the fallback. Pacing spreads a monthly allowance over
//! the month instead of spending it by the 14th. Everything here is pure; the
//! ledger reads live in `search.rs`.

use agent_brain_repository::EngineUse;
use chrono::{DateTime, Datelike, Utc};

use crate::search_engines::{Budget, BudgetPeriod, BudgetUnit};

/// Where an engine stands against its budget right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    /// Within the allowance so far: eligible to be picked first.
    OnPace,
    /// Spent more than the allowance permits by now; usable as a fallback
    /// (it borrows from later in the period).
    AheadOfPace,
    /// At a vendor rate limit; tried last.
    RateLimited,
    /// The period's whole allowance is spent; tried last.
    Spent,
}

/// When the engine's budget period began.
pub fn period_start(per: BudgetPeriod, now: DateTime<Utc>) -> DateTime<Utc> {
    let midnight =
        |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).map(|t| t.and_utc()).unwrap_or(now);
    match per {
        BudgetPeriod::Day => midnight(now.date_naive()),
        BudgetPeriod::Month => now.date_naive().with_day(1).map(midnight).unwrap_or(now),
        BudgetPeriod::Once => DateTime::<Utc>::UNIX_EPOCH,
    }
}

/// The share of a calendar month (UTC) elapsed at `now`, 0.0..=1.0.
fn month_elapsed(now: DateTime<Utc>) -> f64 {
    let start = period_start(BudgetPeriod::Month, now);
    let (y, m) = (start.year(), start.month());
    let next = if m == 12 {
        chrono::NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(y, m + 1, 1)
    };
    let Some(next) = next
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
    else {
        return 1.0;
    };
    let total = (next - start).num_seconds().max(1) as f64;
    ((now - start).num_seconds() as f64 / total).clamp(0.0, 1.0)
}

/// What one call costs in the budget's unit when the ledger has no price.
fn unit_cost(b: &Budget) -> f64 {
    match b.unit {
        BudgetUnit::Call => 1.0,
        BudgetUnit::Usd => b.cost_default.unwrap_or(0.0),
    }
}

/// Units spent this period (rows from before prices were recorded count at
/// the default cost).
pub fn used(b: &Budget, u: &EngineUse) -> f64 {
    u.period_units + u.period_unpriced as f64 * unit_cost(b)
}

/// How much of the allowance may be spent by `now`. A monthly allowance is
/// spread over the month, with a small burst (3% or three calls, whichever
/// is more) so the first hours of a month are not starved; a daily one
/// resets too soon to need pacing; a one-time grant is all available.
pub fn allowance_so_far(b: &Budget, now: DateTime<Utc>) -> f64 {
    match b.per {
        BudgetPeriod::Day | BudgetPeriod::Once => b.free,
        BudgetPeriod::Month => {
            let burst = (b.free * 0.03).max(3.0 * unit_cost(b));
            (b.free * month_elapsed(now) + burst).min(b.free)
        }
    }
}

pub fn standing(b: &Budget, u: &EngineUse, now: DateTime<Utc>) -> Standing {
    let spent = used(b, u);
    if spent >= b.free {
        Standing::Spent
    } else if b.per_minute.is_some_and(|m| u.minute_calls >= m as i64)
        || b.per_hour.is_some_and(|h| u.hour_calls >= h as i64)
    {
        Standing::RateLimited
    } else if spent >= allowance_so_far(b, now) {
        Standing::AheadOfPace
    } else {
        Standing::OnPace
    }
}

/// One engine in the ladder, as the rotation sees it. `budget` is `None` for
/// an engine outside the rotation (no free allowance declared).
#[derive(Debug, Clone)]
pub struct Rung {
    pub name: String,
    pub standing: Option<Standing>,
    pub day_calls: i64,
    pub weight: f64,
}

/// The ladder, rotation first: on-pace budgeted engines by fewest calls
/// today per unit of weight (ties keep ladder order), then the rest in
/// ladder order, then rate-limited and spent engines. `pinned`, if any,
/// stays at the head. Returns the order and whether its head was chosen by
/// rotation.
pub fn order(ladder: &[Rung], pinned: Option<&str>) -> (Vec<String>, bool) {
    let mut on_pace: Vec<(usize, &Rung)> = ladder
        .iter()
        .enumerate()
        .filter(|(_, r)| r.standing == Some(Standing::OnPace) && Some(r.name.as_str()) != pinned)
        .collect();
    on_pace.sort_by(|(ia, a), (ib, b)| {
        let la = a.day_calls as f64 / a.weight;
        let lb = b.day_calls as f64 / b.weight;
        la.total_cmp(&lb).then(ia.cmp(ib))
    });
    let last = |r: &Rung| matches!(r.standing, Some(Standing::RateLimited | Standing::Spent));
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = pinned {
        out.push(p.to_string());
    }
    let rotated = pinned.is_none() && !on_pace.is_empty();
    out.extend(on_pace.iter().map(|(_, r)| r.name.clone()));
    for r in ladder {
        if !out.contains(&r.name) && !last(r) {
            out.push(r.name.clone());
        }
    }
    for r in ladder {
        if !out.contains(&r.name) {
            out.push(r.name.clone());
        }
    }
    (out, rotated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn budget(free: f64, per: BudgetPeriod) -> Budget {
        Budget {
            free,
            unit: BudgetUnit::Call,
            per,
            cost_field: None,
            cost_default: None,
            per_hour: None,
            per_minute: None,
            weight: None,
        }
    }

    fn use_of(units: f64, day_calls: i64) -> EngineUse {
        EngineUse {
            engine: "x".into(),
            period_units: units,
            day_calls,
            ..Default::default()
        }
    }

    fn rung(name: &str, standing: Option<Standing>, day_calls: i64) -> Rung {
        Rung {
            name: name.into(),
            standing,
            day_calls,
            weight: 1.0,
        }
    }

    #[test]
    fn periods_start_at_utc_boundaries() {
        let now = at("2026-10-16T16:07:22Z");
        assert_eq!(
            period_start(BudgetPeriod::Day, now),
            at("2026-10-16T00:00:00Z")
        );
        assert_eq!(
            period_start(BudgetPeriod::Month, now),
            at("2026-10-01T00:00:00Z")
        );
        assert_eq!(
            period_start(BudgetPeriod::Once, now),
            DateTime::<Utc>::UNIX_EPOCH
        );
    }

    #[test]
    fn a_monthly_allowance_is_spread_over_the_month() {
        let b = budget(1000.0, BudgetPeriod::Month);
        // Halfway through October (31 days): ~500 plus the 30-call burst.
        let mid = at("2026-10-16T12:00:00Z");
        let a = allowance_so_far(&b, mid);
        assert!((a - (1000.0 * 15.5 / 31.0 + 30.0)).abs() < 1.0, "{a}");
        assert_eq!(standing(&b, &use_of(400.0, 0), mid), Standing::OnPace);
        assert_eq!(standing(&b, &use_of(600.0, 0), mid), Standing::AheadOfPace);
        assert_eq!(standing(&b, &use_of(1000.0, 0), mid), Standing::Spent);
        // The first minutes of a month still allow the burst.
        assert!(allowance_so_far(&b, at("2026-11-01T00:05:00Z")) >= 30.0);
    }

    #[test]
    fn rate_limits_count_attempts() {
        let mut b = budget(12000.0, BudgetPeriod::Day);
        b.per_minute = Some(30);
        let mut u = use_of(10.0, 10);
        u.minute_calls = 30;
        assert_eq!(standing(&b, &u, Utc::now()), Standing::RateLimited);
    }

    #[test]
    fn unpriced_rows_count_at_the_default_cost() {
        let mut b = budget(10.0, BudgetPeriod::Month);
        b.unit = BudgetUnit::Usd;
        b.cost_default = Some(0.007);
        let u = EngineUse {
            period_units: 1.0,
            period_unpriced: 1000,
            ..Default::default()
        };
        assert!((used(&b, &u) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn the_least_used_on_pace_engine_goes_first() {
        let ladder = vec![
            rung("exa", Some(Standing::OnPace), 40),
            rung("tavily", Some(Standing::OnPace), 12),
            rung("serper", Some(Standing::Spent), 0),
            rung("brave", None, 0),
            rung("tinyfish", Some(Standing::OnPace), 12),
        ];
        let (o, rotated) = order(&ladder, None);
        assert!(rotated);
        // Ties keep ladder order: tavily before tinyfish.
        assert_eq!(o, ["tavily", "tinyfish", "exa", "brave", "serper"]);
    }

    #[test]
    fn weight_scales_the_share() {
        let mut exa = rung("exa", Some(Standing::OnPace), 20);
        exa.weight = 2.0;
        let ladder = vec![exa, rung("tavily", Some(Standing::OnPace), 12)];
        assert_eq!(order(&ladder, None).0[0], "exa");
    }

    #[test]
    fn a_pinned_engine_leads_and_the_rest_rotate() {
        let ladder = vec![
            rung("exa", Some(Standing::OnPace), 40),
            rung("tavily", Some(Standing::OnPace), 12),
            rung("brave", None, 0),
        ];
        let (o, rotated) = order(&ladder, Some("exa"));
        assert!(!rotated);
        assert_eq!(o, ["exa", "tavily", "brave"]);
    }

    #[test]
    fn with_nothing_on_pace_the_ladder_order_stands() {
        let ladder = vec![
            rung("exa", Some(Standing::AheadOfPace), 40),
            rung("tavily", Some(Standing::RateLimited), 12),
            rung("brave", None, 0),
        ];
        let (o, rotated) = order(&ladder, None);
        assert!(!rotated);
        assert_eq!(o, ["exa", "brave", "tavily"]);
    }
}
