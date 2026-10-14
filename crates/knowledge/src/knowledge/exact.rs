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
                    .trim_end_matches(['.', ',', ';', ':', '!', '?', '*'])
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

// ── Checking output ─────────────────────────────────────────────────────────

/// Appended after a link that is not in the text the model was given.
pub const NOT_IN_SOURCES: &str = " [link not in its sources]";

/// What [`check_links`] found, and the text with it applied.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct LinkCheck {
    pub text: String,
    /// Note ids the model wrote as URLs (`https://<id>`), now `note <id>`.
    pub relinked: Vec<String>,
    /// Links in no source it was given, each now marked [`NOT_IN_SOURCES`].
    pub not_in_sources: Vec<String>,
}

impl LinkCheck {
    pub fn changed(&self) -> bool {
        !self.relinked.is_empty() || !self.not_in_sources.is_empty()
    }

    /// The finding as JSON for a tool result, or `None` when there is none.
    pub fn report(&self) -> Option<serde_json::Value> {
        self.changed().then(|| {
            serde_json::json!({
                "relinked_note_ids": self.relinked,
                "links_not_in_sources": self.not_in_sources,
            })
        })
    }
}

fn uuid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
            .expect("valid")
    })
}

impl ExactPatterns {
    /// Check every link in `text` against `evidence`, the text the model was
    /// given (its prompt, or a chat turn's messages and tool results).
    ///
    /// Measured on 795 October `reason` answers (Phase R step 3): 1,913 cited
    /// links, ~27 in no source, nearly all wrong: note ids written as URLs
    /// (`https://fd683db5-…`), a site plus a date (`militarytimes.com/2022-11-01`),
    /// slugs changed on the way through. A link the evidence holds is left
    /// alone; a note id the evidence holds becomes `note <id>`; anything else
    /// is kept and marked, never deleted.
    pub fn check_links(&self, text: &str, evidence: &str) -> LinkCheck {
        let mut out = LinkCheck {
            text: text.to_string(),
            ..Default::default()
        };
        let Some(url) = self.patterns.iter().find(|p| p.name == "url") else {
            return out;
        };
        let ev = evidence.to_lowercase().replace("\\/", "/");
        let mut found = url.spans(text);
        // Edit from the end so earlier offsets stay valid.
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        for (start, end, link) in found {
            let key = link.to_lowercase();
            if ev.contains(key.trim_end_matches('/')) {
                continue;
            }
            if let Some(id) = uuid_re().find(&link).map(|m| m.as_str().to_lowercase())
                && ev.contains(&id)
            {
                out.text.replace_range(start..end, &format!("note {id}"));
                out.relinked.push(id);
                continue;
            }
            // After the whole markdown link, so it stays one: past `](…)` when
            // the URL is the link's text, else past its own closing bracket.
            let rest = &out.text[end..];
            let at = if rest.starts_with("](")
                && let Some(close) = rest.find(')')
            {
                end + close + 1
            } else if let Some(c) = rest.chars().next()
                && matches!(c, ')' | ']' | '】' | '>')
            {
                end + c.len_utf8()
            } else {
                end
            };
            // One mark where a link's text and target are both unsupported.
            if !out.text[at..].starts_with(NOT_IN_SOURCES) {
                out.text.insert_str(at, NOT_IN_SOURCES);
            }
            if !out.not_in_sources.contains(&link) {
                out.not_in_sources.push(link);
            }
        }
        out.relinked.reverse();
        out.relinked.dedup();
        out.not_in_sources.reverse();
        out
    }
}

/// Check an answer and its inferences against `evidence`: both rewritten,
/// and every finding together.
pub fn check_answer(
    answer: &str,
    inferences: &[String],
    evidence: &str,
) -> (String, Vec<String>, LinkCheck) {
    let p = ExactPatterns::global();
    let a = p.check_links(answer, evidence);
    let mut all = LinkCheck {
        relinked: a.relinked.clone(),
        not_in_sources: a.not_in_sources.clone(),
        ..Default::default()
    };
    let infs = inferences
        .iter()
        .map(|i| {
            let c = p.check_links(i, evidence);
            all.relinked.extend(c.relinked);
            all.not_in_sources.extend(c.not_in_sources);
            c.text
        })
        .collect();
    all.relinked.sort();
    all.relinked.dedup();
    all.not_in_sources.sort();
    all.not_in_sources.dedup();
    if all.changed() {
        warn!(relinked = all.relinked.len(), not_in_sources = ?all.not_in_sources,
              "reason: links checked against the prompt");
    }
    (a.text, infs, all)
}

