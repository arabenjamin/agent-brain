//! Exact spans: the strings a question names that must be found as written
//! (money, dates, codes, versions, model names, URLs, quoted text).
//!
//! The vector leg blurs numbers and codes, and BM25 reads "$17.3 billion" as
//! the loose words "17.3" OR "billion", so a note holding the exact string
//! ranked below every note about billions: 0.28 recall inside a question,
//! against 0.95 for the same string as a Lucene phrase (GRAPH_CONTEXT_PLAN
//! Phase R step 1). Spans come from a vetted pattern library (bundle seed dir
//! `exact-patterns/`), each pattern tested against its own examples at load;
//! no model writes one.

use super::*;
use regex::Regex;
use serde::Deserialize;
use std::sync::{Arc, OnceLock};

/// Spans taken from one query; more is a list of words, not a lookup.
pub const MAX_SPANS: usize = 4;
/// Phrase hits read per span before the literal check drops the near ones.
const PHRASE_POOL: i64 = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternFile {
    patterns: Vec<PatternDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternDef {
    name: String,
    pattern: String,
    /// A capture group: a number, or `any` for the first that took part.
    #[serde(default)]
    group: Option<serde_yaml::Value>,
    #[serde(default)]
    matches: Vec<Example>,
    #[serde(default)]
    rejects: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Example {
    text: String,
    span: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Group {
    Index(usize),
    Any,
}

#[derive(Debug, Clone)]
pub struct ExactPattern {
    pub name: String,
    re: Regex,
    group: Group,
}

/// One span found in a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub pattern: String,
}

impl ExactPattern {
    fn spans(&self, text: &str) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        for c in self.re.captures_iter(text) {
            let m = match self.group {
                Group::Index(i) => c.get(i),
                Group::Any => (1..c.len()).find_map(|i| c.get(i)).or_else(|| c.get(0)),
            };
            if let Some(m) = m {
                let s = m
                    .as_str()
                    .trim_end_matches(['.', ',', ';', ':', '!', '?'])
                    .trim();
                if s.chars().count() >= 3 {
                    out.push((m.start(), m.start() + s.len(), s.to_string()));
                }
            }
        }
        out
    }

    /// Compile and test one definition against its own examples.
    fn build(d: &PatternDef) -> Result<Self, String> {
        let re = Regex::new(&d.pattern).map_err(|e| format!("pattern: {e}"))?;
        let group = match &d.group {
            None => Group::Index(0),
            Some(serde_yaml::Value::String(s)) if s == "any" => Group::Any,
            Some(serde_yaml::Value::Number(n)) => Group::Index(
                n.as_u64()
                    .ok_or_else(|| format!("group {n} is not a group number"))?
                    as usize,
            ),
            Some(other) => return Err(format!("group {other:?}: a number or `any`")),
        };
        let p = ExactPattern {
            name: d.name.clone(),
            re,
            group,
        };
        if d.matches.is_empty() {
            return Err("needs at least one `matches` example".into());
        }
        for e in &d.matches {
            let got: Vec<String> = p.spans(&e.text).into_iter().map(|s| s.2).collect();
            if got != [e.span.clone()] {
                return Err(format!(
                    "example {:?} should give [{:?}], gave {got:?}",
                    e.text, e.span
                ));
            }
        }
        for r in &d.rejects {
            let got: Vec<String> = p.spans(r).into_iter().map(|s| s.2).collect();
            if !got.is_empty() {
                return Err(format!("reject {r:?} gave {got:?}"));
            }
        }
        Ok(p)
    }
}

/// The loaded library.
#[derive(Debug, Clone, Default)]
pub struct ExactPatterns {
    pub patterns: Vec<ExactPattern>,
}

impl ExactPatterns {
    /// Parse a file's patterns; one that fails its examples is skipped and
    /// named in the second list.
    pub fn parse(yaml: &str) -> Result<(Vec<ExactPattern>, Vec<String>), String> {
        let f: PatternFile = serde_yaml::from_str(yaml).map_err(|e| format!("YAML: {e}"))?;
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for d in &f.patterns {
            match ExactPattern::build(d) {
                Ok(p) => ok.push(p),
                Err(e) => bad.push(format!("{}: {e}", d.name)),
            }
        }
        Ok((ok, bad))
    }

    /// Every loaded bundle's `exact-patterns/*.yaml`, core first; a name
    /// already taken is skipped.
    fn load_bundles() -> Self {
        let mut patterns: Vec<ExactPattern> = Vec::new();
        for (plugin, path) in
            agent_brain_graph::bundles::Plugins::global().files("exact-patterns", "yaml")
        {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| Self::parse(&t));
            match parsed {
                Ok((ok, bad)) => {
                    for b in bad {
                        warn!(%plugin, path = %path.display(), "exact-patterns: pattern skipped, {b}");
                    }
                    for p in ok {
                        if patterns.iter().any(|q| q.name == p.name) {
                            warn!(%plugin, name = %p.name, "exact-patterns: name already declared, skipped");
                        } else {
                            patterns.push(p);
                        }
                    }
                }
                Err(e) => {
                    warn!(%plugin, path = %path.display(), error = %e, "exact-patterns: file skipped")
                }
            }
        }
        info!(count = patterns.len(), "exact-patterns loaded");
        Self { patterns }
    }

    pub fn global() -> Arc<ExactPatterns> {
        static P: OnceLock<Arc<ExactPatterns>> = OnceLock::new();
        P.get_or_init(|| Arc::new(Self::load_bundles())).clone()
    }

    /// The spans in `query`, in the order they appear: a span inside a longer
    /// one is dropped, a repeat (any case) is dropped, at most [`MAX_SPANS`].
    pub fn spans(&self, query: &str) -> Vec<Span> {
        let mut all: Vec<(usize, usize, String, &str)> = self
            .patterns
            .iter()
            .flat_map(|p| {
                p.spans(query)
                    .into_iter()
                    .map(move |(s, e, t)| (s, e, t, p.name.as_str()))
            })
            .collect();
        // Longest first, so a span inside one already kept is dropped.
        all.sort_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)).then(a.0.cmp(&b.0)));
        let mut kept: Vec<(usize, usize, String, &str)> = Vec::new();
        for s in all {
            let inside = kept.iter().any(|k| k.0 <= s.0 && s.1 <= k.1);
            let repeat = kept
                .iter()
                .any(|k| k.2.to_lowercase() == s.2.to_lowercase());
            if !inside && !repeat {
                kept.push(s);
            }
        }
        kept.sort_by_key(|k| k.0);
        kept.truncate(MAX_SPANS);
        kept.into_iter()
            .map(|(_, _, text, pattern)| Span {
                text,
                pattern: pattern.to_string(),
            })
            .collect()
    }
}

