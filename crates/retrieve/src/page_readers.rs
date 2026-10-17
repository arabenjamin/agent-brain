//! Page readers as data: what `read_page` can use to turn a URL into text.
//!
//! Each reader is a YAML file in a bundle's `page-readers/` dir
//! (`plugins/core/page-readers/*.yaml`): a POST-JSON service (endpoint, auth,
//! where the page URL goes, fixed parameters, where the text and title come
//! back, what a spent allowance looks like) with the same `budget` block as a
//! search engine, so the same rotation spreads reads across the free
//! allowances. One reader is built in (`builtin: direct`): our own GET, free
//! and unlimited, with a crude HTML-to-text; its file says which URLs it reads
//! first (raw files, where the exact bytes beat any service).

use std::sync::OnceLock;

use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::search_engines::{Auth, Budget, budget_units, path_get, redact};

/// Text below this many characters is not a page read (a script-rendered
/// shell, a consent wall): the reader is passed over like an empty search.
pub const MIN_TEXT_CHARS: usize = 200;

/// Most bytes the direct reader downloads.
const DIRECT_MAX_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderSpec {
    pub name: String,
    /// Fallback order (lowest first) after the rotation's pick.
    #[serde(default)]
    pub rank: Option<u32>,
    #[serde(default)]
    pub key_env: Option<String>,
    #[serde(default)]
    pub budget: Option<Budget>,
    /// `direct`: our own fetch, no `request`/`response`.
    #[serde(default)]
    pub builtin: Option<Builtin>,
    /// URLs this reader takes first, ahead of the rotation.
    #[serde(default)]
    pub first_for: Option<FirstFor>,
    #[serde(default)]
    pub request: Option<ReaderRequest>,
    #[serde(default)]
    pub response: Option<ReaderResponse>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    Direct,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstFor {
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Path extensions, without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderRequest {
    /// The service endpoint (always a JSON POST).
    pub url: String,
    pub auth: Option<Auth>,
    /// Field taking the page URL as a string.
    pub url_field: Option<String>,
    /// Field taking the page URL as a one-element list.
    pub urls_field: Option<String>,
    /// Sent on every request.
    #[serde(default)]
    pub fixed: Map<String, Value>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderResponse {
    /// Dot paths tried in order; the first non-empty string is the text.
    pub text: Vec<String>,
    pub title: Option<String>,
    /// Dot paths to a per-URL failure inside a 200 answer.
    #[serde(default)]
    pub error: Vec<String>,
    /// A field that must hold `equals` for the answer to count (Anakin
    /// `status: completed`).
    pub require: Option<Require>,
    #[serde(default)]
    pub spent_status: Vec<u16>,
    #[serde(default)]
    pub spent_words: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Require {
    pub path: String,
    pub equals: Value,
}

/// A page as read.
#[derive(Debug, Clone)]
pub struct Page {
    pub title: Option<String>,
    pub text: String,
    pub units: f64,
}

pub fn parse(text: &str) -> Result<ReaderSpec, String> {
    let spec: ReaderSpec = serde_yaml::from_str(text).map_err(|e| format!("YAML: {e}"))?;
    check(&spec)?;
    Ok(spec)
}

pub fn check(spec: &ReaderSpec) -> Result<(), String> {
    if spec.name.is_empty()
        || !spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("name '{}' must be an identifier", spec.name));
    }
    match (&spec.builtin, &spec.request, &spec.response) {
        (Some(_), None, None) => {}
        (None, Some(r), Some(_)) => {
            if !r.url.starts_with("https://") && !r.url.starts_with("http://") {
                return Err(format!("url '{}' must be http(s)", r.url));
            }
            if r.url_field.is_some() == r.urls_field.is_some() {
                return Err("request takes exactly one of `url_field` or `urls_field`".into());
            }
            if spec.key_env.is_some() && r.auth.is_none() {
                return Err("a keyed reader needs `auth`".into());
            }
        }
        _ => return Err("a reader is either `builtin` or has `request` and `response`".into()),
    }
    if let Some(b) = &spec.budget
        && (b.free.is_nan() || b.free <= 0.0)
    {
        return Err("budget.free must be above zero".into());
    }
    Ok(())
}

/// Every reader from the loaded bundles, core first; a later file naming a
/// reader already loaded is skipped.
pub fn readers() -> &'static [ReaderSpec] {
    static READERS: OnceLock<Vec<ReaderSpec>> = OnceLock::new();
    READERS.get_or_init(|| {
        let plugins = agent_brain_graph::bundles::Plugins::global();
        let mut out: Vec<ReaderSpec> = Vec::new();
        for (plugin, path) in plugins.files("page-readers", "yaml") {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "page reader: cannot read");
                    continue;
                }
            };
            match parse(&text) {
                Ok(s) if out.iter().any(|r| r.name == s.name) => {
                    warn!(reader = %s.name, %plugin, "page reader declared twice; first kept");
                }
                Ok(s) => out.push(s),
                Err(e) => warn!(path = %path.display(), error = %e, "page reader skipped"),
            }
        }
        out.sort_by_key(|r| r.rank.unwrap_or(u32::MAX));
        info!(
            readers = ?out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            "Page readers loaded"
        );
        out
    })
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub fn configured(spec: &ReaderSpec) -> bool {
    spec.key_env
        .as_deref()
        .is_none_or(|k| env_nonempty(k).is_some())
}

