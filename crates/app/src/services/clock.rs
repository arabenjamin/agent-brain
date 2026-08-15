//! The brain's sense of wall-clock time.
//!
//! Two rules, and they pull in opposite directions:
//!
//! 1. **Everything stored stays UTC.** `created_at`, `next_review_at`,
//!    `asserted_at`, job timestamps — all of it is written and compared in UTC.
//!    A graph with mixed local and UTC timestamps cannot be ordered, and the
//!    breakage appears only twice a year at a DST boundary.
//! 2. **Everything *said* is local.** A date in a prompt, a search query, or a
//!    report is a statement about the user's day, not about UTC's.
//!
//! Until 2026-08-12 the brain did only (1) and used it for (2) as well: every
//! `{{date}}` and every "Today's date is …" came from `Utc::now()`, in a
//! container with no `TZ` set. In America/Detroit that means the brain rolls
//! over to tomorrow at 20:00 local. Measured that evening: host `Wed Aug 12
//! 21:22 EDT`, container `Thu Aug 13 01:22 UTC`. So for the last four hours of
//! every day the brain dated its notes to tomorrow, searched for tomorrow's
//! news, and told anyone who asked that it was tomorrow — confidently, because
//! nothing in the pipeline had a second opinion about the date.
//!
//! Local time needs both halves to work: `TZ` in the container environment
//! (`docker-compose.yml`) *and* code that asks for local rather than UTC. Set
//! only the first and every call site here still returns UTC; do only the
//! second and `Local` silently resolves to UTC. Because that failure is silent
//! in both directions, [`log_resolved_timezone`] runs at startup and warns when
//! the zone is unset.

use chrono::{DateTime, Datelike, Local, Utc};
use std::sync::OnceLock;
use tracing::{info, warn};

/// The IANA zone name the process is running in, plus whether it was configured
/// or merely defaulted. Resolved once — the environment does not change under a
/// running container.
fn resolved() -> &'static (String, bool) {
    static ZONE: OnceLock<(String, bool)> = OnceLock::new();
    ZONE.get_or_init(|| {
        // BRAIN_TIMEZONE wins so the brain's sense of "today" can be pinned
        // independently of whatever TZ the rest of the image wants.
        for var in ["BRAIN_TIMEZONE", "TZ"] {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return (v, true);
                }
            }
        }
        // No env var: /etc/localtime is usually a symlink into the zoneinfo
        // tree, which carries the name. chrono reads the file either way; this
        // only recovers the label for display.
        if let Ok(path) = std::fs::read_link("/etc/localtime") {
            let s = path.to_string_lossy();
            if let Some(idx) = s.find("zoneinfo/") {
                let name = s[idx + "zoneinfo/".len()..].trim_matches('/').to_string();
                if !name.is_empty() {
                    return (name, true);
                }
            }
        }
        ("UTC".to_string(), false)
    })
}

/// IANA zone name for display, e.g. `America/Detroit`.
pub fn timezone_name() -> &'static str {
    &resolved().0
}

/// Whether the zone was actually configured, as opposed to falling back to UTC.
pub fn timezone_is_configured() -> bool {
    resolved().1
}

/// Log the resolved zone once at startup.
///
/// An unset zone is a warning rather than an error: UTC is a legitimate
/// deployment choice, but an *accidental* UTC is the bug this module exists to
/// stop, and the two are indistinguishable from the outside.
pub fn log_resolved_timezone() {
    let (name, configured) = resolved();
    let offset = now_local().format("%:z").to_string();
    if *configured {
        info!(timezone = %name, utc_offset = %offset, local_now = %now_stamp(), "Local time resolved");
    } else {
        warn!(
            "No BRAIN_TIMEZONE or TZ set — dates and times default to UTC. Anything \
             the brain says about \"today\" will be wrong for part of every day in \
             a non-UTC zone. Set TZ in docker-compose.yml."
        );
    }
}

/// Now, in the brain's local zone.
pub fn now_local() -> DateTime<Local> {
    Local::now()
}

/// Local calendar date, `YYYY-MM-DD`. This is what `{{date}}` expands to.
pub fn today() -> String {
    now_local().format("%Y-%m-%d").to_string()
}

/// Local day of the week, e.g. `Wednesday`.
pub fn weekday() -> String {
    now_local().weekday().to_string()
}

