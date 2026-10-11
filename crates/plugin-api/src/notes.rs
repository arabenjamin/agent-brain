//! What a note says about where it came from: the article or video a source
//! note records. Read by the core (the note drawer) and the epistemics
//! plugin (a claim's source), so it lives here.

use serde::Serialize;

/// What a claim's source note says about itself: the article or video it
/// records, so a reader can see where the claim came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SourceRef {
    pub title: Option<String>,
    pub url: Option<String>,
    /// The site or channel (`site` in an article summary, `channel` in a video's).
    pub outlet: Option<String>,
}

/// Read a source note's title, URL and outlet. Article and video records carry
/// a summary JSON object with `title` / `url` / `site` or `channel`; anything
/// else (a watch report, a news brief) falls back to its first line, as a
/// heading of any level, cut to 120 characters, and to a URL only when that
/// line holds one: a URL from the body could be any of the report's links.
pub fn source_reference(content: &str) -> SourceRef {
    let summary = crate::json::first_json_object(content, &["title"]);
    let field = |k: &str| {
        summary
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let heading = content
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty());
    let heading_url = heading.and_then(|h| {
        h.split_whitespace()
            .find(|w| w.starts_with("http://") || w.starts_with("https://"))
            .map(String::from)
    });
    SourceRef {
        title: field("title").or_else(|| heading.map(|h| h.chars().take(120).collect())),
        url: field("url").or(heading_url),
        outlet: field("site").or_else(|| field("channel")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_reference_reads_article_and_video_summaries() {
        let article = "# Article: read article: https://citizenlab.ca/the-secrets-of-a-us-spyware-king/\n\n\
            ## ARTICLE SUMMARY\n{\n  \"key_concepts\": [\"Paragon Solutions spyware\"],\n  \
            \"site\": \"The Citizen Lab\",\n  \"title\": \"The Secrets of a US Spyware King - The Citizen Lab\",\n  \
            \"url\": \"https://citizenlab.ca/the-secrets-of-a-us-spyware-king/\"\n}\n\n## NEW vs KNOWN ANALYSIS\n…";
        assert_eq!(
            source_reference(article),
            SourceRef {
                title: Some("The Secrets of a US Spyware King - The Citizen Lab".into()),
                url: Some("https://citizenlab.ca/the-secrets-of-a-us-spyware-king/".into()),
                outlet: Some("The Citizen Lab".into()),
            }
        );
        let video = "# Video learning: watch video: https://www.youtube.com/watch?v=tPhQ4gAylek\n\n\
            ## VIDEO SUMMARY\n{\"channel\": \"Louis Rossmann\", \"title\": \"Crack your video editing software: seriously.\", \
            \"url\": \"https://www.youtube.com/watch?v=tPhQ4gAylek\"}";
        let v = source_reference(video);
        assert_eq!(
            v.title.as_deref(),
            Some("Crack your video editing software: seriously.")
        );
        assert_eq!(v.outlet.as_deref(), Some("Louis Rossmann"));
    }

    #[test]
    fn source_reference_falls_back_to_the_heading_and_never_guesses_a_url() {
        let report = "# Off-grid networking watch — 2026-08-12\n\n## REPORT\n* MeshCore https://meshcore.co.uk is faster";
        assert_eq!(
            source_reference(report),
            SourceRef {
                title: Some("Off-grid networking watch — 2026-08-12".into()),
                url: None,
                outlet: None
            }
        );
        // A truncated summary still yields the heading's URL.
        let cut = "# Video learning: watch video: https://youtu.be/x\n\n## VIDEO SUMMARY\n{\"title\": \"Cut of";
        assert_eq!(
            source_reference(cut).url.as_deref(),
            Some("https://youtu.be/x")
        );
        // A brief that opens with a second-level heading, and one with none.
        let brief =
            "## EXECUTIVE SUMMARY for 2026-10-05  \n- Detroit officials https://example.com/x";
        assert_eq!(
            source_reference(brief),
            SourceRef {
                title: Some("EXECUTIVE SUMMARY for 2026-10-05".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            source_reference(&"x".repeat(300)).title.map(|t| t.len()),
            Some(120)
        );
        assert_eq!(source_reference("  \n\n"), SourceRef::default());
    }
}
