/**
 * Watch areas and what moves in them (MAP_PLAN Phases 4a and 5). Areas live
 * in the brain's graph: draw one on the map ("Watch an area"), switch any of
 * them off, delete a drawn one. The map follows the zoom:
 *
 * - zoomed out (under zoom 5): one bubble per area with how many ships and
 *   aircraft were seen, no outlines, no individual positions;
 * - zoomed in: the outlines, and every ship and aircraft seen in the view.
 *
 * Coverage is beside every area so an empty box reads as unseen, not quiet.
 * "Look here now" takes one OpenSky snapshot of a view nobody watches.
 */
import { useEffect, useRef, useState, type MutableRefObject } from "react";
import { Popup, type GeoJSONSource, type Map as MapLibreMap, type MapLayerMouseEvent, type MapMouseEvent } from "maplibre-gl";
import type { Feature, FeatureCollection } from "geojson";
import {
  addArea, deleteArea, setAreaOn, snapshotAircraft, useAreas, usePositions,
  type AreaList, type Snapshot, type TrackProps,
} from "../../api/maps";
import { viewParams, type MapView } from "../../lib/map";

const AREAS = "watch:areas";
const SUMMARY = "watch:summary";
const TRACKS = "watch:tracks";
const SNAP = "watch:snapshot";
const DRAW = "watch:draw";
const SHIP = "#22d3ee";
const AIRCRAFT = "#e879f9";
const KEY = "hbi.map.watch";
const WINDOWS = [15, 60, 360, 1440];
/** Below this zoom: area bubbles; from it: outlines and positions. */
const DETAIL_ZOOM = 5;
/** OpenSky's 2-credit box (MAX_AIRCRAFT_SQ_DEG in the plugin). */
const MAX_AIRCRAFT_SQ_DEG = 100;
const EMPTY: FeatureCollection = { type: "FeatureCollection", features: [] };

type Box = [number, number, number, number];

function remembered(): { tracks: boolean; minutes: number } {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (v && typeof v === "object") return { tracks: v.tracks !== false, minutes: WINDOWS.includes(v.minutes) ? v.minutes : 60 };
  } catch { /* defaults */ }
  return { tracks: true, minutes: 60 };
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
  "too early": "tone-text-muted",
  "no coverage": "tone-text-red",
  "not collected": "tone-text-muted",
};

function rect([w, s, e, n]: Box): Feature {
  return { type: "Feature", properties: {}, geometry: { type: "Polygon", coordinates: [[[w, s], [e, s], [e, n], [w, n], [w, s]]] } };
}

function trackPopup(p: TrackProps & { snapshot?: boolean }): HTMLElement {
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
  line(`${p.snapshot ? "Snapshot" : "Seen"} ${new Date(p.at * 1000).toLocaleString()}`, "map-popup-line");
  return root;
}

/** Area bubbles for the zoomed-out map: one point per area with its counts. */
function summary(d: AreaList | undefined): FeatureCollection {
  if (!d) return EMPTY;
  return {
    type: "FeatureCollection",
    features: d.features.filter((f) => f.properties.on).map((f) => {
      const c = f.properties.coverage;
      const parts = [
        c.ships ? (c.ships.state === "no coverage" ? "no ship coverage" : `${c.ships.seen} ships`) : null,
        c.aircraft ? (c.aircraft.state === "no coverage" ? "no aircraft coverage" : `${c.aircraft.seen} aircraft`) : null,
      ].filter(Boolean);
      return {
        type: "Feature",
        geometry: { type: "Point", coordinates: f.properties.center },
        properties: {
          name: f.properties.name,
          label: `${f.properties.title}\n${parts.join(" · ")}`,
          total: (c.ships?.seen ?? 0) + (c.aircraft?.seen ?? 0),
        },
      } as Feature;
    }),
  };
}

function ensure(map: MapLibreMap, id: string, add: () => void) {
  if (!map.getSource(id)) add();
}