/// Whether `spec` reads `url` ahead of the rotation.
pub fn first_for(spec: &ReaderSpec, url: &str) -> bool {
    let Some(f) = &spec.first_for else {
        return false;
    };
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let path = rest
        .split_once('/')
        .map(|(_, p)| p)
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.'))
        .map(|(_, e)| e.to_lowercase());
    f.hosts
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        || ext.is_some_and(|e| f.extensions.contains(&e))
}

/// Read one page with one reader. `Err` is text the caller classifies
/// (a spent allowance says "quota exhausted").
pub async fn read(client: &Client, spec: &ReaderSpec, url: &str) -> Result<Page, String> {
    if spec.builtin == Some(Builtin::Direct) {
        return read_direct(client, url).await;
    }
    let (Some(r), Some(rs)) = (&spec.request, &spec.response) else {
        return Err(format!("{} has no request", spec.name));
    };
    let name = &spec.name;
    let key = match &spec.key_env {
        Some(env) => Some(
            env_nonempty(env).ok_or_else(|| format!("{name} key not configured (set {env})"))?,
        ),
        None => None,
    };
    let mut body = r.fixed.clone();
    if let Some(f) = &r.url_field {
        body.insert(f.clone(), json!(url));
    }
    if let Some(f) = &r.urls_field {
        body.insert(f.clone(), json!([url]));
    }
    let mut req = client.post(&r.url);
    if let (Some(auth), Some(key)) = (&r.auth, &key) {
        if let Some(h) = &auth.header {
            req = req.header(h.as_str(), format!("{}{key}", auth.prefix));
        } else if let Some(q) = &auth.query {
            req = req.query(&[(q.as_str(), key.as_str())]);
        }
    }
    let secret = key.as_deref().unwrap_or("");
    let resp = req
        .json(&body)
        .timeout(std::time::Duration::from_secs(r.timeout_secs.unwrap_or(60)))
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", redact(&e.to_string(), secret)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let lower = text.to_lowercase();
        let spent = rs.spent_status.contains(&status.as_u16())
            || rs
                .spent_words
                .iter()
                .any(|w| lower.contains(&w.to_lowercase()));
        let prefix = if spent { "quota exhausted — " } else { "" };
        return Err(format!(
            "{name} read failed: {prefix}HTTP {status} - {}",
            redact(&text, secret).chars().take(300).collect::<String>()
        ));
    }
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("{name}: unparseable answer: {e}"))?;
    if let Some(req) = &rs.require
        && path_get(&json, &req.path) != Some(&req.equals)
    {
        return Err(format!(
            "{name}: {} is {}, not {}",
            req.path,
            path_get(&json, &req.path).cloned().unwrap_or(Value::Null),
            req.equals
        ));
    }
    if let Some(err) = rs
        .error
        .iter()
        .find_map(|p| path_get(&json, p))
        .filter(|v| !v.is_null())
    {
        let e = err
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("{name}: {}", redact(&e, secret)));
    }
    let page_text = rs
        .text
        .iter()
        .find_map(|p| {
            path_get(&json, p)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or("")
        .to_string();
    let title = rs
        .title
        .as_deref()
        .and_then(|p| path_get(&json, p))
        .and_then(Value::as_str)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok(Page {
        title,
        text: page_text,
        units: budget_units(spec.budget.as_ref(), &json),
    })
}

/// Our own GET: HTML reduced to text, any other text type as sent.
async fn read_direct(client: &Client, url: &str) -> Result<Page, String> {
    let mut resp = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/126 Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| match agent_brain_protocol::net_policy::refusal_in(&e) {
            Some(why) => format!("Not read: {why}"),
            None => format!("Request failed: {e}"),
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("direct read failed: HTTP {status}"));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let texty = ctype.is_empty()
        || ctype.starts_with("text/")
        || ctype.contains("json")
        || ctype.contains("xml")
        || ctype.contains("yaml")
        || ctype.contains("javascript");
    if !texty {
        return Err(format!("direct read: not text ({ctype})"));
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= DIRECT_MAX_BYTES {
            break;
        }
    }
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let html = ctype.contains("html") || (ctype.is_empty() && raw.trim_start().starts_with('<'));
    Ok(if html {
        Page {
            title: html_title(&raw),
            text: html_text(&raw),
            units: 1.0,
        }
    } else {
        Page {
            title: None,
            text: raw,
            units: 1.0,
        }
    })
}

fn html_title(html: &str) -> Option<String> {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .ok()?
        .captures(html)
        .map(|c| decode_entities(c[1].trim()))
        .filter(|t| !t.is_empty())
}

/// Common entities and numeric references.
fn decode_entities(s: &str) -> String {
    let named = s
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    let numeric = Regex::new(r"&#(x?[0-9a-fA-F]+);").expect("static regex");
    let out = numeric.replace_all(&named, |c: &regex::Captures| {
        let v = &c[1];
        let n = match v.strip_prefix(['x', 'X']) {
            Some(h) => u32::from_str_radix(h, 16).ok(),
            None => v.parse::<u32>().ok(),
        };
        n.and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default()
    });
    out.replace("&amp;", "&")
}

/// HTML reduced to text with its block structure: boilerplate elements
/// dropped, blocks as lines, headings marked `##`, list items `-`.
pub fn html_text(html: &str) -> String {
    let mut h = Regex::new(r"(?s)<!--.*?-->")
        .expect("static regex")
        .replace_all(html, " ")
        .into_owned();
    for tag in [
        "script", "style", "noscript", "svg", "nav", "header", "footer", "aside", "form", "iframe",
        "button", "select", "template",
    ] {
        let re = Regex::new(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>")).expect("tag regex");
        h = re.replace_all(&h, " ").into_owned();
    }
    let h = Regex::new(r"(?i)<h[1-6]\b[^>]*>")
        .expect("static regex")
        .replace_all(&h, "\n\n## ");
    let h = Regex::new(r"(?i)<li\b[^>]*>")
        .expect("static regex")
        .replace_all(&h, "\n- ");
    let h = Regex::new(
        r"(?i)</(?:p|div|h[1-6]|li|blockquote|tr|section|article|pre|dd|dt|table)\s*>|<br\s*/?>",
    )
    .expect("static regex")
    .replace_all(&h, "\n");
    let h = Regex::new(r"(?s)<[^>]+>")
        .expect("static regex")
        .replace_all(&h, " ");
    let decoded = decode_entities(&h);
    let mut out: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
        } else {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> Vec<ReaderSpec> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/page-readers");
        let mut v: Vec<ReaderSpec> = std::fs::read_dir(dir)
            .expect("core page-readers dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
            .map(|p| {
                parse(&std::fs::read_to_string(&p).unwrap())
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
            })
            .collect();
        v.sort_by_key(|r| r.rank.unwrap_or(u32::MAX));
        v
    }

    #[test]
    fn every_bundled_reader_parses_in_rank_order() {
        let names: Vec<String> = bundled().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["tinyfish", "anakin", "direct"]);
    }

    #[test]
    fn raw_files_go_to_the_direct_reader_first() {
        let direct = bundled().into_iter().find(|r| r.name == "direct").unwrap();
        assert!(first_for(
            &direct,
            "https://raw.githubusercontent.com/ggml-org/llama.cpp/master/README.md"
        ));
        assert!(first_for(&direct, "https://example.com/docs/spec.json?x=1"));
        assert!(first_for(&direct, "https://example.com/src/lib.rs"));
        assert!(!first_for(
            &direct,
            "https://www.reuters.com/world/some-story-2026-10-16/"
        ));
        assert!(!first_for(&direct, "https://example.com/"));
    }

    #[test]
    fn a_reader_is_builtin_or_a_service_never_both() {
        let both = "name: x\nbuiltin: direct\nrequest: { url: https://x, url_field: u }\n\
                    response: { text: [t] }";
        assert!(parse(both).unwrap_err().contains("either"));
        let two_fields = "name: x\nrequest: { url: https://x, url_field: u, urls_field: us }\n\
                          response: { text: [t] }";
        assert!(parse(two_fields).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn html_keeps_blocks_and_drops_boilerplate() {
        let html = "<html><head><title>Fed &amp; rates</title><style>p{}</style></head>\
                    <body><nav>Home | World</nav><h1>Warsh holds rates</h1>\
                    <p>The Fed held rates at&nbsp;4%.</p><ul><li>One</li><li>Two &#8212; three</li></ul>\
                    <footer>© 2026</footer><script>var x=1;</script></body></html>";
        assert_eq!(html_title(html).as_deref(), Some("Fed & rates"));
        let t = html_text(html);
        assert!(t.contains("## Warsh holds rates"), "{t}");
        assert!(t.contains("The Fed held rates at 4%."));
        assert!(t.contains("- Two \u{2014} three"));
        for gone in ["Home | World", "© 2026", "var x", "p{}"] {
            assert!(!t.contains(gone), "{gone} in {t}");
        }
    }
}
