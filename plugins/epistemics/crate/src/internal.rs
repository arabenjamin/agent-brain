//! Claims about the brain itself, and the sections of a source that describe
//! it. The brain's own ledger answers "the brain logged 37 dispatches"; a web
//! search cannot, so such a claim is never extracted and never verified
//! (owner, 2026-10-16: "we shouldn't be doing websearches on internal activity
//! or processes"). The words are the bundle's `internal-claims.yaml`.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const FILE: &str = "internal-claims.yaml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalRules {
    pub self_names: Vec<String>,
    pub verb_window: usize,
    pub verbs: Vec<String>,
    pub nouns: Vec<String>,
}

/// Lowercase, every dash variant as `-`, punctuation other than `-` as space.
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            c if c.is_alphanumeric() || c == '-' => c,
            _ => ' ',
        })
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

impl InternalRules {
    pub fn parse(text: &str) -> Result<Self> {
        let r: InternalRules = serde_yaml::from_str(text)?;
        anyhow::ensure!(
            !r.self_names.is_empty() && !r.verbs.is_empty() && !r.nouns.is_empty(),
            "self_names, verbs and nouns must each list something"
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

    /// Whether the claim is about the brain's own activity.
    pub fn is_internal(&self, claim: &str) -> bool {
        let words = tokens(claim);
        let joined = format!(" {} ", words.join(" "));
        let has_noun = self
            .nouns
            .iter()
            .any(|n| joined.contains(&format!(" {}", tokens(n).join(" "))));
        if !has_noun {
            return false;
        }
        self.self_names.iter().any(|name| {
            let name = tokens(name);
            (0..words.len()).any(|i| {
                words[i..].starts_with(&name)
                    && words
                        .iter()
                        .skip(i + name.len())
                        .take(self.verb_window)
                        .any(|w| self.verbs.iter().any(|v| v == w))
            })
        })
    }
}

/// The bundle's rules, read once. An unreadable file is logged loudly and
/// leaves the check off (nothing is dropped) rather than failing extraction.
pub fn rules() -> &'static InternalRules {
    static RULES: OnceLock<InternalRules> = OnceLock::new();
    RULES.get_or_init(|| match InternalRules::load_from(&crate::bundle_root()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "epistemics: internal-claims.yaml unusable; internal claims are NOT filtered");
            InternalRules::default()
        }
    })
}

/// A Markdown heading's words: `#`s, emphasis and surrounding space removed,
/// uppercased. `None` for a line that is not a heading.
fn heading(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let words = t.trim_start_matches('#');
    if !words.starts_with(' ') && !words.is_empty() {
        return None; // `#hashtag`, not a heading
    }
    Some(words.replace(['*', '_'], "").trim().to_uppercase())
}

/// The text without the sections whose heading starts with one of
/// `prefixes` (case-insensitive). A section runs to the next heading of any
/// level: briefs mix `##` and `###` for sibling sections, so "until the next
/// heading of the same level" swallowed the news under the summary. Returns
/// the text and the headings removed.
pub fn strip_sections(text: &str, prefixes: &[String]) -> (String, Vec<String>) {
    if prefixes.is_empty() {
        return (text.to_string(), vec![]);
    }
    let wanted: Vec<String> = prefixes
        .iter()
        .map(|p| p.replace(['*', '_', '#'], "").trim().to_uppercase())
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = String::new();
    let mut removed = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if let Some(h) = heading(line) {
            skipping = wanted.iter().any(|p| h.starts_with(p));
            if skipping {
                removed.push(h);
                continue;
            }
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> InternalRules {
        InternalRules::load_from(&crate::bundle_root()).expect("internal-claims.yaml parses")
    }

    #[test]
    fn the_briefs_self_reports_are_internal() {
        let r = shipped();
        for c in [
            "The brain logged 37 dispatches of scheduler activity in the last 24 h.",
            "The Brain processed 295 distinct notes in 24 hours, with 89 notes focused on video learning",
            "The system described in the Brain activity section logged 76 distinct notes in the last 24 h.",
            "The brain recorded a dead\u{2011}job failure when attempting to ingest a live\u{2011}event YouTube video.",
            "The code base was updated to version 5345d97, and the brain entered sleep mode at 14:34 UTC.",
            "The brain watched 104 videos.",
            "The brain filled 42 knowledge gaps in the last 24 h",
            "The brain generated 32 claim-evidence pairs in the last 24 hours.",
            "The system recorded 2208 completed tasks in the last 24 hours.",
        ] {
            assert!(r.is_internal(c), "{c}");
        }
    }

    #[test]
    fn claims_about_brains_are_not() {
        let r = shipped();
        for c in [
            "Hasan Minhaj states that the brain actively establishes a target level for body fat.",
            "The brain processed visual input faster after training, the study found.",
            "The code knowledge graph described in the DeepLearningAI video remains dynamic.",
            "Researchers recorded activity in the brain of 40 patients.",
            "Barry County Sheriff Dar Leaf accused Michigan's voter-registration system of enrolling non-citizens.",
        ] {
            assert!(!r.is_internal(c), "{c}");
        }
    }

    #[test]
    fn self_sections_are_cut_up_to_the_next_heading_of_any_level() {
        let brief = "# **DAILY BRIEF for 2026-10-14**\n\
                     ## EXECUTIVE SUMMARY for 2026-10-15\n- The brain logged 176 notes.\n\
                     ### WHAT I DID & LEARNED (last 24h)\n- watched 40 videos\n\
                     ## **SYSTEM STATUS**\n- 3 failed jobs\n\
                     ### YOUR ITEMS\n- renew passport\n\
                     ### METRO DETROIT\n- Detroit council votes on budget.\n\
                     #hashtag stays\n";
        let prefixes: Vec<String> = [
            "EXECUTIVE SUMMARY",
            "WHAT I DID",
            "SYSTEM STATUS",
            "**Your Items**",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (text, removed) = strip_sections(brief, &prefixes);
        assert_eq!(removed.len(), 4);
        assert!(text.contains("DAILY BRIEF"));
        assert!(text.contains("Detroit council votes"));
        assert!(text.contains("#hashtag stays"));
        for gone in ["176 notes", "40 videos", "failed jobs", "passport"] {
            assert!(!text.contains(gone), "{gone}");
        }
    }

    #[test]
    fn no_prefixes_leaves_the_text_alone() {
        assert_eq!(strip_sections("## A\nx", &[]).0, "## A\nx");
    }
}

/// Dry run over an export: `CLAIMS_JSON=<neo4j_query rows {id, content}>
/// cargo test -p agent-brain-plugin-epistemics internal_dry_run -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn internal_dry_run() {
    let path = std::env::var("CLAIMS_JSON").expect("CLAIMS_JSON");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let r = InternalRules::load_from(&crate::bundle_root()).unwrap();
    for row in v["rows"].as_array().unwrap() {
        let c = row["content"].as_str().unwrap_or("");
        if r.is_internal(c) {
            println!("INTERNAL\t{}\t{}", row["id"].as_str().unwrap_or(""), c);
        }
    }
}
