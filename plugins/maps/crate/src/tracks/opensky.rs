//! Aircraft: OpenSky's `states/all` over each area that watches aircraft,
//! every [`EVERY`], with OAuth2 client credentials (`OPENSKY_CLIENT_ID`,
//! `OPENSKY_CLIENT_SECRET`; a token lasts 30 minutes). The URLs are fixed,
//! so the credentials reach OpenSky only.
//!
//! Credits (measured 2026-10-21): 4,000 a day; a box under 25 square degrees
//! costs 1, up to 100 costs 2 (areas are capped at 100). Polling stops for
//! the hour when fewer than [`CREDIT_FLOOR`] remain, and a 429 waits as long
//! as OpenSky says. Each poll is a snapshot: every aircraft it shows is
//! stored, and its coverage row counts the interval it stands for.

use std::time::Duration;

use agent_brain_plugin_api::protocol::net_policy;
use serde_json::Value;
use tracing::{info, warn};

use super::areas::Area;
use super::store::{Coverage, Position, Store};
use super::{Tracks, hour_of, now};

const TOKEN_URL: &str =
    "https://auth.opensky-network.org/auth/realms/opensky-network/protocol/openid-connect/token";
const STATES_URL: &str = "https://opensky-network.org/api/states/all";
const SOURCE: &str = "adsb";
const EVERY: Duration = Duration::from_secs(600);
const CREDIT_FLOOR: i64 = 100;

/// OpenSky's aircraft category (`extended=1`, state index 17).
pub fn category(c: i64) -> Option<&'static str> {
    Some(match c {
        2 => "light",
        3 => "small",
        4 => "large",
        5 => "high vortex large",
        6 => "heavy",
        7 => "high performance",
        8 => "rotorcraft",
        9 => "glider",
        10 => "lighter than air",
        11 => "parachutist",
        12 => "ultralight",
        14 => "drone",
        15 => "space vehicle",
        16 => "emergency vehicle",
        17 => "service vehicle",
        18..=20 => "obstacle",
        _ => return None,
    })
}

/// The positions in one `states/all` answer.
pub fn parse(body: &Value, area: &str) -> Vec<Position> {
    let t = body["time"].as_i64().unwrap_or_else(now);
    let Some(states) = body["states"].as_array() else {
        return vec![];
    };
    states
        .iter()
        .filter_map(|s| {
            let id = s[0].as_str()?.trim().to_lowercase();
            let lon = s[5].as_f64()?;
            let lat = s[6].as_f64()?;
            Some(Position {
                source: SOURCE,
                id,
                area: area.to_string(),
                lat,
                lon,
                speed: s[9].as_f64(),
                course: s[10].as_f64(),
                altitude: s[13].as_f64().or_else(|| s[7].as_f64()),
                at: s[3].as_i64().unwrap_or(t),
                name: s[1]
                    .as_str()
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty()),
                class: s[17].as_i64().and_then(category).map(Into::into),
            })
        })
        .collect()
}

struct Token {
    value: String,
    until: i64,
}

pub fn spawn(t: Tracks, store: Store) {
    tokio::spawn(async move {
        let http = net_policy::guard(reqwest::Client::builder())
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        let mut token: Option<Token> = None;
        // Let the brain finish starting.
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let (all, _) = t.areas();
            let areas: Vec<Area> = all.into_iter().filter(|a| a.aircraft).collect();
            t.status.update(SOURCE, |s| {
                s.areas = areas.iter().map(|a| a.name.clone()).collect()
            });
            let wait = poll(&t, &store, &http, &mut token, &areas).await;
            tokio::time::sleep(wait).await;
        }
    });
}

