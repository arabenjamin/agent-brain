//! Ships: one aisstream.io WebSocket for every area that watches ships.
//!
//! The key (`AISSTREAM_API_KEY`) goes inside the subscription message, so
//! the stream is this code's and the URL is fixed: the key can reach no
//! other host and never passes through a model. It is never logged.
//!
//! A ship reports every few seconds; one position per ship per
//! [`KEEP_EVERY`] is stored. Ship details (name, type, destination) come in
//! their own messages and class the positions that follow. Coverage is
//! flushed every [`FLUSH`]: per area, the messages heard and the seconds the
//! stream was connected, zeros included.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use agent_brain_plugin_api::protocol::net_policy;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use super::areas::{Area, area_of};
use super::store::{Coverage, Position, Store, Vessel};
use super::{Tracks, hour_of, now, wait};

const URL: &str = "wss://stream.aisstream.io/v0/stream";
const SOURCE: &str = "ais";
/// One stored position per ship per this many seconds.
const KEEP_EVERY: i64 = 300;
const FLUSH: Duration = Duration::from_secs(60);
/// No message for this long: reconnect.
const SILENCE: Duration = Duration::from_secs(600);
/// The longest wait before the areas are re-read when nothing wakes the
/// collector (a change in the graph wakes it at once and reconnects).
const REREAD: Duration = Duration::from_secs(600);
const TYPES: &[&str] = &[
    "PositionReport",
    "StandardClassBPositionReport",
    "ExtendedClassBPositionReport",
    "ShipStaticData",
    "StaticDataReport",
];

/// The class of an AIS ship type code (ITU-R M.1371).
pub fn ship_class(t: i64) -> &'static str {
    match t {
        30 => "fishing",
        31 | 32 | 52 => "tug or towing",
        33 => "dredging",
        34 => "diving",
        35 => "military",
        36 => "sailing",
        37 => "pleasure",
        40..=49 => "high speed",
        50 => "pilot",
        51 => "search and rescue",
        53 => "port tender",
        54 => "pollution control",
        55 => "law enforcement",
        58 => "medical",
        60..=69 => "passenger",
        70..=79 => "cargo",
        80..=89 => "tanker",
        _ => "other",
    }
}

/// What one stream message is.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    Position(Position),
    Static(Vessel),
    /// aisstream's own error (a bad key, a bad subscription).
    Error(String),
    Skip,
}

fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| s.trim().trim_end_matches('@').trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn parse(msg: &str, areas: &[Area], types: &HashMap<String, i64>, at: i64) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(msg) else {
        return Parsed::Skip;
    };
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Parsed::Error(e.to_string());
    }
    let kind = v["MessageType"].as_str().unwrap_or("");
    let meta = &v["MetaData"];
    let mmsi = match &meta["MMSI"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return Parsed::Skip,
    };
    let body = &v["Message"][kind];
    match kind {
        "ShipStaticData" => Parsed::Static(Vessel {
            mmsi,
            name: text(&body["Name"]).or_else(|| text(&meta["ShipName"])),
            ship_type: body["Type"].as_i64().filter(|t| *t > 0),
            callsign: text(&body["CallSign"]),
            imo: body["ImoNumber"].as_i64().filter(|n| *n > 0),
            destination: text(&body["Destination"]),
            at,
        }),
        "StaticDataReport" => Parsed::Static(Vessel {
            mmsi,
            name: text(&body["ReportA"]["Name"]).or_else(|| text(&meta["ShipName"])),
            ship_type: body["ReportB"]["ShipType"].as_i64().filter(|t| *t > 0),
            callsign: text(&body["ReportB"]["CallSign"]),
            at,
            ..Default::default()
        }),
        "PositionReport" | "StandardClassBPositionReport" | "ExtendedClassBPositionReport" => {
            let lat = body["Latitude"]
                .as_f64()
                .or_else(|| meta["latitude"].as_f64());
            let lon = body["Longitude"]
                .as_f64()
                .or_else(|| meta["longitude"].as_f64());
            // 91 / 181 mean "not available".
            let (Some(lat), Some(lon)) = (lat, lon) else {
                return Parsed::Skip;
            };
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return Parsed::Skip;
            }
            let Some(area) = area_of(areas, lat, lon) else {
                return Parsed::Skip;
            };
            // 102.3 knots / 360 degrees mean "not available".
            let speed = body["Sog"].as_f64().filter(|s| *s < 102.2);
            let course = body["Cog"].as_f64().filter(|c| *c < 360.0);
            let ship_type = types.get(&mmsi).copied();
            Parsed::Position(Position {
                source: SOURCE,
                id: mmsi,
                area: area.name.clone(),
                lat,
                lon,
                speed,
                course,
                altitude: None,
                at,
                name: text(&meta["ShipName"]),
                class: ship_type.map(|t| ship_class(t).to_string()),
            })
        }
        _ => Parsed::Skip,
    }
}

