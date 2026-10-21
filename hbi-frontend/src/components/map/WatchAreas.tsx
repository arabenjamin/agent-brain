/**
 * Watch areas and what moves in them (MAP_PLAN Phase 4): each area's outline
 * with its last day's coverage, and the latest position of every ship (AIS)
 * and aircraft (ADS-B) the collectors saw. Coverage is shown beside every
 * area so an empty box is read as unseen, not quiet.
 */
import { useEffect, useState, type MutableRefObject } from "react";
import { Popup, type GeoJSONSource, type Map as MapLibreMap, type MapLayerMouseEvent } from "maplibre-gl";
import type { FeatureCollection } from "geojson";
import { useAreas, usePositions, type TrackProps } from "../../api/maps";

const AREAS = "watch:areas";
const TRACKS = "watch:tracks";
const SHIP = "#22d3ee";
const AIRCRAFT = "#e879f9";
const KEY = "hbi.map.watch";
const WINDOWS = [15, 60, 360, 1440];

function remembered(): { areas: boolean; tracks: boolean; minutes: number } {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (v && typeof v === "object") return { areas: v.areas !== false, tracks: v.tracks !== false, minutes: WINDOWS.includes(v.minutes) ? v.minutes : 60 };
  } catch { /* defaults */ }
  return { areas: true, tracks: true, minutes: 60 };
}

function ago(t: number | null | undefined): string {
  if (!t) return "never";
  const s = Math.max(0, Math.round(Date.now() / 1000 - t));
  if (s < 90) return `${s}s ago`;
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  return `${Math.round(s / 3600)} h ago`;
}

function windowLabel(m: number): string {
  return m < 60 ? `${m} min` : m === 60 ? "1 hour" : `${m / 60} hours`;
}

const TONE: Record<string, string> = {
  covered: "tone-text-green",
  partial: "tone-text-yellow",
  "no coverage": "tone-text-red",
  "too early": "tone-text-muted",
  "not collected": "tone-text-muted",
};

function trackPopup(p: TrackProps): HTMLElement {
  const root = document.createElement("div");
  root.className = "map-popup";
  const line = (text: string, cls: string) => {
    const d = document.createElement("div");
    d.className = cls;
    d.textContent = text;
    root.appendChild(d);
  };
  line(p.name ?? p.id, "map-popup-title");
  line(`${p.source === "ais" ? "Ship, MMSI" : "Aircraft, ICAO"} ${p.id}${p.class ? ` · ${p.class}` : ""}`, "map-popup-line");
  const motion = [
    p.speed != null ? (p.source === "ais" ? `${p.speed.toFixed(1)} kn` : `${Math.round(p.speed * 1.944)} kn`) : null,
    p.course != null ? `${Math.round(p.course)}°` : null,
    p.altitude != null ? `${Math.round(p.altitude)} m` : null,
  ].filter(Boolean).join(" · ");
  if (motion) line(motion, "map-popup-line");
  if (p.destination) line(`Bound for ${p.destination}`, "map-popup-line");
  line(`Seen ${new Date(p.at * 1000).toLocaleString()}`, "map-popup-line");
  return root;
}

