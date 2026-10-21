//! Where a fetch the model asked for may go.
//!
//! `http_request`, `read_page`, `ingest_api`, `define_api_context` and the
//! media fetchers take a URL a model chose. Before 2026-10-16 any of them
//! would fetch whatever the brain's container can reach: Neo4j, llama.cpp,
//! the sandboxes, the host, the owner's LAN and tailnet. A prompt injected
//! into a web page could steer a model there.
//!
//! The policy is checked on the address, never the name: a name is resolved
//! by [`GuardedResolver`] on every connection (redirects included), so a
//! Docker service name, `host.docker.internal`, a tailnet name or a DNS
//! answer that changes between lookups is judged by where it actually goes.
//! A URL with an IP literal skips DNS, so [`check_url`] judges it, on the
//! first request and on each redirect hop ([`guard`]).
//!
//! The one way through is `HTTP_ALLOW_INTERNAL`, which only the owner sets:
//! comma-separated `host` or `host:port` entries (a host entry allows every
//! port of it; `host:port` only that one), e.g. a Home Assistant on the
//! tailnet. A model cannot widen it: registering an API (`define_api_context`)
//! does not, because the fetch still goes through this policy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;

/// Every refusal carries this, so a caller can find it in an error chain.
pub const MARKER: &str = "[net policy]";

/// What the owner allowed through (`HTTP_ALLOW_INTERNAL`).
#[derive(Debug, Clone, Default)]
pub struct Allow {
    entries: Vec<(String, Option<u16>)>,
}

impl Allow {
    pub fn parse(spec: &str) -> Self {
        let entries = spec
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                match e.rsplit_once(':') {
                    Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => {
                        (h.to_string(), p.parse().ok())
                    }
                    _ => (e.trim_matches(|c| c == '[' || c == ']').to_string(), None),
                }
            })
            .collect();
        Allow { entries }
    }

    pub fn from_env() -> &'static Allow {
        static A: OnceLock<Allow> = OnceLock::new();
        A.get_or_init(|| Allow::parse(&std::env::var("HTTP_ALLOW_INTERNAL").unwrap_or_default()))
    }

    /// The host is allowed on some port.
    fn host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.entries.iter().any(|(h, _)| *h == host)
    }

    /// The host is allowed on this port (a bare-host entry allows any).
    fn host_port(&self, host: &str, port: Option<u16>) -> bool {
        let host = host.to_ascii_lowercase();
        self.entries
            .iter()
            .any(|(h, p)| *h == host && (p.is_none() || *p == port))
    }

    /// The host has entries, and none of them is this port.
    fn wrong_port(&self, host: &str, port: Option<u16>) -> bool {
        self.host(host) && !self.host_port(host, port)
    }
}

/// An address the brain must not reach on a model's say-so: loopback,
/// private, link-local (cloud metadata), carrier-grade NAT (tailnets),
/// unique-local, multicast, unspecified, reserved; IPv4 inside IPv6 by the
/// address it carries.
pub fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => internal_v4(v4),
        IpAddr::V6(v6) => internal_v6(v6),
    }
}

fn internal_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10, CGNAT / Tailscale
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 198 && (o[1] & 0xFE) == 18) // 198.18.0.0/15
        || o[0] >= 240 // 240.0.0.0/4 reserved
}

fn internal_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if let Some(v4) = ip.to_ipv4_mapped() {
        return internal_v4(v4);
    }
    // 64:ff9b::/96, NAT64: judge the IPv4 it carries.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        let v4 = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
        return internal_v4(v4);
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        || (s[0] & 0xffc0) == 0xfec0 // fec0::/10 site local (deprecated)
        || s[0] == 0x2001 && s[1] == 0x0db8 // documentation
}

fn refusal(what: &str) -> String {
    format!(
        "{MARKER} {what}. The brain fetches only public hosts on a model's request; \
         the owner can allow an internal one with HTTP_ALLOW_INTERNAL."
    )
}