/// A fully qualified instant with no ambiguity left in it, e.g.
/// `Wednesday 2026-08-12 21:22 America/Detroit (UTC-04:00)`.
///
/// The zone name *and* the numeric offset are both present deliberately: the
/// name alone requires the reader to know the current DST state, and the offset
/// alone loses which zone it is.
pub fn now_stamp() -> String {
    let now = now_local();
    format!(
        "{} {} (UTC{})",
        now.format("%A %Y-%m-%d %H:%M"),
        timezone_name(),
        now.format("%:z")
    )
}

/// Render how long ago a UTC instant was, in the coarsest unit that still says
/// something: `just now`, `3 hours ago`, `6 days ago`, `11 months ago`.
///
/// Retrieval hands the model note content with no indication of its age, which
/// is how a September-2023 benchmark result was relayed as a current standing
/// (see the SLM benchmark watch in CLAUDE.md). A model cannot discount stale
/// material it was never told was stale.
pub fn humanize_age(then: DateTime<Utc>) -> String {
    let secs = (Utc::now() - then).num_seconds();
    if secs < 0 {
        // A future timestamp is a data problem, not an age. Say so rather than
        // rendering a negative duration as an enormous positive one.
        return "timestamped in the future".to_string();
    }
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match secs {
        s if s < 90 => "just now".to_string(),
        s if s < 3600 => plural(s / 60, "minute"),
        s if s < 86_400 => plural(s / 3600, "hour"),
        s if s < 86_400 * 45 => plural(s / 86_400, "day"),
        s if s < 86_400 * 365 => plural(s / (86_400 * 30), "month"),
        s => plural(s / (86_400 * 365), "year"),
    }
}

/// [`humanize_age`] for a timestamp that arrived as a string from Neo4j.
///
/// Accepts RFC 3339 and Neo4j's `toString()` form for a naive datetime (no
/// offset), which is assumed UTC because that is what the brain writes.
/// Anything unparseable yields `None` — an age is an aid, and guessing one is
/// worse than omitting it.
pub fn age_from_iso(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(humanize_age(dt.with_timezone(&Utc)));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(humanize_age(naive.and_utc()));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(s, fmt) {
            return Some(humanize_age(date.and_hms_opt(0, 0, 0)?.and_utc()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn age_uses_coarsest_meaningful_unit() {
        let now = Utc::now();
        assert_eq!(humanize_age(now), "just now");
        assert_eq!(humanize_age(now - Duration::minutes(5)), "5 minutes ago");
        assert_eq!(humanize_age(now - Duration::hours(1)), "1 hour ago");
        assert_eq!(humanize_age(now - Duration::hours(5)), "5 hours ago");
        assert_eq!(humanize_age(now - Duration::days(6)), "6 days ago");
        // The unit that matters most: a benchmark number this old is history,
        // and the label is the only thing that says so.
        assert_eq!(humanize_age(now - Duration::days(330)), "11 months ago");
        assert_eq!(humanize_age(now - Duration::days(800)), "2 years ago");
    }

    #[test]
    fn future_timestamps_are_named_not_rendered_as_ages() {
        let ahead = Utc::now() + Duration::days(3);
        assert_eq!(humanize_age(ahead), "timestamped in the future");
    }

    #[test]
    fn age_from_iso_accepts_the_forms_neo4j_actually_returns() {
        // toString() on a datetime with an offset, and on a naive one.
        assert!(age_from_iso("2026-08-01T12:00:00Z").is_some());
        assert!(age_from_iso("2026-08-01T12:00:00.123456000+00:00").is_some());
        assert!(age_from_iso("2026-08-01T12:00:00.000000000").is_some());
        assert!(age_from_iso("2026-08-01").is_some());
        // Unparseable yields nothing rather than a fabricated age.
        assert!(age_from_iso("").is_none());
        assert!(age_from_iso("some time last week").is_none());
    }

    #[test]
    fn now_stamp_carries_both_zone_name_and_numeric_offset() {
        let s = now_stamp();
        assert!(s.contains(timezone_name()), "zone name missing from {s}");
        assert!(s.contains("(UTC"), "numeric offset missing from {s}");
        assert!(s.contains(&today()), "local date missing from {s}");
    }
}