export default function WatchAreas({ map, placingRef }: {
  map: MapLibreMap | null;
  placingRef: MutableRefObject<boolean>;
}) {
  const [opts, setOpts] = useState(remembered);
  const set = (o: Partial<typeof opts>) => setOpts((c) => {
    const n = { ...c, ...o };
    try { localStorage.setItem(KEY, JSON.stringify(n)); } catch { /* not remembered */ }
    return n;
  });
  const areas = useAreas();
  const hasAreas = (areas.data?.features.length ?? 0) > 0;
  const positions = usePositions(opts.minutes, hasAreas && opts.tracks);

  // Outlines.
  useEffect(() => {
    if (!map) return;
    const data = (areas.data ?? { type: "FeatureCollection", features: [] }) as unknown as FeatureCollection;
    const src = map.getSource(AREAS) as GeoJSONSource | undefined;
    if (src) src.setData(data);
    else {
      map.addSource(AREAS, { type: "geojson", data });
      map.addLayer({ id: `${AREAS}:fill`, type: "fill", source: AREAS, paint: { "fill-color": "#f59e0b", "fill-opacity": 0.05 } });
      map.addLayer({ id: `${AREAS}:line`, type: "line", source: AREAS, paint: { "line-color": "#f59e0b", "line-width": 1.2, "line-dasharray": [3, 2] } });
    }
    for (const id of [`${AREAS}:fill`, `${AREAS}:line`]) map.setLayoutProperty(id, "visibility", opts.areas ? "visible" : "none");
  }, [map, areas.data, opts.areas]);

  // Ships and aircraft.
  useEffect(() => {
    if (!map) return;
    const data = (positions.data ?? { type: "FeatureCollection", features: [] }) as unknown as FeatureCollection;
    const src = map.getSource(TRACKS) as GeoJSONSource | undefined;
    if (src) src.setData(data);
    else {
      map.addSource(TRACKS, { type: "geojson", data });
      map.addLayer({
        id: TRACKS,
        type: "circle",
        source: TRACKS,
        paint: {
          "circle-color": ["match", ["get", "source"], "ais", SHIP, AIRCRAFT],
          "circle-radius": 3.5,
          "circle-stroke-color": "#0b0e14",
          "circle-stroke-width": 0.8,
        },
      });
    }
    map.setLayoutProperty(TRACKS, "visibility", opts.tracks ? "visible" : "none");
  }, [map, positions.data, opts.tracks]);

  useEffect(() => {
    if (!map) return;
    const click = (e: MapLayerMouseEvent) => {
      if (placingRef.current) return;
      const f = e.features?.[0];
      if (!f) return;
      new Popup({ maxWidth: "280px" }).setLngLat(e.lngLat).setDOMContent(trackPopup(f.properties as unknown as TrackProps)).addTo(map);
    };
    map.on("click", TRACKS, click);
    return () => { map.off("click", TRACKS, click); };
  }, [map, placingRef]);

  if (areas.isLoading) return null;
  if (areas.error) return <div className="tone-text-red map-problem">Watch areas: {String((areas.error as Error).message)}</div>;
  const d = areas.data;
  if (!d) return null;
  const collectors = Object.entries(d.collectors ?? {});

  return (
    <div className="map-watch">
      <div className="map-side-head">Watch areas</div>
      {!hasAreas && <div className="dw-dim">None: a bundle's areas/*.yaml declares them.</div>}
      {d.store && <div className="tone-text-red map-problem">{d.store}</div>}
      {hasAreas && (
        <>
          <label className="map-layer-head">
            <input type="checkbox" checked={opts.areas} onChange={() => set({ areas: !opts.areas })} />
            <span className="map-swatch" style={{ background: "#f59e0b" }} />
            <span className="map-layer-title">Outlines</span>
          </label>
          <label className="map-layer-head">
            <input type="checkbox" checked={opts.tracks} onChange={() => set({ tracks: !opts.tracks })} />
            <span className="map-swatch" style={{ background: SHIP }} />
            <span className="map-swatch" style={{ background: AIRCRAFT }} />
            <span className="map-layer-title">Ships and aircraft</span>
          </label>
          {opts.tracks && (
            <div className="dw-dim map-layer-said">
              <select value={opts.minutes} onChange={(e) => set({ minutes: Number(e.target.value) })} aria-label="Seen within">
                {WINDOWS.map((m) => <option key={m} value={m}>last {windowLabel(m)}</option>)}
              </select>{" "}
              {positions.isLoading ? "Loading…" : positions.error ? <span className="tone-text-red">{String((positions.error as Error).message)}</span>
                : positions.data ? `${positions.data.count} seen${positions.data.note ? ` · ${positions.data.note}` : ""}` : null}
            </div>
          )}
          {d.features.map((f) => (
            <div key={f.properties.name} className="map-area" title={f.properties.description}>
              <div className="map-area-title">{f.properties.title}</div>
              {Object.entries(f.properties.coverage).map(([what, c]) => (
                <div key={what} className="map-layer-said">
                  <span className={TONE[c.state] ?? ""} title={c.says}>{what}: {c.state}</span>
                  {c.reports > 0 && <span className="dw-dim"> · {c.reports.toLocaleString()} reports in the last day</span>}
                </div>
              ))}
            </div>
          ))}
        </>
      )}
      {collectors.map(([source, s]) => (
        <div key={source} className="dw-dim map-layer-said" title={s.detail ?? undefined}>
          {source === "ais" ? "Ships (aisstream)" : "Aircraft (OpenSky)"}: <span className={s.state === "running" ? "tone-text-green" : s.state === "off" ? "" : "tone-text-yellow"}>{s.state}</span>
          {s.detail ? ` · ${s.detail}` : ""} · last heard {ago(s.last_message_at)}
        </div>
      ))}
      {(d.problems ?? []).map((p) => <div key={p} className="tone-text-red map-problem">{p}</div>)}
    </div>
  );
}
