// The maps plugin's Neo4j constraints, applied by its `start` before the
// watch areas are seeded. Statements end with `;` and must be IF NOT EXISTS.

// Places (geocode.rs): one node per normalised key.
CREATE CONSTRAINT place_key IF NOT EXISTS FOR (p:Place) REQUIRE p.key IS UNIQUE;
// Watch areas (tracks/areas.rs): one node per name.
CREATE CONSTRAINT watch_area_name IF NOT EXISTS FOR (a:WatchArea) REQUIRE a.name IS UNIQUE;
