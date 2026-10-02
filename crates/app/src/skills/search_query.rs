//! Google search syntax, parsed once and rendered for every engine.
//!
//! `search_web` queries may carry Google's documented operators
//! (support.google.com/websearch/answer/2466433): `"exact phrase"`, `-word`,
//! `site:example.com`, `-site:example.com`, `filetype:pdf`,
//! `after:2026-09-01`, `before:2026-10-01`, and `OR`. A `recency` window
//! (day/week/month/year) and a `news` flag come in as arguments.
//!
//! The engines disagree about all of it. The Google family (SerpApi, Serper,
//! Google CSE, Google News) reads the operators in the query text and also has
//! a native recency parameter; Brave and SearXNG's backends read `site:`,
//! quotes and `-` but not the date operators; Exa and Tavily read none of them
//! — operator text there is just noise words — but take domains and date
//! ranges as API parameters. So the query is parsed into a [`QuerySpec`] and
//! each engine renders what it understands, putting the rest in its own
//! parameters. Without this, `site:reuters.com` sent to Exa searched for the
//! word "site", and a "today" query on Google News returned March stories
//! because nothing asked for recency (2026-10-02: 0 of 8 results were current).

use chrono::{Duration, NaiveDate, Utc};

/// A relative recency window, mapped onto each engine's own filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "day" | "d" | "24h" | "today" => Some(Recency::Day),
            "week" | "w" | "7d" => Some(Recency::Week),
            "month" | "m" | "30d" => Some(Recency::Month),
            "year" | "y" | "365d" => Some(Recency::Year),
            _ => None,
        }
    }

    pub fn days(self) -> i64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 31,
            Recency::Year => 365,
        }
    }

    /// One-letter code shared by Google `tbs=qdr:` and SearXNG-style ranges.
    fn letter(self) -> char {
        match self {
            Recency::Day => 'd',
            Recency::Week => 'w',
            Recency::Month => 'm',
            Recency::Year => 'y',
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    /// The smallest window that still covers `since`, for engines that only
    /// take a window (SearXNG, Tavily) when the caller gave an `after:` date.
    fn covering(since: NaiveDate, today: NaiveDate) -> Self {
        match (today - since).num_days() {
            d if d <= 1 => Recency::Day,
            d if d <= 7 => Recency::Week,
            d if d <= 31 => Recency::Month,
            _ => Recency::Year,
        }
    }
}

/// A free-text piece of the query, in the order it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Word(String),
    Phrase(String),
    Or,
}

/// A parsed query. Free text keeps its order (so `a OR b` survives); the
/// operators are pulled out into fields each engine can map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuerySpec {
    parts: Vec<Part>,
    pub exclude_words: Vec<String>,
    pub exclude_phrases: Vec<String>,
    pub sites: Vec<String>,
    pub exclude_sites: Vec<String>,
    pub filetype: Option<String>,
    pub after: Option<NaiveDate>,
    pub before: Option<NaiveDate>,
    pub recency: Option<Recency>,
    pub news: bool,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim().replace('/', "-");
    NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()
}