impl KnowledgeService {
    /// Notes holding `span` as written (any case): the phrase index first
    /// (best first), kept only where the text holds the span literally, then
    /// a scan for the ones the index missed. The scan always runs: the
    /// analyzer splits a span differently in some texts (a date inside a
    /// timestamp), and the phrase alone left 6 of 54 cases short (Phase R
    /// step 2); it costs ~0.16 s.
    pub(super) async fn exact_hits(&self, span: &str, limit: usize) -> Vec<(String, String)> {
        let phrase = format!("\"{}\"", sanitize_lucene_query(span));
        let q = neo4rs::query(
            "CALL db.index.fulltext.queryNodes('note_content_fulltext', $phrase, {limit: $pool}) \
             YIELD node, score \
             WITH node, score WHERE toLower(node.content) CONTAINS toLower($span) \
             RETURN node.id AS id, node.content AS content ORDER BY score DESC LIMIT $limit",
        )
        .param("phrase", phrase)
        .param("pool", PHRASE_POOL)
        .param("span", span)
        .param("limit", limit as i64);
        let rows = |rows: Vec<neo4rs::Row>| -> Vec<(String, String)> {
            rows.into_iter()
                .filter_map(|r| {
                    Some((
                        r.get::<String>("id").ok()?,
                        r.get::<String>("content").ok()?,
                    ))
                })
                .collect()
        };
        let mut found = match self.neo4j.execute(q).await {
            Ok(r) => rows(r),
            Err(e) => {
                warn!(error = %e, "exact: phrase query failed, scanning");
                Vec::new()
            }
        };
        let scan = neo4rs::query(
            "MATCH (n:Note) WHERE toLower(n.content) CONTAINS toLower($span) \
             RETURN n.id AS id, n.content AS content \
             ORDER BY n.created_at DESC LIMIT $limit",
        )
        .param("span", span)
        .param("limit", limit as i64);
        match self.neo4j.execute(scan).await {
            Ok(r) => {
                for hit in rows(r) {
                    if found.len() >= limit {
                        break;
                    }
                    if !found.iter().any(|f| f.0 == hit.0) {
                        found.push(hit);
                    }
                }
            }
            Err(e) => warn!(error = %e, "exact: scan failed"),
        }
        found
    }
}