/// One round over the areas; returns how long to wait before the next.
async fn poll(
    t: &Tracks,
    store: &Store,
    http: &reqwest::Client,
    token: &mut Option<Token>,
    areas: &[Area],
) -> Duration {
    if areas.is_empty() {
        t.status.set(
            SOURCE,
            "off",
            Some("no area watches aircraft (areas/*.yaml, aircraft: true)".into()),
        );
        return EVERY;
    }
    let (Some(id), Some(secret)) = (t.env("OPENSKY_CLIENT_ID"), t.env("OPENSKY_CLIENT_SECRET"))
    else {
        t.status.set(
            SOURCE,
            "off",
            Some("OPENSKY_CLIENT_ID / OPENSKY_CLIENT_SECRET are not set".into()),
        );
        return EVERY;
    };
    if token.as_ref().is_none_or(|k| k.until <= now() + 60) {
        match fetch_token(http, &id, &secret).await {
            Ok(k) => *token = Some(k),
            Err(e) => {
                warn!(error = %e, "opensky: no token");
                t.status.set(SOURCE, "error", Some(e));
                return EVERY;
            }
        }
    }
    let bearer = token.as_ref().map(|k| k.value.clone()).unwrap_or_default();
    let started = now();
    let mut failed = 0;
    let mut credits: Option<i64> = None;
    for (i, a) in areas.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let [w, s, e, n] = a.bbox;
        let resp = http
            .get(STATES_URL)
            .bearer_auth(&bearer)
            .query(&[
                ("lamin", s.to_string()),
                ("lomin", w.to_string()),
                ("lamax", n.to_string()),
                ("lomax", e.to_string()),
                ("extended", "1".to_string()),
            ])
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(err) => {
                let why = match net_policy::refusal_in(&err) {
                    Some(w) => format!("not asked: {w}"),
                    None => format!("request failed: {err}"),
                };
                t.status
                    .set(SOURCE, "error", Some(format!("{}: {why}", a.name)));
                failed += 1;
                continue;
            }
        };
        let remaining = resp
            .headers()
            .get("x-rate-limit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok());
        let status = resp.status();
        if status.as_u16() == 429 {
            let retry = resp
                .headers()
                .get("x-rate-limit-retry-after-seconds")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(3600);
            t.status.set(
                SOURCE,
                "waiting",
                Some(format!("OpenSky credits used up; retrying in {retry}s")),
            );
            return Duration::from_secs(retry.max(60));
        }
        if status.as_u16() == 401 {
            *token = None;
            t.status.set(
                SOURCE,
                "error",
                Some("OpenSky refused the token (401); a new one next round".into()),
            );
            return Duration::from_secs(60);
        }
        if !status.is_success() {
            t.status.set(
                SOURCE,
                "error",
                Some(format!(
                    "{}: OpenSky answered HTTP {}",
                    a.name,
                    status.as_u16()
                )),
            );
            failed += 1;
            continue;
        }
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(err) => {
                t.status.set(
                    SOURCE,
                    "error",
                    Some(format!("{}: unreadable answer: {err}", a.name)),
                );
                failed += 1;
                continue;
            }
        };
        let positions = parse(&body, &a.name);
        let ids: std::collections::HashSet<&str> =
            positions.iter().map(|p| p.id.as_str()).collect();
        let cov = Coverage {
            source: SOURCE,
            area: a.name.clone(),
            hour: hour_of(started),
            messages: positions.len() as i64,
            ids: ids.len() as i64,
            up_secs: EVERY.as_secs() as i64,
        };
        let n = positions.len() as u64;
        let res = store
            .blocking(move |s| {
                s.insert_positions(&positions)?;
                s.add_coverage(&[cov])
            })
            .await;
        if let Err(err) = res {
            t.status
                .set(SOURCE, "error", Some(format!("could not store: {err}")));
            failed += 1;
            continue;
        }
        t.status.update(SOURCE, |s| {
            s.messages += 1;
            s.stored += n;
            s.last_message_at = Some(now());
        });
        if let Some(r) = remaining {
            credits = Some(r);
            if r < CREDIT_FLOOR {
                t.status.set(
                    SOURCE,
                    "waiting",
                    Some(format!(
                        "only {r} OpenSky credits left; paused for the hour"
                    )),
                );
                return Duration::from_secs(3600);
            }
        }
    }
    // A round with a failure leaves the error showing until a clean round.
    if failed == 0 {
        t.status.set(
            SOURCE,
            "running",
            credits.map(|r| format!("{r} credits left today")),
        );
    }
    info!(areas = areas.len(), failed, "opensky: polled");
    EVERY
}

async fn fetch_token(http: &reqwest::Client, id: &str, secret: &str) -> Result<Token, String> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", id),
            ("client_secret", secret),
        ])
        .send()
        .await
        .map_err(|e| format!("OpenSky token request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("OpenSky token answer unreadable: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "OpenSky refused the client credentials (HTTP {}): {}",
            status.as_u16(),
            body["error_description"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("")
        ));
    }
    let value = body["access_token"]
        .as_str()
        .ok_or("OpenSky's token answer has no access_token")?
        .to_string();
    let life = body["expires_in"].as_i64().unwrap_or(1800);
    Ok(Token {
        value,
        until: now() + life,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_state_vector_becomes_a_position() {
        let body = json!({"time": 1000, "states": [
            ["4b1805", "SWR22   ", "Switzerland", 990, 999, 56.1, 26.2, 11000.0, false, 240.0, 95.0, 0.0, null, 11300.0, "1000", false, 0, 6],
            ["abc123", "", "Iran", null, 999, null, null, null, true, null, null, null, null, null, null, false, 0, 0],
            ["def456", null, "Oman", null, 999, 57.0, 25.0, 300.0, false, 70.0, 10.0, 0.0, null, null, null, false, 0, 8]
        ]});
        let p = parse(&body, "hormuz");
        assert_eq!(p.len(), 2, "no position, not stored");
        assert_eq!(p[0].id, "4b1805");
        assert_eq!(p[0].name.as_deref(), Some("SWR22"));
        assert_eq!(p[0].class.as_deref(), Some("heavy"));
        assert_eq!(p[0].altitude, Some(11300.0));
        assert_eq!(p[0].at, 990);
        assert_eq!(p[1].at, 1000, "no time_position: the answer's time");
        assert_eq!(p[1].name, None);
        assert_eq!(p[1].class.as_deref(), Some("rotorcraft"));
        assert_eq!(p[1].altitude, Some(300.0));
        assert!(parse(&json!({"time": 1, "states": null}), "x").is_empty());
    }
}
