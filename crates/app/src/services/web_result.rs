//! Judging individual web search results, shared by `search_web` and the claim
//! verifier.

/// First line of the banner `search_web` puts ahead of a SearXNG answer.
pub const SEARXNG_FALLBACK_BANNER: &str = "⚠ SEARXNG FALLBACK";

/// Split a `search_web` result into (answered by the SearXNG fallback?, body).
///
/// The banner is text ahead of the JSON array, ended by a blank line, so a
/// caller that parses the array must strip it first or it reads nothing.
pub fn strip_fallback_banner(text: &str) -> (bool, &str) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(SEARXNG_FALLBACK_BANNER) {
        return (false, text);
    }
    match trimmed.split_once("\n\n") {
        Some((_, body)) => (true, body),
        None => (true, ""),
    }
}

/// Whether a result link is a search engine's own results page.
///
/// `ecosia.org/search?q=<query>`, `dailymotion.com/search/<query>/`,
/// `us.search.yahoo.com/search?p=<query>`: an index page for the query, not a
/// page about the subject. Their snippets restate the query, so a reader takes
/// them as a hit on it — the claim verifier read its own claim echoed back as
/// support (2026-10-01..06). A `search` segment counts only when something
/// follows it (a query string or a further segment), so the Wikipedia article
/// `/wiki/Search` is not one.
pub fn is_search_page(link: &str) -> bool {
    let rest = link.split_once("://").map(|x| x.1).unwrap_or(link);
    let (host, path_and_query) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_lowercase();
    let lower = path_and_query.to_lowercase();
    let (path, has_query) = match lower.split_once(['?', '#']) {
        Some((p, q)) => (p, !q.is_empty()),
        None => (lower.as_str(), false),
    };
    let segments: Vec<&str> = path.split('/').collect();
    let search_with_more = segments.iter().enumerate().any(|(i, s)| {
        *s == "search" && (has_query || segments.get(i + 1).is_some_and(|next| !next.is_empty()))
    });
    search_with_more || (host.starts_with("search.") && has_query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_banner_is_stripped_and_reported() {
        let text = format!(
            "{SEARXNG_FALLBACK_BANNER} — keyed engines unavailable\nmore\n\n[{{\"link\":\"https://a.com\"}}]"
        );
        let (fallback, body) = strip_fallback_banner(&text);
        assert!(fallback);
        assert_eq!(body, r#"[{"link":"https://a.com"}]"#);
        let plain = r#"[{"link":"https://a.com"}]"#;
        assert_eq!(strip_fallback_banner(plain), (false, plain));
    }

    #[test]
    fn engine_results_pages_are_search_pages() {
        for link in [
            "https://www.ecosia.org/search?q=Sonnet%205.5%20matched",
            "https://www.dailymotion.com/search/Prosecutors%20presented/",
            "https://us.search.yahoo.com/search?p=Roblox",
            "https://pinterest.com/search/pins/?q=Folios",
            "https://imgur.com/search?q=Trump",
            "https://www.startpage.com/do/search?q=x",
            "https://news.google.com/search?q=x",
        ] {
            assert!(is_search_page(link), "{link}");
        }
    }

    #[test]
    fn articles_are_not_search_pages() {
        for link in [
            "https://en.wikipedia.org/wiki/Search",
            "https://www.reuters.com/business/fed-seen-skipping-october",
            "https://www.nytimes.com/2026/07/10/business/ai-white-collar-jobs.html",
            "https://www.searchenginejournal.com/some-article/",
            "https://example.com/research/search-and-rescue",
        ] {
            assert!(!is_search_page(link), "{link}");
        }
    }
}
