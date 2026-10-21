//! The tracks store (MAP_PLAN Phase 4): ship and aircraft positions in a
//! DuckDB file of their own (`TRACKS_DB_PATH`), never the graph: thousands
//! of rows an hour, kept [`RETENTION_DAYS`]. Only what they add up to (an
//! event, a count) reaches the graph.
//!
//! Times are unix seconds (BIGINT, column `seen`: `at` is reserved in
//! DuckDB), so no time-zone extension is needed.
//!
//! `coverage` is what keeps "no ships" from reading as "quiet": one row per
//! source, area and hour the collector ran, zeros included, with how long
//! it ran. An area a source ran over for a day and heard nothing from has no
//! receivers there (the Red Sea, measured 2026-10-21), and is said so.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use duckdb::{Connection, params};
use serde_json::{Value, json};

use super::ais::ship_class;

pub const RETENTION_DAYS: i64 = 14;

/// One position as a collector saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// `ais` or `adsb`.
    pub source: &'static str,
    /// MMSI or ICAO 24-bit address.
    pub id: String,
    pub area: String,
    pub lat: f64,
    pub lon: f64,
    /// Knots (ships) or m/s (aircraft), as the source reports.
    pub speed: Option<f64>,
    /// Degrees from north.
    pub course: Option<f64>,
    /// Metres (aircraft).
    pub altitude: Option<f64>,
    pub at: i64,
    /// Ship name or callsign.
    pub name: Option<String>,
    /// `tanker`, `cargo`, `military` … or an aircraft category.
    pub class: Option<String>,
}

/// What a ship says about itself (AIS ShipStaticData).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vessel {
    pub mmsi: String,
    pub name: Option<String>,
    pub ship_type: Option<i64>,
    pub callsign: Option<String>,
    pub imo: Option<i64>,
    pub destination: Option<String>,
    pub at: i64,
}