/// Exact hits first, then the rest of `ranked` as it was. A note holding more
/// of the spans ranks higher; among equals, its place in `ranked`, then the
/// order the spans found it.
pub(super) fn exact_first(
    ranked: Vec<(String, String)>,
    exact: Vec<Vec<(String, String)>>,
) -> (Vec<(String, String)>, HashMap<String, usize>) {
    let place: HashMap<&str, usize> = ranked
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();
    let mut held: HashMap<String, (usize, usize, String)> = HashMap::new();
    let mut seen = 0usize;
    for hits in &exact {
        for (id, content) in hits {
            let e = held.entry(id.clone()).or_insert_with(|| {
                seen += 1;
                (0, seen, content.clone())
            });
            e.0 += 1;
        }
    }
    let mut first: Vec<(String, usize, usize, usize, String)> = held
        .into_iter()
        .map(|(id, (n, order, content))| {
            let p = place.get(id.as_str()).copied().unwrap_or(usize::MAX);
            (id, n, p, order, content)
        })
        .collect();
    first.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    let counts: HashMap<String, usize> = first.iter().map(|f| (f.0.clone(), f.1)).collect();
    let mut out: Vec<(String, String)> = first.into_iter().map(|f| (f.0, f.4)).collect();
    out.extend(
        ranked
            .into_iter()
            .filter(|(id, _)| !counts.contains_key(id)),
    );
    (out, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> ExactPatterns {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/exact-patterns/core.yaml");
        let (ok, bad) = ExactPatterns::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(
            bad.is_empty(),
            "patterns failing their own examples: {bad:?}"
        );
        assert!(ok.len() >= 9, "only {} patterns", ok.len());
        ExactPatterns { patterns: ok }
    }

    fn texts(p: &ExactPatterns, q: &str) -> Vec<String> {
        p.spans(q).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn every_shipped_pattern_passes_its_examples() {
        core();
    }

    #[test]
    fn a_pattern_that_fails_its_example_is_skipped_and_named() {
        let (ok, bad) = ExactPatterns::parse(
            "patterns:\n  - name: broken\n    pattern: 'x+'\n    matches: [{text: 'yyy', span: 'yyy'}]\n",
        )
        .unwrap();
        assert!(ok.is_empty());
        assert!(bad[0].starts_with("broken: example"), "{bad:?}");
    }

    #[test]
    fn spans_keep_the_longest_and_drop_repeats() {
        let p = core();
        // The code inside the quoted span is not its own span.
        assert_eq!(
            texts(
                &p,
                r#"What is "the GBU-12 kit" and the gbu-12 price, $40,000?"#
            ),
            vec!["the GBU-12 kit", "$40,000"]
        );
        assert_eq!(
            texts(&p, "What do we know about $17.3 billion?"),
            vec!["$17.3 billion"]
        );
        assert!(texts(&p, "What have we learned today?").is_empty());
        assert!(texts(&p, "How do mesh networks route around failures?").is_empty());
    }

    #[test]
    fn at_most_four_spans() {
        let p = core();
        let q = "2026-01-01 2026-01-02 2026-01-03 2026-01-04 2026-01-05";
        assert_eq!(p.spans(q).len(), MAX_SPANS);
    }

    #[test]
    fn exact_hits_rank_first_by_how_many_spans_they_hold() {
        let ranked = vec![
            ("a".to_string(), "A".to_string()),
            ("b".to_string(), "B".to_string()),
            ("c".to_string(), "C".to_string()),
        ];
        let exact = vec![
            vec![
                ("x".to_string(), "X".to_string()),
                ("c".to_string(), "C".to_string()),
            ],
            vec![("c".to_string(), "C".to_string())],
        ];
        let (out, counts) = exact_first(ranked, exact);
        let ids: Vec<&str> = out.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["c", "x", "a", "b"]);
        assert_eq!(counts["c"], 2);
    }
}
