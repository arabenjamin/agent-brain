/**
 * Where an entity is (MAP_PLAN Phase 1): its Wikidata coordinates on the
 * self-hosted map. Shown for places, and for anything else that has a point
 * (a headquarters, a venue). When there is no map, it says why.
 */
import { lazy, Suspense, useEffect, useState } from "react";
import { useEntityReference } from "../../api/wiki";
import { loadManifest, zoomForKind, type MapManifest } from "../../lib/map";
import { Section } from "./parts";

const PointMap = lazy(() => import("../map/PointMap"));

export default function EntityMap({ id, name, entityType, kind }: {
  id: string;
  name: string;
  entityType: string | null | undefined;
  kind: string | null | undefined;
}) {
  const q = useEntityReference(id);
  const [manifest, setManifest] = useState<MapManifest | null | undefined>(undefined);
  useEffect(() => { loadManifest().then(setManifest); }, []);
  const link = q.data?.link ?? null;
  const loc = link?.location ?? null;
  const point = loc && loc.lat != null && loc.lon != null ? { lat: loc.lat, lon: loc.lon } : null;
  const isPlace = entityType === "location";
  if (!q.data || (!point && !isPlace)) return null;

  let why: string | null = null;
  if (!link) why = "Not linked to Wikidata, so there are no coordinates. Link it under Reference.";
  else if (!loc) why = "Coordinates not read yet; they arrive with the next reference pass.";
  else if (!point) why = `Wikidata has no coordinates for ${link.label}.`;
  else if (manifest === null) why = "No map tiles on this server: run scripts/fetch_map_tiles.sh.";

  return (
    <Section
      title="Map"
      aside={point ? `${point.lat.toFixed(4)}, ${point.lon.toFixed(4)}${(loc?.coord_count ?? 0) > 1 ? ` · first of ${loc!.coord_count} coordinates` : ""}` : undefined}
    >
      {why ? (
        <div className="dw-dim">{why}</div>
      ) : point && manifest ? (
        <Suspense fallback={<div className="point-map" />}>
          <PointMap manifest={manifest} lat={point.lat} lon={point.lon} zoom={zoomForKind(kind)} label={name} />
        </Suspense>
      ) : (
        <div className="point-map" />
      )}
    </Section>
  );
}
