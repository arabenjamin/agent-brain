/**
 * The self-hosted basemap (MAP_PLAN.md): Protomaps vector tiles served by the
 * HBI at /maps/, fetched by scripts/fetch_map_tiles.sh. Which tile sources
 * exist comes from /maps/manifest.json, never a list in the code: the world,
 * then any regions extracted in detail, drawn on top from the world's
 * maximum zoom up. Fonts and sprites are local too, so viewing a map never
 * leaves the network.
 */
import { layers, namedFlavor } from "@protomaps/basemaps";
import type { LayerSpecification, StyleSpecification } from "maplibre-gl";

export interface TileSource {
  name: string;
  file: string;
  maxzoom: number;
  bbox: [number, number, number, number];
  label: string;
}

export interface MapManifest {
  build: string;
  fetched_at: string;
  sources: TileSource[];
}

let manifest: Promise<MapManifest | null> | null = null;

/** The tile manifest, or null when no tiles were fetched (said, not hidden). */
export function loadManifest(): Promise<MapManifest | null> {
  // `no-cache`: revalidate even a copy the browser was told to keep (a
  // day-old manifest hid newly fetched regions).
  manifest ??= fetch("/maps/manifest.json", { cache: "no-cache" })
    .then((r) => (r.ok ? (r.json() as Promise<MapManifest>) : null))
    .catch(() => null);
  return manifest;
}

const ATTRIBUTION =
  '<a href="https://protomaps.com" target="_blank" rel="noreferrer">Protomaps</a> © <a href="https://openstreetmap.org/copyright" target="_blank" rel="noreferrer">OpenStreetMap</a>';

export function basemapStyle(m: MapManifest): StyleSpecification {
  const origin = window.location.origin;
  const flavor = namedFlavor("dark");
  const world = m.sources.find((s) => s.name === "world");
  const sources: StyleSpecification["sources"] = {};
  const all: LayerSpecification[] = [];
  for (const s of m.sources) {
    sources[s.name] = {
      type: "vector",
      url: `pmtiles://${origin}/maps/${s.file}`,
      attribution: ATTRIBUTION,
    };
    const ls = layers(s.name, flavor, { lang: "en" }) as LayerSpecification[];
    for (const l of ls) {
      // One background, the world's: a background layer is not clipped to
      // its source, so a region's painted over every region before it (with
      // the US, Alaska's and Hawaii's hid the contiguous US's roads and
      // buildings, leaving only labels: 2026-10-21).
      if (l.type === "background" && s.name !== "world") continue;
      // A region's detail draws over the world from where the world's tiles end.
      const detail = s.name !== "world" && world ? { minzoom: Math.max(l.minzoom ?? 0, world.maxzoom) } : {};
      all.push({ ...l, id: `${s.name}:${l.id}`, ...detail } as LayerSpecification);
    }
  }
  // Labels last, so a region's fills never cover the world's labels.
  const symbols = all.filter((l) => l.type === "symbol");
  return {
    version: 8,
    glyphs: `${origin}/maps/assets/fonts/{fontstack}/{range}.pbf`,
    sprite: `${origin}/maps/assets/sprites/v4/dark`,
    sources,
    layers: [...all.filter((l) => l.type !== "symbol"), ...symbols],
  };
}

/** How close to start, by what kind of place it is. */
export function zoomForKind(kind: string | null | undefined): number {
  switch (kind) {
    case "country": return 3.5;
    case "state or province":
    case "region": return 5;
    case "county":
    case "geographic feature": return 7.5;
    case "municipality": return 10;
    case "facility": return 13;
    default: return 8;
  }
}

/** What the map shows now (MAP_PLAN Phase 5b): its zoom and, when it is a
 * plain box, its bounds `[west, south, east, north]`. A view across the
 * antimeridian, or wider than half the world, is `bbox: null`: the whole
 * world, which clustering handles. */
export interface MapView {
  zoom: number;
  bbox: [number, number, number, number] | null;
  /** The view's size in square degrees (Infinity for the whole world). */
  sqDeg: number;
}

const r3 = (x: number) => Math.round(x * 1000) / 1000;

export function viewOf(map: { getZoom(): number; getBounds(): { getWest(): number; getSouth(): number; getEast(): number; getNorth(): number } }): MapView {
  const b = map.getBounds();
  const [w, s, e, n] = [b.getWest(), b.getSouth(), b.getEast(), b.getNorth()];
  const zoom = Math.round(map.getZoom() * 10) / 10;
  if (w < -180 || e > 180 || e - w >= 180) return { zoom, bbox: null, sqDeg: Infinity };
  const bbox: [number, number, number, number] = [r3(w), r3(Math.max(s, -90)), r3(e), r3(Math.min(n, 90))];
  return { zoom, bbox, sqDeg: (bbox[2] - bbox[0]) * (bbox[3] - bbox[1]) };
}

/** Query params for a view: the box only from `fromZoom` in (below it the
 * whole world is cheap and clustered). */
export function viewParams(v: MapView | null, fromZoom = 3): Record<string, number> {
  if (!v) return {};
  const p: Record<string, number> = { zoom: v.zoom };
  if (v.bbox && v.zoom >= fromZoom) {
    [p.west, p.south, p.east, p.north] = v.bbox;
  }
  return p;
}