fn clean_domain(s: &str) -> String {
    s.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

impl QuerySpec {
    /// Parse Google syntax. Anything that is not an operator stays free text;
    /// an unparseable `after:`/`before:` date is kept as a word rather than lost.
    pub fn parse(query: &str) -> Self {
        let mut spec = QuerySpec::default();
        let chars: Vec<char> = query.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_whitespace() {
                i += 1;
                continue;
            }
            let negated = chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace();
            let start = if negated { i + 1 } else { i };
            if chars.get(start) == Some(&'"') {
                let close = chars[start + 1..]
                    .iter()
                    .position(|c| *c == '"')
                    .map(|p| start + 1 + p);
                let end = close.unwrap_or(chars.len());
                let phrase: String = chars[start + 1..end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !phrase.is_empty() {
                    if negated {
                        spec.exclude_phrases.push(phrase);
                    } else {
                        spec.parts.push(Part::Phrase(phrase));
                    }
                }
                i = close.map(|c| c + 1).unwrap_or(chars.len());
                continue;
            }
            let end = chars[start..]
                .iter()
                .position(|c| c.is_whitespace())
                .map(|p| start + p)
                .unwrap_or(chars.len());
            // Grouping parentheses, as in `(site:a OR site:b)`, are syntax.
            let token: String = chars[start..end]
                .iter()
                .collect::<String>()
                .trim_matches(|c| c == '(' || c == ')')
                .to_string();
            i = end;
            if token.is_empty() {
                continue;
            }
            let lower = token.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("site:").filter(|v| !v.is_empty()) {
                let d = clean_domain(v);
                if negated {
                    spec.exclude_sites.push(d);
                } else {
                    spec.sites.push(d);
                }
            } else if let Some(v) = lower.strip_prefix("filetype:").filter(|v| !v.is_empty()) {
                spec.filetype = Some(v.to_string());
            } else if let Some(d) = lower.strip_prefix("after:").and_then(parse_date) {
                spec.after = Some(d);
            } else if let Some(d) = lower.strip_prefix("before:").and_then(parse_date) {
                spec.before = Some(d);
            } else if negated {
                spec.exclude_words.push(token);
            } else if token == "OR" || token == "|" {
                spec.parts.push(Part::Or);
            } else {
                spec.parts.push(Part::Word(token));
            }
        }
        spec
    }

    /// Restrict to these domains (a SourceList), on top of any `site:` given.
    pub fn add_sites(&mut self, domains: &[String]) {
        for d in domains {
            let d = clean_domain(d);
            if !d.is_empty() && !self.sites.contains(&d) {
                self.sites.push(d);
            }
        }
    }

    /// The earliest date a result may carry: the later of `after:` and the
    /// recency window's start.
    pub fn since(&self, today: NaiveDate) -> Option<NaiveDate> {
        let from_recency = self.recency.map(|r| today - Duration::days(r.days()));
        match (self.after, from_recency) {
            (Some(a), Some(r)) => Some(a.max(r)),
            (a, r) => a.or(r),
        }
    }

    /// A window covering [`Self::since`], for engines that take only a window.
    pub fn window(&self, today: NaiveDate) -> Option<Recency> {
        self.recency
            .or_else(|| self.since(today).map(|s| Recency::covering(s, today)))
    }

    fn today() -> NaiveDate {
        Utc::now().date_naive()
    }

    /// Free text only: words, `OR`, and quoted phrases, in the order written.
    fn free_text(&self, quote_phrases: bool, keep_or: bool) -> String {
        let mut out: Vec<String> = Vec::new();
        // An OR whose neighbours were operators (`site:a OR site:b`) has
        // nothing left to join once those are pulled out.
        let is_text = |p: Option<&Part>| matches!(p, Some(Part::Word(_)) | Some(Part::Phrase(_)));
        for (i, p) in self.parts.iter().enumerate() {
            if matches!(p, Part::Or)
                && !(i > 0 && is_text(self.parts.get(i - 1)) && is_text(self.parts.get(i + 1)))
            {
                continue;
            }
            match p {
                Part::Word(w) => out.push(w.clone()),
                Part::Phrase(ph) if quote_phrases => out.push(format!("\"{ph}\"")),
                Part::Phrase(ph) => out.push(ph.clone()),
                Part::Or if keep_or => out.push("OR".into()),
                Part::Or => {}
            }
        }
        out.join(" ")
    }

    /// Google syntax. `with_dates` adds `after:`/`before:` (Google web search
    /// honours them; SearXNG's other backends and Brave do not, so those get a
    /// window parameter instead).
    pub fn google_syntax(&self, with_dates: bool) -> String {
        let mut out = vec![self.free_text(true, true)];
        out.extend(self.exclude_words.iter().map(|w| format!("-{w}")));
        out.extend(self.exclude_phrases.iter().map(|p| format!("-\"{p}\"")));
        match self.sites.len() {
            0 => {}
            1 => out.push(format!("site:{}", self.sites[0])),
            // Google caps query length; 15 sites was the SourceList bound.
            _ => out.push(format!(
                "({})",
                self.sites
                    .iter()
                    .take(15)
                    .map(|s| format!("site:{s}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            )),
        }
        out.extend(self.exclude_sites.iter().map(|s| format!("-site:{s}")));
        if let Some(ft) = &self.filetype {
            out.push(format!("filetype:{ft}"));
        }
        if with_dates {
            if let Some(a) = self.since(Self::today()) {
                out.push(format!("after:{}", a.format("%Y-%m-%d")));
            }
            if let Some(b) = self.before {
                out.push(format!("before:{}", b.format("%Y-%m-%d")));
            }
        }
        out.retain(|s| !s.trim().is_empty());
        out.join(" ")
    }

    /// For Exa and Tavily, which read no operators: the words and phrases,
    /// with `OR` dropped (it is a noise word to a neural / keyword ranker).
    pub fn plain_text(&self, quote_phrases: bool) -> String {
        self.free_text(quote_phrases, false)
    }

    /// The first phrase, if it is short enough for Exa's `includeText`
    /// (one string of at most five words).
    pub fn exa_include_text(&self) -> Option<String> {
        self.parts.iter().find_map(|p| match p {
            Part::Phrase(ph) if ph.split_whitespace().count() <= 5 => Some(ph.clone()),
            _ => None,
        })
    }

    /// The first exclusion, if short enough for Exa's `excludeText` (one
    /// string, at most five words). Exa cannot take more than one.
    pub fn exa_exclude_text(&self) -> Option<String> {
        self.exclude_words.first().cloned().or_else(|| {
            self.exclude_phrases
                .iter()
                .find(|p| p.split_whitespace().count() <= 5)
                .cloned()
        })
    }

    pub fn has_phrases(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Phrase(_)))
    }

    /// `tbs=qdr:X` for Google web (SerpApi / Serper), from the window.
    pub fn google_tbs(&self) -> Option<String> {
        self.window(Self::today())
            .map(|w| format!("qdr:{}", w.letter()))
    }

    /// Google CSE `dateRestrict` (`d1`, `w1`, `m1`, `y1`).
    pub fn cse_date_restrict(&self) -> Option<String> {
        self.window(Self::today())
            .map(|w| format!("{}1", w.letter()))
    }

    /// Google News' own recency operator, appended to the query.
    pub fn google_news_when(&self) -> Option<String> {
        let since = self.since(Self::today())?;
        let days = (Self::today() - since).num_days().max(1);
        Some(format!("when:{days}d"))
    }

    /// Brave `freshness`: `pd`/`pw`/`pm`/`py`, or an explicit date range.
    pub fn brave_freshness(&self) -> Option<String> {
        if self.recency.is_none()
            && let (Some(a), Some(b)) = (self.after, self.before)
        {
            return Some(format!(
                "{}to{}",
                a.format("%Y-%m-%d"),
                b.format("%Y-%m-%d")
            ));
        }
        self.window(Self::today())
            .map(|w| format!("p{}", w.letter()))
    }

    /// SearXNG `time_range` (`day`/`week`/`month`/`year`).
    pub fn searxng_time_range(&self) -> Option<&'static str> {
        self.window(Self::today()).map(Recency::word)
    }

    /// Exa `startPublishedDate` / `endPublishedDate` (ISO 8601).
    pub fn exa_dates(&self) -> (Option<String>, Option<String>) {
        (
            self.since(Self::today())
                .map(|d| format!("{}T00:00:00.000Z", d.format("%Y-%m-%d"))),
            self.before
                .map(|d| format!("{}T00:00:00.000Z", d.format("%Y-%m-%d"))),
        )
    }

    /// Tavily `start_date` / `end_date` (`YYYY-MM-DD`).
    pub fn tavily_dates(&self) -> (Option<String>, Option<String>) {
        (
            self.since(Self::today())
                .map(|d| d.format("%Y-%m-%d").to_string()),
            self.before.map(|d| d.format("%Y-%m-%d").to_string()),
        )
    }

    /// What to log as the query: the Google rendering, which shows every
    /// constraint in one line.
    pub fn display(&self) -> String {
        let mut s = self.google_syntax(true);
        if let Some(r) = self.recency {
            s.push_str(&format!(" [recency:{}]", r.word()));
        }
        if self.news {
            s.push_str(" [news]");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parses_every_documented_operator() {
        let q = QuerySpec::parse(
            r#"jaguar "top speed" -car -"used cars" site:www.reuters.com -site:pinterest.com filetype:pdf after:2026/03/10 before:2026-04-01"#,
        );
        assert_eq!(q.plain_text(true), r#"jaguar "top speed""#);
        assert_eq!(q.exclude_words, vec!["car"]);
        assert_eq!(q.exclude_phrases, vec!["used cars"]);
        assert_eq!(q.sites, vec!["reuters.com"]);
        assert_eq!(q.exclude_sites, vec!["pinterest.com"]);
        assert_eq!(q.filetype.as_deref(), Some("pdf"));
        assert_eq!(q.after, Some(d("2026-03-10")));
        assert_eq!(q.before, Some(d("2026-04-01")));
    }

    #[test]
    fn google_rendering_round_trips_and_keeps_or_in_place() {
        let q = QuerySpec::parse(r#"meshtastic OR reticulum "mesh network" -site:reddit.com"#);
        assert_eq!(
            q.google_syntax(false),
            r#"meshtastic OR reticulum "mesh network" -site:reddit.com"#
        );
        // OR is a Google operator, not a word: Exa and Tavily never see it.
        assert_eq!(q.plain_text(false), "meshtastic reticulum mesh network");
    }

    #[test]
    fn a_grouped_site_list_parses_to_sites_without_stray_ors() {
        let q = QuerySpec::parse(
            "latest major world news (site:apnews.com OR site:reuters.com OR site:bbc.com)",
        );
        assert_eq!(q.sites, vec!["apnews.com", "reuters.com", "bbc.com"]);
        assert_eq!(q.plain_text(false), "latest major world news");
        assert_eq!(
            q.google_syntax(false),
            "latest major world news (site:apnews.com OR site:reuters.com OR site:bbc.com)"
        );
    }

    #[test]
    fn a_hyphenated_word_is_not_an_exclusion() {
        let q = QuerySpec::parse("state-of-the-art e-bike");
        assert!(q.exclude_words.is_empty());
        assert_eq!(q.plain_text(false), "state-of-the-art e-bike");
    }

    #[test]
    fn several_sites_are_or_grouped_for_google() {
        let mut q = QuerySpec::parse("tariffs");
        q.add_sites(&["apnews.com".into(), "https://www.reuters.com/".into()]);
        assert_eq!(
            q.google_syntax(false),
            "tariffs (site:apnews.com OR site:reuters.com)"
        );
    }

    #[test]
    fn recency_and_after_combine_to_the_later_bound() {
        let today = d("2026-10-02");
        let mut q = QuerySpec::parse("news after:2026-09-01");
        q.recency = Some(Recency::Week);
        assert_eq!(q.since(today), Some(d("2026-09-25")));
        let q2 = QuerySpec::parse("news after:2026-10-01");
        assert_eq!(q2.window(today), Some(Recency::Day));
    }

    #[test]
    fn engine_parameters_follow_the_window() {
        let mut q = QuerySpec::parse("latest world news");
        q.recency = Some(Recency::Day);
        assert_eq!(q.google_tbs().as_deref(), Some("qdr:d"));
        assert_eq!(q.cse_date_restrict().as_deref(), Some("d1"));
        assert_eq!(q.brave_freshness().as_deref(), Some("pd"));
        assert_eq!(q.searxng_time_range(), Some("day"));
        assert_eq!(q.google_news_when().as_deref(), Some("when:1d"));
        assert!(q.exa_dates().0.is_some());
        let plain = QuerySpec::parse("rust async");
        assert!(plain.google_tbs().is_none() && plain.exa_dates().0.is_none());
    }

    #[test]
    fn an_unparseable_date_stays_in_the_query() {
        let q = QuerySpec::parse("after:yesterday tariffs");
        assert!(q.after.is_none());
        assert_eq!(q.plain_text(false), "after:yesterday tariffs");
    }

    #[test]
    fn exa_include_text_takes_a_short_phrase_only() {
        assert_eq!(
            QuerySpec::parse(r#"x "data poisoning""#)
                .exa_include_text()
                .as_deref(),
            Some("data poisoning")
        );
        assert!(
            QuerySpec::parse(r#""one two three four five six""#)
                .exa_include_text()
                .is_none()
        );
    }

    #[test]
    fn recency_words_parse() {
        assert_eq!(Recency::parse("Day"), Some(Recency::Day));
        assert_eq!(Recency::parse("7d"), Some(Recency::Week));
        assert_eq!(Recency::parse("fortnight"), None);
    }
}
