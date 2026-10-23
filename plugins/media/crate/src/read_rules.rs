//! Reading rules: the URL to fetch instead of the one handed over
//! (`read-rules.yaml`), e.g. an arXiv abstract page → the full paper. Loaded
//! the way `said-here-claims.yaml` is in epistemics. Drafted by the brain's
//! deep-diver (gpt-oss, 2026-10-23), finished in review.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

pub const FILE: &str = "read-rules.yaml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRule {
    pub name: String,
    pub pattern: String,
    pub read: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRules {
    pub rules: Vec<ReadRule>,
    /// Each rule's pattern, compiled once at load, in rule order.
    #[serde(skip)]
    compiled: Vec<Regex>,
}

impl ReadRules {
    /// Parse the file; a pattern that does not compile fails it, naming the rule.
    pub fn parse(text: &str) -> Result<Self> {
        let mut rules: ReadRules = serde_yaml::from_str(text)?;
        rules.compiled = rules
            .rules
            .iter()
            .map(|rule| {
                RegexBuilder::new(&rule.pattern)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| format!("rule `{}`: pattern does not compile", rule.name))
            })
            .collect::<Result<_>>()?;
        Ok(rules)
    }

    pub fn load_from(dir: &Path) -> Result<Self> {
        let p = dir.join(FILE);
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", p.display()))
    }

    /// The first matching rule's `read`, its captures expanded; `None` when no
    /// rule matches.
    pub fn reading_url(&self, url: &str) -> Option<String> {
        self.rules
            .iter()
            .zip(&self.compiled)
            .find_map(|(rule, re)| {
                re.captures(url).map(|caps| {
                    let mut out = String::new();
                    caps.expand(&rule.read, &mut out);
                    out
                })
            })
    }
}

/// The bundle's rules, loaded once. A bad file is logged and reads nothing
/// differently.
pub fn rules() -> &'static ReadRules {
    static RULES: OnceLock<ReadRules> = OnceLock::new();
    RULES.get_or_init(|| match ReadRules::load_from(&crate::bundle_root()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "media: read-rules.yaml unusable; every page is read at the URL given");
            ReadRules::default()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> ReadRules {
        ReadRules::load_from(&crate::bundle_root()).expect("read-rules.yaml parses")
    }

    #[test]
    fn an_arxiv_abstract_is_read_as_the_full_paper() {
        let r = shipped();
        assert_eq!(
            r.reading_url("https://arxiv.org/abs/2610.14224").as_deref(),
            Some("https://arxiv.org/html/2610.14224")
        );
        assert_eq!(
            r.reading_url("HTTPS://www.arXiv.org/abs/2610.14224v2?context=cs")
                .as_deref(),
            Some("https://arxiv.org/html/2610.14224v2")
        );
        assert_eq!(r.reading_url("https://arxiv.org/html/2610.14224"), None);
        assert_eq!(r.reading_url("https://example.com/abs/1"), None);
    }

    #[test]
    fn a_pattern_that_does_not_compile_names_its_rule() {
        let err = ReadRules::parse("rules:\n  - name: broken\n    pattern: '('\n    read: x\n")
            .unwrap_err();
        assert!(format!("{err:#}").contains("broken"), "{err:#}");
    }
}
