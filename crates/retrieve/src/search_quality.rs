//! Cheap quality hints for one engine answer, recorded in the search ledger.
//!
//! Relevance cannot be read from a response (the 2026-09-01 junk-results
//! failure counted as a success), so these are proxies: how many results
//! carry a date, how many fall inside the window the caller asked for, how
//! many come from a domain on one of our source lists, and how many distinct
//! domains answered. The stronger signal, whether a result was later *used*
//! (cited as claim evidence), is joined at report time from the links kept
//! here.

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::Value;

use crate::search_query::QuerySpec;

/// A result's date, from the shapes engines send: ISO 8601 / `YYYY-MM-DD…`,
/// RFC 2822, Google News' `MM/DD/YYYY, hh:mm AM, +0000 UTC`, `Oct 14, 2026`
/// and relative `3 hours ago`. Anything else is undated.
pub fn parse_date(s: &str, now: DateTime<Utc>) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(d) = s
        .get(..10)
        .and_then(|p| NaiveDate::parse_from_str(p, "%Y-%m-%d").ok())
    {
        return Some(d);
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(s) {
        return Some(d.date_naive());
    }
    if let Some(d) = s
        .split(',')
        .next()
        .and_then(|p| NaiveDate::parse_from_str(p.trim(), "%m/%d/%Y").ok())
    {
        return Some(d);
    }
    for fmt in ["%b %d, %Y", "%B %d, %Y", "%d %b %Y", "%d %B %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    relative(s, now)
}

/// `N unit(s) ago`, `yesterday`, `today`.
fn relative(s: &str, now: DateTime<Utc>) -> Option<NaiveDate> {
    let lower = s.to_lowercase();
    match lower.as_str() {
        "today" | "just now" => return Some(now.date_naive()),
        "yesterday" => return Some((now - Duration::days(1)).date_naive()),
        _ => {}
    }
    let mut words = lower.split_whitespace();
    let n: i64 = match words.next()? {
        "a" | "an" => 1,
        w => w.parse().ok()?,
    };
    let unit = words.next()?.trim_end_matches('s');
    if words.next()? != "ago" {
        return None;
    }
    let back = match unit {
        "second" | "sec" | "minute" | "min" => Duration::minutes(n.min(60 * 24)),
        "hour" | "hr" => Duration::hours(n),
        "day" => Duration::days(n),
        "week" => Duration::weeks(n),
        "month" => Duration::days(30 * n),
        "year" => Duration::days(365 * n),
        _ => return None,
    };
    Some((now - back).date_naive())
}

/// The host of a link without `www.`, lowercased.
pub fn host(link: &str) -> Option<String> {
    let rest = link.split_once("://").map(|x| x.1)?;
    let host = rest.split(['/', '?', '#']).next()?.split(':').next()?;
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

/// Whether `host` is `domain` or a subdomain of it.
fn on_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The hints for one answer.
#[derive(Debug, Default, PartialEq)]
pub struct Quality {
    pub dated: i64,
    /// `None` when the caller asked for no date window.
    pub in_window: Option<i64>,
    pub known_source: i64,
    pub domains: i64,
    pub links: Vec<String>,
}

pub fn assess(items: &[Value], q: &QuerySpec, known: &[String], now: DateTime<Utc>) -> Quality {
    let today = now.date_naive();
    let since = q.since(today);
    let windowed = since.is_some() || q.before.is_some();
    let mut out = Quality {
        in_window: windowed.then_some(0),
        ..Default::default()
    };
    let mut hosts: HashSet<String> = HashSet::new();
    for item in items {
        let link = item
            .get("link")
            .or_else(|| item.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !link.is_empty() {
            out.links.push(link.to_string());
        }
        if let Some(h) = host(link) {
            if known.iter().any(|d| on_domain(&h, d)) {
                out.known_source += 1;
            }
            hosts.insert(h);
        }
        let date = item
            .get("date")
            .or_else(|| item.get("publishedDate"))
            .and_then(Value::as_str)
            .and_then(|d| parse_date(d, now));
        if let Some(d) = date {
            out.dated += 1;
            if windowed && since.is_none_or(|s| d >= s) && q.before.is_none_or(|b| d < b) {
                out.in_window = out.in_window.map(|n| n + 1);
            }
        }
    }
    out.domains = hosts.len() as i64;
    out
}

/// A link reduced for matching against a cited one: no scheme, no `www.`,
/// no fragment, no trailing slash, host lowercased.
pub fn link_key(link: &str) -> Option<String> {
    let h = host(link)?;
    let rest = link.split_once("://").map(|x| x.1).unwrap_or(link);
    let path = rest
        .split_once('/')
        .map(|(_, p)| p)
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    Some(if path.is_empty() {
        h
    } else {
        format!("{h}/{path}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        "2026-10-16T16:00:00Z".parse().unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn engine_date_shapes_parse() {
        let n = now();
        assert_eq!(
            parse_date("2026-10-14T07:00:00.000Z", n),
            Some(d("2026-10-14"))
        );
        assert_eq!(parse_date("2026-10-14", n), Some(d("2026-10-14")));
        assert_eq!(
            parse_date("Wed, 14 Oct 2026 07:00:00 GMT", n),
            Some(d("2026-10-14"))
        );
        assert_eq!(
            parse_date("10/14/2026, 07:00 AM, +0000 UTC", n),
            Some(d("2026-10-14"))
        );
        assert_eq!(parse_date("Oct 14, 2026", n), Some(d("2026-10-14")));
        assert_eq!(parse_date("2 days ago", n), Some(d("2026-10-14")));
        assert_eq!(parse_date("12 hours ago", n), Some(d("2026-10-16")));
        assert_eq!(parse_date("an hour ago", n), Some(d("2026-10-16")));
        assert_eq!(parse_date("yesterday", n), Some(d("2026-10-15")));
        assert_eq!(parse_date("recently", n), None);
        assert_eq!(parse_date("", n), None);
    }

    #[test]
    fn an_answer_is_assessed_against_the_window_and_the_lists() {
        let mut q = QuerySpec::parse("fed rate");
        q.recency = crate::search_query::Recency::parse("week");
        let items = vec![
            json!({"link": "https://www.reuters.com/a", "date": "2 days ago"}),
            json!({"link": "https://news.reuters.com/b", "date": "2026-01-01"}),
            json!({"link": "https://blog.example/c"}),
        ];
        let a = assess(&items, &q, &["reuters.com".to_string()], now());
        assert_eq!(a.dated, 2);
        assert_eq!(a.in_window, Some(1));
        assert_eq!(a.known_source, 2);
        assert_eq!(a.domains, 3);
        assert_eq!(a.links.len(), 3);
    }

    #[test]
    fn no_window_means_no_in_window_count() {
        let items = vec![json!({"link": "https://a.example/", "date": "2026-10-01"})];
        let a = assess(&items, &QuerySpec::parse("x"), &[], now());
        assert_eq!(a.in_window, None);
        assert_eq!(a.dated, 1);
    }

    #[test]
    fn links_match_across_cosmetic_differences() {
        assert_eq!(
            link_key("https://www.Reuters.com/world/x/#top"),
            link_key("http://reuters.com/world/x")
        );
        assert_eq!(link_key("https://a.example"), Some("a.example".into()));
        assert_eq!(link_key("not a url"), None);
    }
}