/// One coverage increment for an hour.
#[derive(Debug, Clone)]
pub struct Coverage {
    pub source: &'static str,
    pub area: String,
    /// Unix seconds at the start of the hour.
    pub hour: i64,
    pub messages: i64,
    /// Distinct ids so far this hour (stored as the greatest seen).
    pub ids: i64,
    /// Seconds the collector ran (and was connected, for a stream).
    pub up_secs: i64,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("cannot open {path}"))?;
        conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS positions (
                source VARCHAR NOT NULL, id VARCHAR NOT NULL, area VARCHAR NOT NULL,
                lat DOUBLE NOT NULL, lon DOUBLE NOT NULL,
                speed DOUBLE, course DOUBLE, altitude DOUBLE,
                seen BIGINT NOT NULL, name VARCHAR, class VARCHAR
            );
            CREATE INDEX IF NOT EXISTS positions_seen ON positions (seen);
            CREATE TABLE IF NOT EXISTS vessels (
                mmsi VARCHAR PRIMARY KEY, name VARCHAR, ship_type BIGINT,
                callsign VARCHAR, imo BIGINT, destination VARCHAR, updated BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS coverage (
                source VARCHAR NOT NULL, area VARCHAR NOT NULL, hour BIGINT NOT NULL,
                messages BIGINT NOT NULL, ids BIGINT NOT NULL, up_secs BIGINT NOT NULL,
                PRIMARY KEY (source, area, hour)
            );
            ",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Self::open(":memory:").unwrap()
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("tracks store lock poisoned"))?;
        f(&c)
    }

    /// Run `f` off the async threads.
    pub async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let s = self.clone();
        tokio::task::spawn_blocking(move || f(&s)).await?
    }

    pub fn insert_positions(&self, rows: &[Position]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.with(|c| {
            let mut app = c.appender("positions")?;
            for p in rows {
                app.append_row(params![
                    p.source, p.id, p.area, p.lat, p.lon, p.speed, p.course, p.altitude, p.at,
                    p.name, p.class
                ])?;
            }
            app.flush()?;
            Ok(())
        })
    }

    pub fn upsert_vessels(&self, rows: &[Vessel]) -> Result<()> {
        self.with(|c| {
            let mut st = c.prepare(
                "INSERT INTO vessels VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (mmsi) DO UPDATE SET
                   name = coalesce(excluded.name, name), ship_type = coalesce(excluded.ship_type, ship_type),
                   callsign = coalesce(excluded.callsign, callsign), imo = coalesce(excluded.imo, imo),
                   destination = coalesce(excluded.destination, destination), updated = excluded.updated",
            )?;
            for v in rows {
                st.execute(params![
                    v.mmsi, v.name, v.ship_type, v.callsign, v.imo, v.destination, v.at
                ])?;
            }
            Ok(())
        })
    }

    /// Ship types known so far, for classing positions as they arrive.
    pub fn ship_types(&self) -> Result<HashMap<String, i64>> {
        self.with(|c| {
            let mut st =
                c.prepare("SELECT mmsi, ship_type FROM vessels WHERE ship_type IS NOT NULL")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
    }

    pub fn add_coverage(&self, rows: &[Coverage]) -> Result<()> {
        self.with(|c| {
            let mut st = c.prepare(
                "INSERT INTO coverage VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (source, area, hour) DO UPDATE SET
                   messages = messages + excluded.messages,
                   ids = greatest(ids, excluded.ids),
                   up_secs = up_secs + excluded.up_secs",
            )?;
            for r in rows {
                st.execute(params![
                    r.source, r.area, r.hour, r.messages, r.ids, r.up_secs
                ])?;
            }
            Ok(())
        })
    }

    /// Drop positions and coverage older than the retention window.
    pub fn prune(&self, now: i64) -> Result<usize> {
        let cutoff = now - RETENTION_DAYS * 86_400;
        self.with(|c| {
            let n = c.execute("DELETE FROM positions WHERE seen < ?", params![cutoff])?;
            c.execute("DELETE FROM coverage WHERE hour < ?", params![cutoff])?;
            Ok(n)
        })
    }

    /// One source's coverage of an area since `since`: hours collected,
    /// seconds up, messages heard.
    pub fn coverage(&self, source: &str, area: &str, since: i64) -> Result<(i64, i64, i64)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT count(*), coalesce(sum(up_secs), 0), coalesce(sum(messages), 0)
                 FROM coverage WHERE source = ? AND area = ? AND hour >= ?",
                params![source, area, since - since.rem_euclid(3600)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
    }

    /// Distinct ids seen in an area since `since`, by class.
    pub fn by_class(
        &self,
        source: &str,
        area: &str,
        since: i64,
    ) -> Result<(i64, Vec<(String, i64)>)> {
        self.with(|c| {
            let total: i64 = c.query_row(
                "SELECT count(DISTINCT id) FROM positions WHERE source = ? AND area = ? AND seen >= ?",
                params![source, area, since],
                |r| r.get(0),
            )?;
            // A ship's type comes in its own message, often after its first
            // positions: class from the vessel details when a row has none.
            let mut st = c.prepare(
                "SELECT arg_max(p.class, p.seen), any_value(v.ship_type) FROM positions p
                 LEFT JOIN vessels v ON p.source = 'ais' AND v.mmsi = p.id
                 WHERE p.source = ? AND p.area = ? AND p.seen >= ? GROUP BY p.id",
            )?;
            let rows = st.query_map(params![source, area, since], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?))
            })?;
            let mut counts: HashMap<String, i64> = HashMap::new();
            for (class, ty) in rows.filter_map(|r| r.ok()) {
                let k = class
                    .or_else(|| ty.map(|t| ship_class(t).to_string()))
                    .unwrap_or_else(|| "unknown".into());
                *counts.entry(k).or_default() += 1;
            }
            let mut out: Vec<(String, i64)> = counts.into_iter().collect();
            out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            Ok((total, out))
        })
    }

    /// Distinct ids per area and source seen since `since`.
    pub fn area_counts(&self, since: i64) -> Result<Vec<(String, String, i64)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT area, source, count(DISTINCT id) FROM positions WHERE seen >= ? GROUP BY area, source",
            )?;
            let rows = st.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
    }

    /// The latest position of each id seen since `since`, newest first.
    /// `area` None = every area; `bbox` `[w, s, e, n]` keeps positions inside
    /// it (the map's view).
    pub fn latest(
        &self,
        source: Option<&str>,
        area: Option<&str>,
        since: i64,
        bbox: Option<[f64; 4]>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let [w, so, e, n] = bbox.map(|b| b.map(Some)).unwrap_or([None; 4]);
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT p.source, p.id, p.area, arg_max(p.lat, p.seen), arg_max(p.lon, p.seen),
                        arg_max(p.speed, p.seen), arg_max(p.course, p.seen), arg_max(p.altitude, p.seen),
                        max(p.seen), coalesce(arg_max(p.name, p.seen), any_value(v.name)),
                        arg_max(p.class, p.seen), any_value(v.destination), any_value(v.ship_type)
                 FROM positions p LEFT JOIN vessels v ON p.source = 'ais' AND v.mmsi = p.id
                 WHERE p.seen >= ? AND (? IS NULL OR p.source = ?) AND (? IS NULL OR p.area = ?)
                   AND (? IS NULL OR (p.lon BETWEEN ? AND ? AND p.lat BETWEEN ? AND ?))
                 GROUP BY p.source, p.id, p.area
                 ORDER BY max(p.seen) DESC LIMIT ?",
            )?;
            let rows = st.query_map(
                params![since, source, source, area, area, w, w, e, so, n, limit as i64],
                |r| {
                    Ok(json!({
                        "source": r.get::<_, String>(0)?,
                        "id": r.get::<_, String>(1)?,
                        "area": r.get::<_, String>(2)?,
                        "lat": r.get::<_, f64>(3)?,
                        "lon": r.get::<_, f64>(4)?,
                        "speed": r.get::<_, Option<f64>>(5)?,
                        "course": r.get::<_, Option<f64>>(6)?,
                        "altitude": r.get::<_, Option<f64>>(7)?,
                        "at": r.get::<_, i64>(8)?,
                        "name": r.get::<_, Option<String>>(9)?,
                        "class": r
                            .get::<_, Option<String>>(10)?
                            .or_else(|| r.get::<_, Option<i64>>(12).ok().flatten().map(|t| ship_class(t).to_string())),
                        "destination": r.get::<_, Option<String>>(11)?,
                    }))
                },
            )?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(id: &str, at: i64, class: Option<&str>) -> Position {
        Position {
            source: "ais",
            id: id.into(),
            area: "hormuz".into(),
            lat: 26.0,
            lon: 56.0 + at as f64 / 1e6,
            speed: Some(10.0),
            course: None,
            altitude: None,
            at,
            name: None,
            class: class.map(Into::into),
        }
    }

    #[test]
    fn positions_count_by_class_and_the_latest_wins() {
        let s = Store::memory();
        s.insert_positions(&[
            pos("1", 100, None),
            pos("1", 200, Some("tanker")),
            pos("2", 150, Some("cargo")),
            pos("3", 50, Some("tanker")),
        ])
        .unwrap();
        s.insert_positions(&[pos("4", 160, None)]).unwrap();
        s.upsert_vessels(&[Vessel {
            mmsi: "4".into(),
            ship_type: Some(35),
            at: 170,
            ..Default::default()
        }])
        .unwrap();
        s.upsert_vessels(&[Vessel {
            mmsi: "2".into(),
            name: Some("EVER GIVEN".into()),
            destination: Some("JEBEL ALI".into()),
            at: 150,
            ..Default::default()
        }])
        .unwrap();
        let (total, classes) = s.by_class("ais", "hormuz", 90).unwrap();
        assert_eq!(total, 3);
        // Ship 4's position came before its details: classed from them.
        assert_eq!(
            classes,
            vec![
                ("cargo".into(), 1),
                ("military".into(), 1),
                ("tanker".into(), 1)
            ]
        );
        let latest = s.latest(Some("ais"), Some("hormuz"), 0, None, 10).unwrap();
        assert_eq!(latest.len(), 4);
        assert_eq!(latest[0]["id"], "1");
        assert_eq!(latest[0]["class"], "tanker");
        assert_eq!(latest[1]["class"], "military");
        assert_eq!(latest[2]["name"], "EVER GIVEN");
        assert_eq!(latest[2]["destination"], "JEBEL ALI");
        assert!(
            s.latest(Some("adsb"), None, 0, None, 10)
                .unwrap()
                .is_empty()
        );
        // The view: only what is inside it.
        let inside = s
            .latest(None, None, 0, Some([56.000_12, 25.0, 56.000_17, 27.0]), 10)
            .unwrap();
        assert_eq!(
            inside
                .iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["4", "2"]
        );
        let counts = s.area_counts(0).unwrap();
        assert_eq!(counts, vec![("hormuz".to_string(), "ais".to_string(), 4)]);
    }

    #[test]
    fn coverage_adds_up_and_keeps_the_most_ids() {
        let s = Store::memory();
        let c = |m, ids, up| Coverage {
            source: "ais",
            area: "red-sea".into(),
            hour: 7200,
            messages: m,
            ids,
            up_secs: up,
        };
        s.add_coverage(&[c(0, 0, 60), c(0, 0, 60)]).unwrap();
        assert_eq!(s.coverage("ais", "red-sea", 7300).unwrap(), (1, 120, 0));
        s.add_coverage(&[c(5, 3, 60), c(2, 2, 60)]).unwrap();
        assert_eq!(s.coverage("ais", "red-sea", 0).unwrap(), (1, 240, 7));
        assert_eq!(s.coverage("adsb", "red-sea", 0).unwrap(), (0, 0, 0));
    }

    #[test]
    fn old_rows_are_pruned() {
        let s = Store::memory();
        let now = 30 * 86_400;
        s.insert_positions(&[pos("1", 1, None), pos("2", now - 3600, None)])
            .unwrap();
        assert_eq!(s.prune(now).unwrap(), 1);
        assert_eq!(s.latest(None, None, 0, None, 10).unwrap().len(), 1);
    }
}