export default function WatchAreas({ map, view, placingRef }: {
  map: MapLibreMap | null;
  view: MapView | null;
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
  const detail = (view?.zoom ?? 0) >= DETAIL_ZOOM;
  const positions = usePositions(opts.minutes, opts.tracks && detail && view != null, viewParams(view, DETAIL_ZOOM));
  const [selected, setSelected] = useState<string | null>(null);
  const [said, setSaid] = useState<string | null>(null);

  // Drawing a new area: press, drag, release.
  const [drawing, setDrawing] = useState(false);
  const drawingRef = useRef(false);
  useEffect(() => { drawingRef.current = drawing; }, [drawing]);
  const [box, setBox] = useState<Box | null>(null);
  const [title, setTitle] = useState("");
  const [ships, setShips] = useState(true);
  const [aircraft, setAircraft] = useState(true);
  const [busy, setBusy] = useState(false);

  // Look here now.
  const [snap, setSnap] = useState<Snapshot | null>(null);

  // Sources and layers, once.
  useEffect(() => {
    if (!map) return;
    ensure(map, AREAS, () => {
      map.addSource(AREAS, { type: "geojson", data: EMPTY });
      map.addLayer({ id: `${AREAS}:fill`, type: "fill", source: AREAS, minzoom: 4, paint: { "fill-color": "#f59e0b", "fill-opacity": ["case", ["boolean", ["get", "selected"], false], 0.12, 0.04] } });
      map.addLayer({
        id: `${AREAS}:line`, type: "line", source: AREAS, minzoom: 4,
        paint: { "line-color": ["case", ["get", "on"], "#f59e0b", "#6b7280"], "line-width": ["case", ["boolean", ["get", "selected"], false], 2.5, 1.2], "line-dasharray": [3, 2] },
      });
    });
    ensure(map, SUMMARY, () => {
      map.addSource(SUMMARY, { type: "geojson", data: EMPTY });
      map.addLayer({
        id: SUMMARY, type: "circle", source: SUMMARY, maxzoom: DETAIL_ZOOM,
        paint: {
          "circle-color": "#f59e0b", "circle-opacity": 0.25, "circle-stroke-color": "#f59e0b", "circle-stroke-width": 1.5,
          "circle-radius": ["interpolate", ["linear"], ["sqrt", ["get", "total"]], 0, 8, 30, 28],
        },
      });
      map.addLayer({
        id: `${SUMMARY}:label`, type: "symbol", source: SUMMARY, maxzoom: DETAIL_ZOOM,
        layout: { "text-field": ["get", "label"], "text-font": ["Noto Sans Medium"], "text-size": 11, "text-offset": [0, 2.2], "text-anchor": "top" },
        paint: { "text-color": "#fbbf24", "text-halo-color": "#0b0e14", "text-halo-width": 1.5 },
      });
    });
    ensure(map, TRACKS, () => {
      map.addSource(TRACKS, { type: "geojson", data: EMPTY });
      map.addLayer({
        id: TRACKS, type: "circle", source: TRACKS, minzoom: DETAIL_ZOOM,
        paint: {
          "circle-color": ["match", ["get", "source"], "ais", SHIP, AIRCRAFT],
          "circle-radius": 3.5, "circle-stroke-color": "#0b0e14", "circle-stroke-width": 0.8,
        },
      });
    });
    ensure(map, SNAP, () => {
      map.addSource(SNAP, { type: "geojson", data: EMPTY });
      map.addLayer({
        id: SNAP, type: "circle", source: SNAP,
        paint: { "circle-color": "rgba(0,0,0,0)", "circle-radius": 5, "circle-stroke-color": AIRCRAFT, "circle-stroke-width": 2 },
      });
    });
    ensure(map, DRAW, () => {
      map.addSource(DRAW, { type: "geojson", data: EMPTY });
      map.addLayer({ id: DRAW, type: "line", source: DRAW, paint: { "line-color": "#22c55e", "line-width": 2 } });
    });
  }, [map]);

  // Data into them.
  useEffect(() => {
    if (!map) return;
    const fc: FeatureCollection = {
      type: "FeatureCollection",
      features: (areas.data?.features ?? []).map((f) => ({ ...f, properties: { ...f.properties, selected: f.properties.name === selected } })) as Feature[],
    };
    (map.getSource(AREAS) as GeoJSONSource | undefined)?.setData(fc);
    (map.getSource(SUMMARY) as GeoJSONSource | undefined)?.setData(summary(areas.data));
  }, [map, areas.data, selected]);

  useEffect(() => {
    if (!map) return;
    (map.getSource(TRACKS) as GeoJSONSource | undefined)?.setData((positions.data ?? EMPTY) as FeatureCollection);
    if (map.getLayer(TRACKS)) map.setLayoutProperty(TRACKS, "visibility", opts.tracks ? "visible" : "none");
  }, [map, positions.data, opts.tracks]);

  useEffect(() => {
    if (!map) return;
    (map.getSource(SNAP) as GeoJSONSource | undefined)?.setData((snap ?? EMPTY) as FeatureCollection);
  }, [map, snap]);

  useEffect(() => {
    if (!map) return;
    (map.getSource(DRAW) as GeoJSONSource | undefined)?.setData(box ? { type: "FeatureCollection", features: [rect(box)] } : EMPTY);
  }, [map, box]);

  // Clicks: a position's popup; a bubble zooms to its area.
  useEffect(() => {
    if (!map) return;
    const pop = (e: MapLayerMouseEvent) => {
      if (placingRef.current || drawingRef.current) return;
      const f = e.features?.[0];
      if (!f) return;
      new Popup({ maxWidth: "280px" }).setLngLat(e.lngLat).setDOMContent(trackPopup(f.properties as unknown as TrackProps)).addTo(map);
    };
    const bubble = (e: MapLayerMouseEvent) => {
      const name = e.features?.[0]?.properties?.name;
      const a = areas.data?.features.find((f) => f.properties.name === name);
      if (!a) return;
      setSelected(a.properties.name);
      map.fitBounds(a.properties.bbox, { padding: 40 });
    };
    map.on("click", TRACKS, pop);
    map.on("click", SNAP, pop);
    map.on("click", SUMMARY, bubble);
    return () => { map.off("click", TRACKS, pop); map.off("click", SNAP, pop); map.off("click", SUMMARY, bubble); };
  }, [map, placingRef, areas.data]);

  // Drawing: the map stops panning while a box is dragged.
  useEffect(() => {
    if (!map || !drawing) return;
    map.dragPan.disable();
    map.getCanvas().style.cursor = "crosshair";
    let start: [number, number] | null = null;
    const toBox = (a: [number, number], b: [number, number]): Box =>
      [Math.min(a[0], b[0]), Math.min(a[1], b[1]), Math.max(a[0], b[0]), Math.max(a[1], b[1])].map((x) => Math.round(x * 100) / 100) as Box;
    const down = (e: MapMouseEvent) => { start = [e.lngLat.lng, e.lngLat.lat]; };
    const move = (e: MapMouseEvent) => { if (start) setBox(toBox(start, [e.lngLat.lng, e.lngLat.lat])); };
    const up = (e: MapMouseEvent) => {
      if (!start) return;
      const b = toBox(start, [e.lngLat.lng, e.lngLat.lat]);
      start = null;
      if (b[2] - b[0] < 0.05 || b[3] - b[1] < 0.05) { setBox(null); return; }
      setBox(b);
      setDrawing(false);
    };
    map.on("mousedown", down);
    map.on("mousemove", move);
    map.on("mouseup", up);
    return () => {
      map.off("mousedown", down);
      map.off("mousemove", move);
      map.off("mouseup", up);
      map.dragPan.enable();
      map.getCanvas().style.cursor = "";
    };
  }, [map, drawing]);

  const boxSq = box ? (box[2] - box[0]) * (box[3] - box[1]) : 0;
  const aircraftTooBig = boxSq > MAX_AIRCRAFT_SQ_DEG;

  const save = async () => {
    if (!box) return;
    setBusy(true);
    setSaid(null);
    try {
      const r = await addArea({ title: title.trim(), bbox: box, ships, aircraft: aircraft && !aircraftTooBig });
      setSaid(`Watching "${title.trim()}" (${r.created}): the collectors pick it up now.`);
      setSelected(r.created);
      setBox(null);
      setTitle("");
    } catch (e) {
      setSaid(String((e as Error).message));
    } finally {
      setBusy(false);
    }
  };

  const lookHere = async () => {
    if (!view?.bbox) return;
    setSaid(null);
    try {
      setSnap(await snapshotAircraft(view.bbox));
    } catch (e) {
      setSaid(String((e as Error).message));
    }
  };
  const lookWhy = !view?.bbox || view.zoom < DETAIL_ZOOM
    ? `Zoom in (to zoom ${DETAIL_ZOOM}) to look here.`
    : view.sqDeg > MAX_AIRCRAFT_SQ_DEG
      ? `The view is ${Math.round(view.sqDeg)} square degrees: zoom in to ${MAX_AIRCRAFT_SQ_DEG} or less.`
      : null;

  if (areas.isLoading) return null;
  if (areas.error) return <div className="tone-text-red map-problem">Watch areas: {String((areas.error as Error).message)}</div>;
  const d = areas.data;
  if (!d) return null;
  const collectors = Object.entries(d.collectors ?? {});

  return (
    <div className="map-watch">
      <div className="map-side-head">Watch areas</div>
      {d.store && <div className="tone-text-red map-problem">{d.store}</div>}
      <div className="dw-dim">
        {detail ? "Outlines and every ship and aircraft in view." : `Zoomed out: one bubble per area. Zoom in (to ${DETAIL_ZOOM}) for outlines and positions.`}
      </div>

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
          {!detail ? null : positions.isLoading ? "Loading…" : positions.error ? <span className="tone-text-red">{String((positions.error as Error).message)}</span>
            : positions.data ? `${positions.data.count} in view${positions.data.note ? ` · ${positions.data.note}` : ""}` : null}
        </div>
      )}

      {!hasAreas && <div className="dw-dim">None yet: draw one.</div>}
      {d.features.map((f) => {
        const p = f.properties;
        return (
          <div key={p.name} className={`map-area${p.name === selected ? " selected" : ""}${p.on ? "" : " off"}`} title={p.description}>
            <div className="map-area-head">
              <input type="checkbox" checked={p.on} aria-label={`Watch ${p.title}`}
                onChange={() => setAreaOn(p.name, !p.on).catch((e) => setSaid(String((e as Error).message)))} />
              <button type="button" className="map-place-name" onClick={() => { setSelected(p.name); map?.fitBounds(p.bbox, { padding: 40 }); }}>
                {p.title}
              </button>
              {p.from === "yaml"
                ? <span className="map-private" title="From a bundle file: change it there; here it can only be switched off">file</span>
                : <button type="button" className="ui-icon-btn" title={`Stop watching ${p.title}`}
                    onClick={() => { if (window.confirm(`Stop watching "${p.title}" and delete it?`)) deleteArea(p.name).catch((e) => setSaid(String((e as Error).message))); }}>✕</button>}
            </div>
            {Object.entries(p.coverage).map(([what, c]) => (
              <div key={what} className="map-layer-said">
                <span className={TONE[c.state] ?? ""} title={c.says}>{what}: {c.state}</span>
                {c.reports > 0 && <span className="dw-dim"> · {c.reports.toLocaleString()} reports in the last day</span>}
              </div>
            ))}
          </div>
        );
      })}

      {!box && (
        <button type="button" className="btn-ghost" disabled={!map} onClick={() => { setDrawing(!drawing); setSaid(null); }}>
          {drawing ? "Cancel" : "+ Watch an area"}
        </button>
      )}
      {drawing && <div className="dw-dim">Drag a box on the map.</div>}
      {box && (
        <div className="map-place-form">
          <div className="dw-dim">{box.join(", ")} · {Math.round(boxSq)} square degrees</div>
          <input type="text" placeholder="Name it (e.g. Gulf of Aden)" value={title} maxLength={80} onChange={(e) => setTitle(e.target.value)} autoFocus />
          <label className="map-layer-head"><input type="checkbox" checked={ships} onChange={() => setShips(!ships)} /> Ships (aisstream)</label>
          <label className="map-layer-head" title={aircraftTooBig ? "OpenSky charges more above 100 square degrees" : undefined}>
            <input type="checkbox" checked={aircraft && !aircraftTooBig} disabled={aircraftTooBig} onChange={() => setAircraft(!aircraft)} /> Aircraft (OpenSky)
          </label>
          {aircraftTooBig && <div className="dw-dim">Aircraft only up to {MAX_AIRCRAFT_SQ_DEG} square degrees: draw a smaller box for them.</div>}
          <div className="map-place-actions">
            <button type="button" className="btn-ghost" disabled={busy || !title.trim() || (!ships && !(aircraft && !aircraftTooBig))} onClick={save}>Watch it</button>
            <button type="button" className="btn-ghost" onClick={() => setBox(null)}>Cancel</button>
          </div>
        </div>
      )}

      <div className="map-look">
        <button type="button" className="btn-ghost" disabled={!!lookWhy} title={lookWhy ?? "One OpenSky snapshot of this view (1 to 2 credits; not stored)"} onClick={lookHere}>
          Look here now (aircraft)
        </button>
        {lookWhy && <div className="dw-dim">{lookWhy}</div>}
        {snap && (
          <div className="dw-dim map-layer-said">
            Snapshot {new Date(snap.at * 1000).toLocaleTimeString()}: {snap.count} aircraft
            {snap.credits_left != null ? ` · ${snap.credits_left} credits left today` : ""}
            {snap.note ? ` · ${snap.note}` : ""}{" "}
            <button type="button" className="btn-ghost" onClick={() => setSnap(null)}>Clear</button>
          </div>
        )}
      </div>

      {said && <div className="dw-dim">{said}</div>}
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
