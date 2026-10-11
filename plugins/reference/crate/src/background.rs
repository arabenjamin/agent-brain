//! `wiki_background`: Wikipedia as the first step of researching a question
//! (HBI plan Phase 21, §12.4).
//!
//! - **Search the subjects, not the question.** Measured on 20 gap questions
//!   (§12.1), the question text as a Wikipedia query found nothing for 7 and
//!   a relevant top hit for about 5. So the model names the question's
//!   subjects ("Strait of Hormuz"), and each is searched on its own.
//! - **Picked, never the top hit**, as in Phase 18: Wikidata's candidates with
//!   an English article, and the model chooses one or none.
//! - **Background, not the answer.** It returns each article's lead, the
//!   section that best matches the question, and the domains the article
//!   cites most; the web search still runs. Wikipedia lags recent events.
//! - **Same exposure as `search_web`**, which already sends the question to
//!   search engines: Wikimedia gets only the subject terms.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::wiki;
use agent_brain_plugin_api::Llm as LlmProvider;

const MAX_SUBJECTS: usize = 3;
const LEAD_CHARS: usize = 1200;
const SECTION_CHARS: usize = 1500;
const MAX_DOMAINS: usize = 8;
/// The condensed background's budget (`condense`).
pub const CONDENSED_CHARS: usize = 1500;
/// Sections that are lists of links, not text about the subject.
const SKIP_SECTIONS: &[&str] = &[
    "references",
    "see also",
    "external links",
    "notes",
    "further reading",
    "bibliography",
    "sources",
    "citations",
];
const STOP: &[&str] = &[
    "what",
    "which",
    "when",
    "where",
    "who",
    "whom",
    "whose",
    "why",
    "how",
    "does",
    "did",
    "the",
    "and",
    "for",
    "are",
    "was",
    "were",
    "with",
    "that",
    "this",
    "from",
    "into",
    "about",
    "their",
    "there",
    "have",
    "has",
    "been",
    "between",
    "current",
    "specific",
    "specifically",
    "typical",
    "research",
    "investigate",
    "examine",
    "latest",
    "known",
    "used",
    "using",
    "used",
    "most",
];

#[derive(Debug, Clone, Serialize)]
pub struct Article {
    pub title: String,
    pub url: String,
    pub lead: String,
    pub section: Option<(String, String)>,
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Subject {
    pub term: String,
    pub article: Option<Article>,
    /// Why the model picked the article, or why none fit.
    pub why: Option<String>,
}

// ─── The model's part (pure) ──────────────────────────────────────────────────

pub fn subjects_prompt(question: &str) -> String {
    format!(
        "A researcher will read Wikipedia before searching the web for this question:\n\n{question}\n\n\
         Name the 1 to {MAX_SUBJECTS} NAMED things the question is about that could each have their own Wikipedia \
         article: a specific person, place, organisation, event, law, product or programme. Use the name an \
         encyclopedia would use (\"Strait of Hormuz\", not \"Hormuz trade disruption\"). Not general concepts \
         (\"border control\", \"inflation\", \"national debt\"), not the question's angle (\"impact\", \"risks\"), \
         and nothing too new or too narrow to have an article. Real things only: no fictional characters, \
         franchises or works (\"Wolverine\", \"The Terminator\") unless the question asks about that fiction. \
         If nothing qualifies, give none.\n\
         Output ONLY JSON: {{\"subjects\": [\"...\"]}}"
    )
}

pub fn parse_subjects(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v["subjects"].as_array().into_iter().flatten() {
        if let Some(t) = s.as_str().map(str::trim)
            && t.chars().count() >= 2
            && t.chars().count() <= 80
            && !out.iter().any(|o| o.eq_ignore_ascii_case(t))
        {
            out.push(t.to_string());
        }
    }
    out.truncate(MAX_SUBJECTS);
    out
}

pub fn pick_prompt(term: &str, question: &str, cands: &[wiki::Item]) -> String {
    let list: String = cands
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let desc = if c.description.is_empty() {
                "(no description)"
            } else {
                &c.description
            };
            format!("{}. {} — {}\n", i + 1, c.label, desc)
        })
        .collect();
    format!(
        "Which Wikipedia article is \"{term}\", as this question means it?\n\nQUESTION: {question}\n\n\
         CANDIDATES:\n{list}\n\
         Rules:\n\
         - The article must be about \"{term}\" itself, as the question uses it, not a related topic.\n\
         - If none of them is it, answer 0. A wrong pick is worse than 0.\n\
         - Output ONLY JSON: {{\"pick\": <number or 0>, \"why\": \"<one short sentence>\"}}"
    )
}

