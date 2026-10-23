//! Claims that a recording proves by itself: an `attribution` whose subject
//! is someone speaking in the recording it came from ("Louis Rossmann
//! notes…", "the interviewee stated…"). Its ASSERTED_IN source already shows
//! it was said, and no outside page repeats a host's aside, so the verifier
//! skips it (`verify_skipped: said_here`). The words are the bundle's
//! `said-here-claims.yaml`, which says how the rule was measured.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const FILE: &str = "said-here-claims.yaml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaidHereRules {
    pub recordings: Vec<String>,
    pub lead_words: usize,
    pub verb_window: usize,
    pub speakers: Vec<String>,
    pub name_stopwords: Vec<String>,
    pub verbs: Vec<String>,
}

/// What the rule reads of a claim.
pub struct ClaimFacts<'a> {
    pub text: &'a str,
    pub kind: Option<&'a str>,
    pub asserted_by: Option<&'a str>,
    pub source_context: Option<&'a str>,
}

/// Lowercase words: letters, digits and apostrophes.
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

impl SaidHereRules {
    pub fn parse(text: &str) -> Result<Self> {
        let r: SaidHereRules = serde_yaml::from_str(text)?;
        anyhow::ensure!(
            !r.recordings.is_empty() && !r.verbs.is_empty(),
            "recordings and verbs must each list something"
        );
        anyhow::ensure!(r.verb_window > 0, "verb_window must be at least 1");
        Ok(r)
    }

    pub fn load_from(dir: &Path) -> Result<Self> {
        let p = dir.join(FILE);
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", p.display()))
    }

    /// The ways the claim can name its speaker: each role, the whole
    /// `asserted_by`, and each of its distinctive words (a surname).
    fn speaker_names(&self, asserted_by: Option<&str>) -> Vec<Vec<String>> {
        let mut names: Vec<Vec<String>> = self.speakers.iter().map(|s| tokens(s)).collect();
        let by = tokens(asserted_by.unwrap_or(""));
        if !by.is_empty() {
            names.extend(
                by.iter()
                    .filter(|w| w.chars().count() >= 4 && !self.name_stopwords.contains(w))
                    .map(|w| vec![w.clone()]),
            );
            names.push(by);
        }
        names.retain(|n| !n.is_empty());
        names
    }

    /// Whether the claim is only that someone in the recording said it.
    pub fn is_said_here(&self, c: &ClaimFacts) -> bool {
        if c.kind != Some("attribution")
            || !c
                .source_context
                .is_some_and(|s| self.recordings.iter().any(|r| r == s))
        {
            return false;
        }
        let words = tokens(c.text);
        self.speaker_names(c.asserted_by).iter().any(|name| {
            (0..=self.lead_words).any(|start| {
                words.get(start..start + name.len()) == Some(name.as_slice())
                    && words
                        .iter()
                        .skip(start + name.len())
                        .take(self.verb_window)
                        .any(|w| self.verbs.contains(w))
            })
        })
    }
}

/// The bundle's rules, read once. An unreadable file is logged loudly and
/// leaves the check off (every claim is verified, as before).
pub fn rules() -> &'static SaidHereRules {
    static RULES: OnceLock<SaidHereRules> = OnceLock::new();
    RULES.get_or_init(|| match SaidHereRules::load_from(&crate::bundle_root()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "epistemics: said-here-claims.yaml unusable; said-here claims are verified like any other");
            SaidHereRules::default()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> SaidHereRules {
        SaidHereRules::load_from(&crate::bundle_root()).expect("said-here-claims.yaml parses")
    }

    fn video<'a>(text: &'a str, by: &'a str) -> ClaimFacts<'a> {
        ClaimFacts {
            text,
            kind: Some("attribution"),
            asserted_by: Some(by),
            source_context: Some("video_learning"),
        }
    }

    #[test]
    fn a_speakers_own_words_in_the_recording_are_said_here() {
        let r = shipped();
        assert!(r.is_said_here(&video(
            "Louis Rossmann notes there is confusion over the keywords 'digital game you already own'.",
            "Louis Rossmann"
        )));
        // By surname alone, and by a role.
        assert!(r.is_said_here(&video(
            "Baron Cohen responded by suggesting that in many instances the people are not fools.",
            "Baron Cohen"
        )));
        assert!(r.is_said_here(&video(
            "The interviewee suggested that describing Putin as a humanitarian might be applicable.",
            "C-SPAN"
        )));
    }

    #[test]
    fn reported_speech_facts_and_articles_are_still_verified() {
        let r = shipped();
        // The subject is not the speaker.
        assert!(!r.is_said_here(&video(
            "Federal investigators suspect Omar al-Bayoumi's video indicates foreknowledge of targets.",
            "60 Minutes"
        )));
        // A checkable fact a speaker states is an event, not an attribution.
        let mut fact = video(
            "The interviewee stated that Iran's inflation rate is 301%.",
            "C-SPAN",
        );
        fact.kind = Some("event");
        assert!(!r.is_said_here(&fact));
        // An article reports speech made elsewhere: not proven by the source.
        let mut article = video(
            "Mark Warner (D-VA) confirmed that Russia has spread disinformation since 2016.",
            "Mark Warner (D-VA)",
        );
        article.source_context = Some("article_learning");
        assert!(!r.is_said_here(&article));
        // A stopword of the outlet's name is not its speaker.
        assert!(!r.is_said_here(&video(
            "With prices rising, the agency said exports would be capped.",
            "Channel 5 with Andrew Callaghan"
        )));
    }
}
