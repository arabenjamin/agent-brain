/**
 * A MapLibre map of one point over the self-hosted basemap (MAP_PLAN Phase 1).
 * Loaded lazily: maplibre-gl is the HBI's largest dependency, and most views
 * never show a map.
 */
import { useEffect, useRef, useState } from "react";
import { addProtocol, Map as MapLibreMap, Marker, NavigationControl, Popup, setWorkerUrl } from "maplibre-gl";
// MapLibre 6 decodes tiles in an ES-module worker that imports a shared
// chunk: Vite bundles it as a worker and hands back its URL.
import workerUrl from "maplibre-gl/dist/maplibre-gl-worker.mjs?worker&url";
import { Protocol } from "pmtiles";
import "maplibre-gl/dist/maplibre-gl.css";
import { basemapStyle, type MapManifest } from "../../lib/map";

let protocolAdded = false;

export default function PointMap({ manifest, lat, lon, zoom, label }: {
  manifest: MapManifest;
  lat: number;
  lon: number;
  zoom: number;
  label: string;
}) {
  const el = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!el.current) return;
    if (!protocolAdded) {
      setWorkerUrl(workerUrl);
      addProtocol("pmtiles", new Protocol().tile);
      protocolAdded = true;
    }
    const map = new MapLibreMap({
      container: el.current,
      style: basemapStyle(manifest),
      center: [lon, lat],
      zoom,
      attributionControl: { compact: true },
    });
    map.addControl(new NavigationControl({ showCompass: false }), "top-right");
    new Marker({ color: "#f59e0b" }).setLngLat([lon, lat]).setPopup(new Popup().setText(label)).addTo(map);
    map.on("error", (e) => setError(e.error?.message ?? "the map failed to load"));
    return () => map.remove();
  }, [manifest, lat, lon, zoom, label]);
  return (
    <>
      <div ref={el} className="point-map" role="img" aria-label={`Map of ${label}`} />
      {error && <div className="dw-dim tone-text-red">Map: {error}</div>}
    </>
  );
}
