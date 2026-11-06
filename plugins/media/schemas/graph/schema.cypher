// The media plugin's Neo4j constraints and indexes, applied by its `start`.
// Statements end with `;` and must be IF NOT EXISTS.

CREATE CONSTRAINT media_source_name IF NOT EXISTS FOR (m:MediaSource) REQUIRE m.name IS UNIQUE;
CREATE CONSTRAINT media_id IF NOT EXISTS FOR (m:Media) REQUIRE m.id IS UNIQUE;
// poll_media_sources scans active watchlist entries every tick.
CREATE INDEX media_source_active IF NOT EXISTS FOR (m:MediaSource) ON (m.active);
CREATE INDEX media_channel_idx IF NOT EXISTS FOR (m:Media) ON (m.channel_id);
CREATE INDEX media_ingested_idx IF NOT EXISTS FOR (m:Media) ON (m.ingested_at);