/// [`ExactPatterns::check_links`] with the loaded library.
pub fn check_links(text: &str, evidence: &str) -> LinkCheck {
    ExactPatterns::global().check_links(text, evidence)
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
    fn a_link_in_its_sources_stands_and_one_that_is_not_is_marked() {
        let p = core();
        let ev = "CONTEXT: [note fd683db5-f172-45f0-b42a-bd7440a9ac28] HITL matters. \
                  Source: https://www.pbs.org/newshour/a-story";
        let c = p.check_links(
            "HITL (https://fd683db5-f172-45f0-b42a-bd7440a9ac28). PBS: [story](https://www.pbs.org/newshour/a-story). \
             Giant (https://www.militarytimes.com/2022-11-01).",
            ev,
        );
        assert_eq!(
            c.text,
            "HITL (note fd683db5-f172-45f0-b42a-bd7440a9ac28). PBS: [story](https://www.pbs.org/newshour/a-story). \
             Giant (https://www.militarytimes.com/2022-11-01) [link not in its sources]."
        );
        assert_eq!(c.relinked, ["fd683db5-f172-45f0-b42a-bd7440a9ac28"]);
        assert_eq!(
            c.not_in_sources,
            ["https://www.militarytimes.com/2022-11-01"]
        );
        // A trailing slash, case and markdown emphasis are not differences.
        let same = p.check_links("**https://WWW.pbs.org/newshour/a-story/**", ev);
        assert!(!same.changed(), "{same:?}");
        // A URL that is a markdown link's text: marked after the whole link.
        let md = p.check_links("([https://https://x.org/a](https://x.org/a))", ev);
        assert_eq!(
            md.text,
            "([https://https://x.org/a](https://x.org/a) [link not in its sources])"
        );
        // A note id the evidence does not hold is not a note: marked.
        let stray = p.check_links("see https://11111111-2222-3333-4444-555555555555", ev);
        assert!(stray.relinked.is_empty() && stray.not_in_sources.len() == 1);
    }

    /// Replay logged `reason` calls (a DuckDB export of `interactions`, one
    /// JSON object per line with `prompt` and `response`) through the link
    /// check and print what it would do. Measurement, not a gate:
    /// `PHASE_R_REPLAY=path cargo test -p agent-brain-knowledge replay -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn replay_logged_reason_calls() {
        let Ok(path) = std::env::var("PHASE_R_REPLAY") else {
            return;
        };
        let p = core();
        let (mut answers, mut links, mut changed) = (0usize, 0usize, 0usize);
        let (mut relinked, mut marked) = (Vec::new(), Vec::new());
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let Some(args) = row["prompt"]
                .as_str()
                .and_then(|p| p.strip_prefix("reason: "))
            else {
                continue;
            };
            let Ok(args) = serde_json::from_str::<serde_json::Value>(args) else {
                continue;
            };
            let ctx = match &args["context"] {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let evidence = format!("{}\n{ctx}", args["question"].as_str().unwrap_or(""));
            let resp = row["response"].as_str().unwrap_or("");
            let answer = serde_json::from_str::<serde_json::Value>(resp)
                .ok()
                .and_then(|v| v["answer"].as_str().map(String::from))
                .unwrap_or_else(|| resp.to_string());
            answers += 1;
            links += p
                .patterns
                .iter()
                .find(|x| x.name == "url")
                .unwrap()
                .spans(&answer)
                .len();
            let c = p.check_links(&answer, &evidence);
            if c.changed() {
                changed += 1;
            }
            relinked.extend(c.relinked);
            marked.extend(c.not_in_sources);
        }
        println!("{answers} answers, {links} links; {changed} answers changed");
        println!("relinked note ids: {}", relinked.len());
        println!("marked not in sources: {}", marked.len());
        for m in &marked {
            println!("  {m}");
        }
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