/// Judge a URL before it is requested, and each redirect hop: the scheme,
/// and a host given as an IP literal (which DNS, and so the resolver, never
/// sees). A name is judged where it resolves.
pub fn check_url(url: &url::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{MARKER} only http and https URLs are fetched, not {}:",
            url.scheme()
        ));
    }
    check_url_host(url)
}

/// [`check_url`] without the scheme: the host and port.
fn check_url_host(url: &url::Url) -> Result<(), String> {
    let allow = Allow::from_env();
    let port = url.port_or_known_default();
    match url.host() {
        None => Err(format!("{MARKER} the URL has no host")),
        Some(url::Host::Ipv4(v4)) => check_ip(allow, &v4.to_string(), IpAddr::V4(v4), port),
        Some(url::Host::Ipv6(v6)) => check_ip(allow, &v6.to_string(), IpAddr::V6(v6), port),
        Some(url::Host::Domain(d)) => {
            if allow.wrong_port(d, port) {
                Err(refusal(&format!(
                    "{d} is allowed only on the port(s) HTTP_ALLOW_INTERNAL names, not {}",
                    port.map_or("this one".to_string(), |p| p.to_string())
                )))
            } else {
                Ok(())
            }
        }
    }
}

/// [`check_url`] on a string; an unparseable URL is left to the caller's
/// own error.
pub fn check_url_str(url: &str) -> Result<(), String> {
    match url::Url::parse(url) {
        Ok(u) => check_url(&u),
        Err(_) => Ok(()),
    }
}

fn check_ip(allow: &Allow, host: &str, ip: IpAddr, port: Option<u16>) -> Result<(), String> {
    if is_internal(ip) && !allow.host_port(host, port) {
        Err(refusal(&format!("{ip} is an internal address")))
    } else {
        Ok(())
    }
}

/// Resolve `host` and refuse it if any address is internal (an allowed host
/// excepted). For a fetcher that is not a reqwest client (yt-dlp): its own
/// lookup may differ, so this narrows the window, it does not close it.
pub async fn check_host(host: &str) -> Result<(), String> {
    resolve_checked(host, 0).await.map(|_| ())
}

/// For a client that opens its own socket (a WebSocket): judge `url` as
/// [`check_url`] does (ws and wss stand for http and https) and return the
/// checked addresses to connect to. Connecting to these, never re-resolving
/// the name, closes the window [`check_host`] leaves open.
pub async fn connect_addrs(url: &url::Url) -> Result<Vec<SocketAddr>, String> {
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return Err(format!(
            "{MARKER} only http(s) and ws(s) URLs are opened, not {}:",
            url.scheme()
        ));
    }
    check_url_host(url)?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| format!("{MARKER} the URL has no port"))?;
    match url.host() {
        None => Err(format!("{MARKER} the URL has no host")),
        Some(url::Host::Ipv4(v4)) => Ok(vec![SocketAddr::new(IpAddr::V4(v4), port)]),
        Some(url::Host::Ipv6(v6)) => Ok(vec![SocketAddr::new(IpAddr::V6(v6), port)]),
        Some(url::Host::Domain(d)) => resolve_checked(d, port).await,
    }
}

async fn resolve_checked(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("could not resolve {host}: {e}"))?
        .collect();
    if Allow::from_env().host(host) {
        return Ok(addrs);
    }
    if let Some(bad) = addrs.iter().find(|a| is_internal(a.ip())) {
        return Err(refusal(&format!(
            "{host} resolves to an internal address ({})",
            bad.ip()
        )));
    }
    Ok(addrs)
}

/// The DNS resolver for a guarded client: every lookup a connection makes,
/// redirects included, is judged by the addresses it returns.
#[derive(Debug, Clone, Default)]
pub struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_checked(&host, 0)
                .await
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;
            let it: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