/// The question's content words, for choosing a section.
fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() > 3 && !STOP.contains(&w.as_str()))
        .collect()
}

/// Split a plain-text extract into its lead and `(heading, body)` sections.
pub fn sections(extract: &str) -> (String, Vec<(String, String)>) {
    let mut lead = String::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for line in extract.lines() {
        let t = line.trim();
        if t.starts_with("==") && t.ends_with("==") && t.len() > 4 {
            out.push((t.trim_matches('=').trim().to_string(), String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        } else {
            lead.push_str(line);
            lead.push('\n');
        }
    }
    (lead.trim().to_string(), out)
}

/// The section sharing most content words with the question (a heading word
/// counts three times), or `None` when none shares any. Words of the article's
/// own title don't count: every section is about the subject, and on the
/// smoke test "guajira" picked a 1769 rebellion for a question about today's
/// border.
pub fn best_section(
    question: &str,
    title: &str,
    secs: &[(String, String)],
) -> Option<(String, String)> {
    let own = words(title);
    let q: HashSet<String> = words(question).difference(&own).cloned().collect();
    secs.iter()
        .filter(|(h, b)| {
            !SKIP_SECTIONS.contains(&h.to_lowercase().as_str()) && !b.trim().is_empty()
        })
        .map(|(h, b)| {
            let hs = words(h).intersection(&q).count() * 3;
            let bs = words(b).intersection(&q).count();
            (hs + bs, h, b)
        })
        .filter(|(s, _, _)| *s > 0)
        .max_by_key(|(s, _, _)| *s)
        .map(|(_, h, b)| (h.clone(), clip(b.trim(), SECTION_CHARS)))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    let at = cut.rfind(". ").map(|i| i + 1).unwrap_or(cut.len());
    format!("{} […]", &cut[..at])
}

/// The hosts an article cites most, less Wikimedia's own and archives.
pub fn top_domains(links: &[String]) -> Vec<String> {
    let mut n: HashMap<String, usize> = HashMap::new();
    for l in links {
        if let Ok(u) = url::Url::parse(l)
            && let Some(h) = u.host_str()
        {
            let h = h.trim_start_matches("www.").to_string();
            if [
                "wikipedia.org",
                "wikimedia.org",
                "wikidata.org",
                "archive.org",
                "doi.org",
                "worldcat.org",
                "books.google.com",
                "jstor.org",
            ]
            .iter()
            .any(|x| h.ends_with(x))
            {
                continue;
            }
            *n.entry(h).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = n.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(MAX_DOMAINS).map(|(h, _)| h).collect()
}

/// The text the reason step reads.
pub fn render(subjects: &[Subject]) -> String {
    render_with(subjects, true)
}

/// `domains: false` leaves out the cited-sources lines: given them, the
/// condenser invented a summary for each site ("the people.com article
/// focuses on…") on the v2 smoke test.
fn render_with(subjects: &[Subject], domains: bool) -> String {
    let mut out = String::from("## BACKGROUND — Wikipedia (encyclopedic; may lag recent events)\n");
    let mut none = Vec::new();
    for s in subjects {
        match &s.article {
            Some(a) => {
                out.push_str(&format!("\n### {} — {}\n{}\n", a.title, a.url, a.lead));
                if let Some((h, b)) = &a.section {
                    out.push_str(&format!("\n#### {h}\n{b}\n"));
                }
                if domains && !a.domains.is_empty() {
                    out.push_str(&format!(
                        "\nSources this article cites most: {}\n",
                        a.domains.join(", ")
                    ));
                }
            }
            None => none.push(s.term.clone()),
        }
    }
    if subjects.is_empty() {
        out.push_str("\nThe question names nothing with its own Wikipedia article.\n");
    } else if !none.is_empty() {
        out.push_str(&format!(
            "\nNo Wikipedia article found for: {}\n",
            none.join(", ")
        ));
    }
    out
}

/// Condensing the background so it survives the chain's handoff (Phase 21
/// v2): in the v1 replay the full text was squeezed out of every answer.
pub fn condense_prompt(question: &str, background: &str) -> String {
    format!(
        "QUESTION: {question}\n\n{background}\n\n\
         The background above has one or more Wikipedia articles, each starting with a \"### Title — URL\" line. \
         For each article, write at most 3 bullets: facts stated in that article's text that help answer the \
         QUESTION (names, dates, figures, mechanisms). End each bullet with that article's en.wikipedia.org URL \
         in parentheses. If an article states nothing that helps, write one bullet saying what the subject is. \
         Use only sentences in the background; add nothing from memory. At most {CONDENSED_CHARS} characters. \
         Output only the bullets."
    )
}

/// The condensed bullets under the section header, with the articles that
/// had none listed, clipped to the budget.
pub fn condensed(bullets: &str, subjects: &[Subject]) -> String {
    let mut out = String::from("## BACKGROUND — Wikipedia (encyclopedic; may lag recent events)\n");
    out.push_str(&clip(bullets.trim(), CONDENSED_CHARS));
    out.push('\n');
    for a in subjects.iter().filter_map(|s| s.article.as_ref()) {
        if !a.domains.is_empty() {
            out.push_str(&format!(
                "Sources {} cites most: {}\n",
                a.title,
                a.domains.join(", ")
            ));
        }
    }
    let none: Vec<&str> = subjects
        .iter()
        .filter(|s| s.article.is_none())
        .map(|s| s.term.as_str())
        .collect();
    if !none.is_empty() {
        out.push_str(&format!(
            "No Wikipedia article found for: {}\n",
            none.join(", ")
        ));
    }
    out
}

/// The web query: the question, plus the article titles it doesn't already name.
pub fn web_query(question: &str, subjects: &[Subject]) -> String {
    let q = question.to_lowercase();
    let extra: Vec<&str> = subjects
        .iter()
        .filter_map(|s| s.article.as_ref())
        .map(|a| a.title.as_str())
        .filter(|t| !q.contains(&t.to_lowercase()))
        .collect();
    if extra.is_empty() {
        question.to_string()
    } else {
        format!("{question} {}", extra.join(" "))
    }
}

// ─── Wikimedia ────────────────────────────────────────────────────────────────

async fn article(title: &str, question: &str) -> Result<Article> {
    let v = wiki::wikipedia(&[
        ("action", "query"),
        ("prop", "extracts|extlinks"),
        ("explaintext", "1"),
        ("ellimit", "500"),
        ("redirects", "1"),
        ("titles", title),
    ])
    .await?;
    let page = &v["query"]["pages"][0];
    let title = page["title"].as_str().unwrap_or(title).to_string();
    let text = page["extract"].as_str().unwrap_or_default();
    let (lead, secs) = sections(text);
    let links: Vec<String> = page["extlinks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["url"].as_str().map(String::from))
        .collect();
    Ok(Article {
        url: wiki::article_url("https://en.wikipedia.org/wiki/", &title),
        section: best_section(question, &title, &secs),
        title,
        lead: clip(&lead, LEAD_CHARS),
        domains: top_domains(&links),
    })
}

/// Find, pick and read the articles for `question`.
pub async fn background(
    llm: &dyn LlmProvider,
    question: &str,
    condense: bool,
) -> Result<(Vec<Subject>, String, String)> {
    let terms = llm
        .generate_json(&subjects_prompt(question), None, &["subjects"], 1)
        .await
        .map(|v| parse_subjects(&v))?;
    let mut subjects = Vec::new();
    let mut seen_titles: HashSet<String> = HashSet::new();
    for term in terms {
        let cands: Vec<wiki::Item> =
            wiki::candidates_for(&wiki::name_variants(&term), &HashSet::new())
                .await?
                .into_iter()
                .filter(|c| c.wiki_title.is_some())
                .collect();
        if cands.is_empty() {
            subjects.push(Subject {
                term,
                article: None,
                why: Some("no candidate with an English article".into()),
            });
            continue;
        }
        let a = llm
            .generate_json(&pick_prompt(&term, question, &cands), None, &["pick"], 1)
            .await?;
        let why = a["why"].as_str().map(String::from);
        let Some(i) = wiki::parse_pick(&a, cands.len()) else {
            subjects.push(Subject {
                term,
                article: None,
                why,
            });
            continue;
        };
        let title = cands[i].wiki_title.clone().unwrap_or_default();
        if !seen_titles.insert(title.clone()) {
            continue; // two subjects, one article
        }
        let art = article(&title, question).await?;
        subjects.push(Subject {
            term,
            article: Some(art),
            why,
        });
    }
    let mut text = render(&subjects);
    if condense && subjects.iter().any(|s| s.article.is_some()) {
        let c = llm
            .generate(
                &condense_prompt(question, &render_with(&subjects, false)),
                None,
            )
            .await?;
        text = condensed(&c, &subjects);
    }
    let query = web_query(question, &subjects);
    Ok((subjects, text, query))
}

/// The tool's JSON: `text` for the reasoner, `query` for the web search.
pub fn result_json(subjects: &[Subject], text: &str, query: &str, banked: bool) -> Value {
    json!({
        "text": text,
        "query": query,
        "articles": subjects.iter().filter(|s| s.article.is_some()).count(),
        "subjects": subjects.iter().map(|s| json!({
            "term": s.term,
            "title": s.article.as_ref().map(|a| a.title.clone()),
            "url": s.article.as_ref().map(|a| a.url.clone()),
            "section": s.article.as_ref().and_then(|a| a.section.as_ref().map(|x| x.0.clone())),
            "why": s.why,
        })).collect::<Vec<_>>(),
        "banked": banked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subjects_prompt_asks_for_real_named_things() {
        // v2 replay: "creatures that survive 7.62 rounds" got Wolverine,
        // the Terminator and Doomsday as subjects, and the answer cited them.
        let p = subjects_prompt("What creatures survive 7.62 rounds?");
        assert!(p.contains("Real things only"));
        assert!(p.contains("Not general concepts"));
    }

    #[test]
    fn subjects_are_trimmed_deduplicated_and_capped() {
        let v = json!({"subjects": [" Strait of Hormuz ", "strait of hormuz", "x", "Food security", "Harvest", "Iran"]});
        assert_eq!(
            parse_subjects(&v),
            vec!["Strait of Hormuz", "Food security", "Harvest"]
        );
        assert!(parse_subjects(&json!({"subjects": []})).is_empty());
    }

    #[test]
    fn the_matching_section_wins_and_reference_lists_never_do() {
        let text = "Lead about the strait.\n\n== History ==\nOld times.\n\n== Shipping ==\nOil tankers and trade disruption raise food prices.\n\n== References ==\ntrade disruption food prices hunger harvest";
        let (lead, secs) = sections(text);
        assert_eq!(lead, "Lead about the strait.");
        let (h, b) = best_section(
            "How does trade disruption in Hormuz affect food prices?",
            "Strait of Hormuz",
            &secs,
        )
        .unwrap();
        assert_eq!(h, "Shipping");
        assert!(b.contains("tankers"));
        assert!(best_section("quantum chromodynamics", "Strait of Hormuz", &secs).is_none());
        // The subject's own name matches every section, so it doesn't count.
        let secs = vec![(
            "The Guajira rebellion".to_string(),
            "Wayuu revolts.".to_string(),
        )];
        assert!(
            best_section(
                "border control in the Guajira region",
                "La Guajira Department",
                &secs
            )
            .is_none()
        );
    }

    #[test]
    fn the_condensed_background_keeps_its_header_and_names_what_had_no_article() {
        let s = vec![Subject {
            term: "Keh clan".into(),
            article: None,
            why: None,
        }];
        let long = "- fact (https://en.wikipedia.org/wiki/X)\n".repeat(100);
        let c = condensed(&long, &s);
        assert!(c.starts_with("## BACKGROUND — Wikipedia"));
        assert!(c.contains("No Wikipedia article found for: Keh clan"));
        assert!(
            c.chars().count() < CONDENSED_CHARS + 200,
            "clipped to the budget"
        );
    }

    #[test]
    fn domains_are_counted_without_wikimedia_or_archives() {
        let links: Vec<String> = [
            "https://www.reuters.com/a",
            "https://reuters.com/b",
            "https://web.archive.org/x",
            "https://en.wikipedia.org/wiki/Y",
            "https://www.eia.gov/z",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(top_domains(&links), vec!["reuters.com", "eia.gov"]);
    }

    #[test]
    fn the_web_query_adds_titles_the_question_lacks() {
        let s = vec![Subject {
            term: "Hormuz".into(),
            article: Some(Article {
                title: "Strait of Hormuz".into(),
                url: String::new(),
                lead: String::new(),
                section: None,
                domains: vec![],
            }),
            why: None,
        }];
        assert_eq!(
            web_query("trade disruption in Hormuz", &s),
            "trade disruption in Hormuz Strait of Hormuz"
        );
        assert_eq!(
            web_query("the Strait of Hormuz today", &s),
            "the Strait of Hormuz today"
        );
        let none = vec![Subject {
            term: "Keh clan".into(),
            article: None,
            why: None,
        }];
        assert!(render(&none).contains("No Wikipedia article found for: Keh clan"));
    }
}
