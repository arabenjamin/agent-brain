-- The maps plugin's DuckDB tables (tracks.duckdb: positions, vessel details,
-- per-hour coverage), run whole on every open by tracks::Store::open
-- (plugins/maps/crate/src/tracks/store.rs). Every statement must be idempotent.

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