/// A position, once for every area that contains it (areas may overlap).
pub fn in_every_area(p: Position, areas: &[Area]) -> Vec<Position> {
    let mut out: Vec<Position> = areas
        .iter()
        .filter(|a| a.name != p.area && a.contains(p.lat, p.lon))
        .map(|a| Position {
            area: a.name.clone(),
            ..p.clone()
        })
        .collect();
    out.insert(0, p);
    out
}

/// The subscription: every ship area's box, as aisstream wants it
/// (`[[lat, lon], [lat, lon]]` corners).
pub fn subscription(key: &str, areas: &[Area]) -> String {
    let boxes: Vec<Value> = areas
        .iter()
        .map(|a| {
            let [w, s, e, n] = a.bbox;
            json!([[s, w], [n, e]])
        })
        .collect();
    json!({"APIKey": key, "BoundingBoxes": boxes, "FilterMessageTypes": TYPES}).to_string()
}

/// Per-area tallies between flushes.
#[derive(Default)]
struct Tally {
    hour: i64,
    messages: HashMap<String, i64>,
    ids: HashMap<String, HashSet<String>>,
}

pub fn spawn(t: Tracks, store: Store) {
    tokio::spawn(async move {
        let mut backoff = 30;
        let mut rx = t.subscribe();
        loop {
            let all = match t.areas().await {
                Ok(a) => a,
                Err(e) => {
                    t.status.set(SOURCE, "error", Some(e));
                    wait(&mut rx, Duration::from_secs(60)).await;
                    continue;
                }
            };
            let areas: Vec<Area> = all.into_iter().filter(|a| a.ships).collect();
            t.status.update(SOURCE, |s| {
                s.areas = areas.iter().map(|a| a.name.clone()).collect()
            });
            if areas.is_empty() {
                t.status.set(
                    SOURCE,
                    "off",
                    Some("no watch area watches ships: draw one on the map".into()),
                );
                wait(&mut rx, REREAD).await;
                continue;
            }
            let Some(key) = t.env("AISSTREAM_API_KEY") else {
                t.status
                    .set(SOURCE, "off", Some("AISSTREAM_API_KEY is not set".into()));
                wait(&mut rx, REREAD).await;
                continue;
            };
            t.status.set(SOURCE, "connecting", None);
            match run(&t, &store, &key, &areas, &mut rx).await {
                Ok(why) => {
                    info!(why = %why, "ais: reconnecting");
                    backoff = 30;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    warn!(error = %e, "ais: stream failed");
                    t.status.set(
                        SOURCE,
                        "error",
                        Some(format!("{e} (retrying in {backoff}s)")),
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(600);
                }
            }
        }
    });
}

/// One connection, until it drops (`Err`) or should be renewed (`Ok(why)`).
async fn run(
    t: &Tracks,
    store: &Store,
    key: &str,
    areas: &[Area],
    rx: &mut tokio::sync::watch::Receiver<u64>,
) -> Result<String, String> {
    let url = url::Url::parse(URL).map_err(|e| e.to_string())?;
    let addrs = net_policy::connect_addrs(&url).await?;
    let mut last = format!("{URL} resolved to no address");
    let mut ws = None;
    for addr in &addrs {
        match tokio::time::timeout(Duration::from_secs(20), TcpStream::connect(addr)).await {
            Ok(Ok(tcp)) => match tokio_tungstenite::client_async_tls(URL, tcp).await {
                Ok((w, _)) => {
                    ws = Some(w);
                    break;
                }
                Err(e) => last = format!("WebSocket handshake failed: {e}"),
            },
            Ok(Err(e)) => last = format!("TCP connect to {addr} failed: {e}"),
            Err(_) => last = format!("TCP connect to {addr} timed out"),
        }
    }
    let mut ws = ws.ok_or(last)?;
    ws.send(Message::text(subscription(key, areas)))
        .await
        .map_err(|e| format!("subscription not sent: {e}"))?;

    let mut types = store.blocking(|s| s.ship_types()).await.unwrap_or_default();
    let mut kept: HashMap<String, i64> = HashMap::new();
    let mut positions: Vec<Position> = Vec::new();
    let mut vessels: Vec<Vessel> = Vec::new();
    let mut tally = Tally {
        hour: hour_of(now()),
        ..Default::default()
    };
    let names: Vec<String> = areas.iter().map(|a| a.name.clone()).collect();
    let mut flush = tokio::time::interval(FLUSH);
    flush.tick().await;
    let mut last_flush = now();
    let mut heard = false;
    let mut last_msg = now();
    t.status.set(SOURCE, "running", None);

    loop {
        tokio::select! {
            msg = ws.next() => {
                let msg = match msg {
                    None => return Err("the stream closed".into()),
                    Some(Err(e)) => return Err(format!("the stream failed: {e}")),
                    Some(Ok(m)) => m,
                };
                last_msg = now();
                let body = match msg {
                    Message::Text(s) => s.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                    Message::Close(f) => return Err(format!("aisstream closed the stream: {f:?}")),
                    _ => continue,
                };
                let at = now();
                match parse(&body, areas, &types, at) {
                    Parsed::Error(e) => return Err(format!("aisstream: {e}")),
                    Parsed::Static(v) => {
                        if let Some(ty) = v.ship_type {
                            types.insert(v.mmsi.clone(), ty);
                        }
                        vessels.push(v);
                    }
                    Parsed::Position(p) => {
                        heard = true;
                        let keep = kept.get(&p.id).is_none_or(|k| at - k >= KEEP_EVERY);
                        if keep {
                            kept.insert(p.id.clone(), at);
                        }
                        // A ship in overlapping areas counts in each of them.
                        for p in in_every_area(p, areas) {
                            *tally.messages.entry(p.area.clone()).or_default() += 1;
                            tally.ids.entry(p.area.clone()).or_default().insert(p.id.clone());
                            if keep {
                                positions.push(p);
                            }
                        }
                    }
                    Parsed::Skip => {}
                }
                t.status.update(SOURCE, |s| { s.messages += 1; s.last_message_at = Some(at); });
            }
            _ = flush.tick() => {
                let at = now();
                if at - last_msg > SILENCE.as_secs() as i64 {
                    return Ok(format!("no message for {}s", SILENCE.as_secs()));
                }
                let up = at - last_flush;
                last_flush = at;
                let cov: Vec<Coverage> = names.iter().map(|a| Coverage {
                    source: SOURCE,
                    area: a.clone(),
                    hour: tally.hour,
                    messages: tally.messages.get(a).copied().unwrap_or(0),
                    ids: tally.ids.get(a).map(|s| s.len() as i64).unwrap_or(0),
                    up_secs: up,
                }).collect();
                tally.messages.clear();
                if hour_of(at) != tally.hour {
                    tally.hour = hour_of(at);
                    tally.ids.clear();
                }
                let stored = positions.len() as u64;
                let (p, v) = (std::mem::take(&mut positions), std::mem::take(&mut vessels));
                let res = store.blocking(move |s| {
                    s.insert_positions(&p)?;
                    s.upsert_vessels(&v)?;
                    s.add_coverage(&cov)
                }).await;
                match res {
                    Ok(()) => t.status.update(SOURCE, |s| s.stored += stored),
                    Err(e) => {
                        warn!(error = %e, "ais: flush failed");
                        t.status.set(SOURCE, "error", Some(format!("could not store: {e}")));
                    }
                }
                if heard {
                    t.status.set(SOURCE, "running", None);
                }
                // Forget ships not heard from in a day.
                kept.retain(|_, k| at - *k < 86_400);
            }
            _ = rx.changed() => {
                let Ok(all) = t.areas().await else { continue };
                let now_areas: Vec<String> = all.iter().filter(|a| a.ships).map(|a| format!("{}{:?}", a.name, a.bbox)).collect();
                let had: Vec<String> = areas.iter().map(|a| format!("{}{:?}", a.name, a.bbox)).collect();
                if now_areas != had {
                    return Ok("the ship areas changed".into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn areas() -> Vec<Area> {
        vec![
            Area::parse("name: hormuz\ntitle: H\nbbox: [54.0, 24.0, 58.5, 27.5]\nships: true\n")
                .unwrap(),
        ]
    }

    #[test]
    fn positions_inside_an_area_are_kept_and_classed() {
        let types = HashMap::from([("422000001".to_string(), 84)]);
        let msg = r#"{"MessageType":"PositionReport","MetaData":{"MMSI":422000001,"ShipName":"HONOR   @@","latitude":26.5,"longitude":56.2},
            "Message":{"PositionReport":{"Latitude":26.5,"Longitude":56.2,"Sog":12.5,"Cog":360}}}"#;
        let Parsed::Position(p) = parse(msg, &areas(), &types, 1000) else {
            panic!("not a position");
        };
        assert_eq!(p.id, "422000001");
        assert_eq!(p.area, "hormuz");
        assert_eq!(p.name.as_deref(), Some("HONOR"));
        assert_eq!(p.class.as_deref(), Some("tanker"));
        assert_eq!(p.speed, Some(12.5));
        assert_eq!(p.course, None, "360 means not available");
        // Outside every area, or no position: skipped.
        let out = msg.replace("26.5", "12.0");
        assert_eq!(parse(&out, &areas(), &types, 1000), Parsed::Skip);
        let na = msg.replace("26.5", "91");
        assert_eq!(parse(&na, &areas(), &types, 1000), Parsed::Skip);
    }

    #[test]
    fn ship_details_and_errors_are_read() {
        let msg = r#"{"MessageType":"ShipStaticData","MetaData":{"MMSI":"1"},
            "Message":{"ShipStaticData":{"Name":"EVER GIVEN","Type":71,"CallSign":"H3RC","ImoNumber":9811000,"Destination":"JEBEL ALI  "}}}"#;
        let Parsed::Static(v) = parse(msg, &areas(), &HashMap::new(), 5) else {
            panic!("not static");
        };
        assert_eq!(v.ship_type, Some(71));
        assert_eq!(ship_class(71), "cargo");
        assert_eq!(v.destination.as_deref(), Some("JEBEL ALI"));
        assert_eq!(
            parse(
                r#"{"error":"Api Key Is Not Valid"}"#,
                &areas(),
                &HashMap::new(),
                5
            ),
            Parsed::Error("Api Key Is Not Valid".into())
        );
        assert_eq!(ship_class(35), "military");
    }

    #[test]
    fn a_ship_in_overlapping_areas_counts_in_each() {
        let mut both = areas();
        both.push(
            Area::parse("name: gulf\ntitle: G\nbbox: [56.0, 25.0, 60.0, 27.0]\nships: true\n")
                .unwrap(),
        );
        let msg = r#"{"MessageType":"PositionReport","MetaData":{"MMSI":1},"Message":{"PositionReport":{"Latitude":26.5,"Longitude":56.2}}}"#;
        let Parsed::Position(p) = parse(msg, &both, &HashMap::new(), 1) else {
            panic!("not a position");
        };
        let all = in_every_area(p, &both);
        assert_eq!(
            all.iter().map(|p| p.area.as_str()).collect::<Vec<_>>(),
            vec!["hormuz", "gulf"]
        );
        let outside = Parsed::Position(Position {
            lon: 59.0,
            ..all[1].clone()
        });
        let Parsed::Position(o) = outside else {
            unreachable!()
        };
        assert_eq!(in_every_area(o, &both).len(), 1);
    }

    #[test]
    fn the_subscription_has_every_box_as_lat_lon_corners() {
        let s: Value = serde_json::from_str(&subscription("k", &areas())).unwrap();
        assert_eq!(s["BoundingBoxes"], json!([[[24.0, 54.0], [27.5, 58.5]]]));
        assert_eq!(s["APIKey"], "k");
    }
}