/// Put a client builder under the policy: the guarded resolver, and a
/// redirect policy that judges each hop (at most 10, reqwest's default).
pub fn guard(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .dns_resolver(std::sync::Arc::new(GuardedResolver))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if let Err(e) = check_url(attempt.url()) {
                attempt.error(e)
            } else {
                attempt.follow()
            }
        }))
}

/// A guarded client with reqwest's defaults otherwise.
pub fn guarded_client() -> reqwest::Client {
    guard(reqwest::Client::builder())
        .build()
        .unwrap_or_default()
}

/// The policy's refusal inside a request error, if that is what failed.
pub fn refusal_in(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let s = e.to_string();
        if let Some(i) = s.find(MARKER) {
            return Some(s[i..].to_string());
        }
        cur = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn internal_addresses() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.17.0.1",
            "192.168.1.10",
            "169.254.169.254",
            "100.101.102.103",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(is_internal(ip(a)), "{a} should be internal");
        }
        for a in [
            "1.1.1.1",
            "93.184.216.34",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_internal(ip(a)), "{a} should be public");
        }
    }

    #[test]
    fn ip_literals_and_schemes_are_judged_before_dns() {
        for u in [
            "http://127.0.0.1:7474/db",
            "http://[::1]/",
            "http://2130706433/", // 127.0.0.1 as one decimal number
            "http://0x7f.1/",     // 127.0.0.1 in hex shorthand
            "http://169.254.169.254/latest/meta-data/",
            "http://[::ffff:192.168.1.1]/",
        ] {
            let e = check_url_str(u).unwrap_err();
            assert!(e.contains(MARKER) && e.contains("internal"), "{u}: {e}");
        }
        assert!(
            check_url_str("file:///etc/passwd")
                .unwrap_err()
                .contains("only http")
        );
        assert!(check_url_str("https://example.com/x").is_ok());
        // A name is the resolver's to judge.
        assert!(check_url_str("http://neo4j:7474/").is_ok());
    }

    #[test]
    fn the_allowlist_takes_hosts_and_ports() {
        let a = Allow::parse(" HA.tailnet.ts.net:8123 , 192.168.1.5 ");
        assert!(a.host_port("ha.tailnet.ts.net", Some(8123)));
        assert!(!a.host_port("ha.tailnet.ts.net", Some(22)));
        assert!(a.wrong_port("ha.tailnet.ts.net", Some(22)));
        assert!(a.host_port("192.168.1.5", Some(80)));
        assert!(!a.host("other.ts.net"));
    }

    #[tokio::test]
    async fn a_socket_client_gets_checked_addresses_or_a_refusal() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        for bad in ["ws://127.0.0.1:8081/ws", "wss://[::1]/", "ws://localhost/"] {
            let e = connect_addrs(&u(bad)).await.unwrap_err();
            assert!(e.contains(MARKER) && e.contains("internal"), "{bad}: {e}");
        }
        assert!(
            connect_addrs(&u("ftp://1.1.1.1/"))
                .await
                .unwrap_err()
                .contains("only http(s) and ws(s)")
        );
        // An IP literal skips DNS; the scheme's default port is filled in.
        let a = connect_addrs(&u("wss://1.1.1.1/x")).await.unwrap();
        assert_eq!(a, vec!["1.1.1.1:443".parse().unwrap()]);
    }

    #[tokio::test]
    async fn a_name_that_resolves_inside_is_refused() {
        let e = check_host("localhost").await.unwrap_err();
        assert!(e.contains("internal address"), "{e}");
    }

    #[tokio::test]
    async fn a_guarded_client_refuses_loopback_by_name_and_says_why() {
        let err = guarded_client()
            .get("http://localhost:1/")
            .send()
            .await
            .unwrap_err();
        let why = refusal_in(&err).expect("the refusal is findable in the error chain");
        assert!(why.contains("internal address"), "{why}");
    }
}
