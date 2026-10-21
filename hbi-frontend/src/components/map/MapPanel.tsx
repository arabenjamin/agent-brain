/**
 * The Map workspace (MAP_PLAN Phase 3): every layer the loaded bundles
 * declare (`layers/*.yaml`), drawn live over the self-hosted basemap, with a
 * time window for the layers that have a time, and places added by hand
 * (private: kept in the graph, never geocoded or sent anywhere, §5.2).
 *
 * Which layers exist comes from the brain, never a list in the code. Each
 * layer says what it could not show: rows cut at its limit, rows without a
 * point, an empty window.
 */
import { useEffect, useRef, useState, type MutableRefObject } from "react";
import { Map as MapLibreMap, Marker, NavigationControl, Popup, type GeoJSONSource, type MapLayerMouseEvent } from "maplibre-gl";
import { addPlace, deletePlace, useLayerFeatures, useLayers, usePlaces, type FeatureProps, type LayerInfo } from "../../api/maps";
import { basemapStyle, loadManifest, type MapManifest } from "../../lib/map";
import { DRAWER_KINDS, openEntity, type DrawerKind } from "../drawer/entities";
import type { FeatureCollection, Point } from "geojson";
import { ensureMapSetup } from "./setup";

const ON_KEY = "hbi.map.layers";
const DAYS_KEY = "hbi.map.days";
/** The time window's steps, in days; null = all time. */
const WINDOWS: (number | null)[] = [1, 7, 30, 90, 365, null];

function remembered<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : (JSON.parse(v) as T);
  } catch { return fallback; }
}
function remember(key: string, v: unknown) {
  try { localStorage.setItem(key, JSON.stringify(v)); } catch { /* not remembered */ }
}

/** The window's start: `days` before now, or null for all time. */
function sinceFor(days: number | null): string | null {
  return days === null ? null : new Date(Date.now() - days * 86_400_000).toISOString();
}

function windowLabel(days: number | null): string {
  if (days === null) return "All time";
  if (days === 1) return "Last day";
  if (days === 365) return "Last year";
  return `Last ${days} days`;
}

/** A small DOM popup: the label, then what is known, then the actions. Text only, never HTML. */
function popupContent(p: FeatureProps, actions: { label: string; run: () => void }[]): HTMLElement {
  const root = document.createElement("div");
  root.className = "map-popup";
  const title = document.createElement("div");
  title.className = "map-popup-title";
  title.textContent = p.label;
  root.appendChild(title);
  for (const line of [p.kind, p.detail, p.at ? new Date(p.at).toLocaleString() : null]) {
    if (!line) continue;
    const d = document.createElement("div");
    d.className = "map-popup-line";
    d.textContent = line;
    root.appendChild(d);
  }
  for (const a of actions) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "btn-ghost";
    b.textContent = a.label;
    b.onclick = a.run;
    root.appendChild(b);
  }
  return root;
}

export default function MapPanel() {
  const [manifest, setManifest] = useState<MapManifest | null | undefined>(undefined);
  useEffect(() => { loadManifest().then(setManifest); }, []);
  const layers = useLayers();
  const [on, setOn] = useState<Record<string, boolean>>(() => remembered(ON_KEY, {}));
  const [dayIdx, setDayIdx] = useState<number>(() => {
    const i = remembered<number>(DAYS_KEY, 2);
    return i >= 0 && i < WINDOWS.length ? i : 2;
  });
  const days = WINDOWS[dayIdx];
  // Fixed when the window is chosen, so a re-render never shifts it.
  const [since, setSince] = useState<string | null>(() => sinceFor(WINDOWS[dayIdx]));
  const [map, setMap] = useState<MapLibreMap | null>(null);
  const [mapError, setMapError] = useState<string | null>(null);
  const el = useRef<HTMLDivElement>(null);

  // Adding a place: the next click on the map drops a draft marker.
  const [placing, setPlacing] = useState(false);
  const placingRef = useRef(false);
  useEffect(() => { placingRef.current = placing; }, [placing]);
  const [draft, setDraft] = useState<{ lat: number; lon: number } | null>(null);
  const draftMarker = useRef<Marker | null>(null);

  useEffect(() => {
    if (!el.current || !manifest) return;
    ensureMapSetup();
    const m = new MapLibreMap({
      container: el.current,
      style: basemapStyle(manifest),
      center: [20, 25],
      zoom: 1.6,
      attributionControl: { compact: true },
    });
    m.addControl(new NavigationControl({ showCompass: false }), "top-right");
    m.on("error", (e) => setMapError(e.error?.message ?? "the map failed to load"));
    m.on("load", () => setMap(m));
    m.on("click", (e) => {
      if (!placingRef.current) return;
      setDraft({ lat: e.lngLat.lat, lon: e.lngLat.lng });
      setPlacing(false);
    });
    return () => { setMap(null); m.remove(); };
  }, [manifest]);

  useEffect(() => {
    draftMarker.current?.remove();
    draftMarker.current = null;
    if (map && draft) {
      draftMarker.current = new Marker({ color: "#22c55e", draggable: true })
        .setLngLat([draft.lon, draft.lat])
        .addTo(map);
      draftMarker.current.on("dragend", () => {
        const p = draftMarker.current?.getLngLat();
        if (p) setDraft({ lat: p.lat, lon: p.lng });
      });
    }
  }, [map, draft?.lat, draft?.lon]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (map) map.getCanvas().style.cursor = placing ? "crosshair" : "";
  }, [map, placing]);

  const list = layers.data?.layers ?? [];
  const isOn = (l: LayerInfo) => on[l.name] ?? l.on;
  const toggle = (l: LayerInfo) => setOn((o) => {
    const next = { ...o, [l.name]: !isOn(l) };
    remember(ON_KEY, next);
    return next;
  });
  const anyTimedOn = list.some((l) => l.timed && isOn(l));

  return (
    <div className="map-panel">
      <aside className="map-side">
        <div className="map-side-head">Layers</div>
        {layers.isLoading && <div className="dw-dim">Loading layers…</div>}
        {layers.error && <div className="tone-text-red">Layers: {String((layers.error as Error).message)}</div>}
        {layers.data && list.length === 0 && (
          <div className="dw-dim">No layers: no loaded plugin ships a layers/ folder.</div>
        )}
        {list.map((l) => (
          <LayerRow key={l.name} layer={l} on={isOn(l)} onToggle={() => toggle(l)} map={map}
            since={l.timed ? since : null} placingRef={placingRef} />
        ))}
        {(layers.data?.problems ?? []).map((p) => (
          <div key={p.file} className="tone-text-red map-problem" title={p.file}>
            {p.plugin}: a layer did not load: {p.error}
          </div>
        ))}

        {anyTimedOn && (
          <div className="map-window">
            <label htmlFor="map-window">{windowLabel(days)}</label>
            <input id="map-window" type="range" min={0} max={WINDOWS.length - 1} step={1} value={dayIdx}
              onChange={(e) => { const i = Number(e.target.value); setDayIdx(i); setSince(sinceFor(WINDOWS[i])); remember(DAYS_KEY, i); }} />
            <div className="dw-dim">For the layers with a time (marked ⏱).</div>
          </div>
        )}

        <MyPlaces placing={placing} setPlacing={setPlacing} draft={draft} setDraft={setDraft} map={map} />
      </aside>

      <div className="map-main">
        {manifest === null ? (
          <div className="dw-dim map-missing">No map tiles on this server: run scripts/fetch_map_tiles.sh.</div>
        ) : (
          <div ref={el} className="map-canvas" role="region" aria-label="Map" />
        )}
        {mapError && <div className="tone-text-red map-error">Map: {mapError}</div>}
        {placing && <div className="map-hint">Click the map where the place is.</div>}
      </div>
    </div>
  );
}

function LayerRow({ layer, on, onToggle, map, since, placingRef }: {
  layer: LayerInfo;
  on: boolean;
  onToggle: () => void;
  map: MapLibreMap | null;
  since: string | null;
  placingRef: MutableRefObject<boolean>;
}) {
  const q = useLayerFeatures(layer.name, on, since);
  const id = `layer:${layer.name}`;

  // Draw: one GeoJSON source and one circle layer per map layer.
  useEffect(() => {
    if (!map) return;
    const data = q.data ?? { type: "FeatureCollection" as const, features: [] };
    const src = map.getSource(id) as GeoJSONSource | undefined;
    if (src) src.setData(data as FeatureCollection);
    else {
      map.addSource(id, { type: "geojson", data: data as FeatureCollection });
      map.addLayer({
        id,
        type: "circle",
        source: id,
        paint: {
          "circle-color": layer.color,
          // A weighted point (a count) grows with its square root: 1 → 5 px, 400 → 24 px.
          "circle-radius": ["interpolate", ["linear"], ["sqrt", ["coalesce", ["get", "weight"], 1]], 1, 5, 20, 24],
          "circle-stroke-color": "#0b0e14",
          "circle-stroke-width": 1,
          "circle-opacity": 0.9,
        },
      });
    }
    map.setLayoutProperty(id, "visibility", on ? "visible" : "none");
  }, [map, q.data, on, id, layer.color]);

  // Click: what it is, and open it in the drawer when it has one.
  useEffect(() => {
    if (!map) return;
    const click = (e: MapLayerMouseEvent) => {
      if (placingRef.current) return;
      const f = e.features?.[0];
      if (!f) return;
      const p = f.properties as FeatureProps;
      const actions: { label: string; run: () => void }[] = [];
      if (p.id && p.drawer && (DRAWER_KINDS as readonly string[]).includes(p.drawer)) {
        actions.push({ label: "Open", run: () => openEntity(p.drawer as DrawerKind, p.id!) });
      }
      const [lon, lat] = (f.geometry as Point).coordinates;
      new Popup({ maxWidth: "280px" }).setLngLat([lon, lat]).setDOMContent(popupContent(p, actions)).addTo(map);
    };
    const enter = () => { if (!placingRef.current) map.getCanvas().style.cursor = "pointer"; };
    const leave = () => { if (!placingRef.current) map.getCanvas().style.cursor = ""; };
    map.on("click", id, click);
    map.on("mouseenter", id, enter);
    map.on("mouseleave", id, leave);
    return () => {
      map.off("click", id, click);
      map.off("mouseenter", id, enter);
      map.off("mouseleave", id, leave);
    };
  }, [map, id, placingRef]);

  const d = q.data;
  const said = [
    d && `${d.count}`,
    d && d.skipped > 0 && `${d.skipped} without a point`,
    d?.note,
  ].filter(Boolean).join(" · ");
  return (
    <div className="map-layer">
      <label className="map-layer-head" title={layer.description}>
        <input type="checkbox" checked={on} onChange={onToggle} />
        <span className="map-swatch" style={{ background: layer.color }} />
        <span className="map-layer-title">{layer.title}</span>
        {layer.timed && <span title="Filtered by the time window">⏱</span>}
        {layer.private && <span className="map-private" title="Private: never leaves the network">private</span>}
      </label>
      {on && (
        <div className="dw-dim map-layer-said">
          {q.isLoading ? "Loading…" : q.error ? <span className="tone-text-red">{String((q.error as Error).message)}</span> : said}
          {layer.owner && (
            <span> · {Object.entries(layer.owner).filter(([, v]) => v).map(([k, v]) => `${k} ${v}`).join("")}</span>
          )}
        </div>
      )}
    </div>
  );
}

function MyPlaces({ placing, setPlacing, draft, setDraft, map }: {
  placing: boolean;
  setPlacing: (v: boolean) => void;
  draft: { lat: number; lon: number } | null;
  setDraft: (v: { lat: number; lon: number } | null) => void;
  map: MapLibreMap | null;
}) {
  const places = usePlaces();
  const [name, setName] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<string | null>(null);

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    setSaid(null);
    try {
      const r = await addPlace({ name, lat: draft.lat, lon: draft.lon, note });
      setSaid(r.action === "moved" ? `Moved "${name.trim()}".` : `Added "${name.trim()}".`);
      setDraft(null);
      setName("");
      setNote("");
    } catch (e) {
      setSaid(String((e as Error).message));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (n: string) => {
    if (!window.confirm(`Remove "${n}" from your places?`)) return;
    try { await deletePlace(n); setSaid(`Removed "${n}".`); } catch (e) { setSaid(String((e as Error).message)); }
  };

  return (
    <div className="map-places">
      <div className="map-side-head">My places</div>
      <div className="dw-dim">Private: kept in the brain's graph, never geocoded or sent anywhere.</div>
      {!draft && (
        <button type="button" className="btn-ghost" disabled={!map} onClick={() => { setPlacing(!placing); setSaid(null); }}>
          {placing ? "Cancel" : "+ Add a place"}
        </button>
      )}
      {draft && (
        <div className="map-place-form">
          <div className="dw-dim">{draft.lat.toFixed(5)}, {draft.lon.toFixed(5)} (drag the marker to adjust)</div>
          <input type="text" placeholder="Name" value={name} maxLength={80} onChange={(e) => setName(e.target.value)} autoFocus />
          <input type="text" placeholder="Note (optional)" value={note} maxLength={500} onChange={(e) => setNote(e.target.value)} />
          <div className="map-place-actions">
            <button type="button" className="btn-ghost" disabled={busy || !name.trim()} onClick={save}>Save</button>
            <button type="button" className="btn-ghost" onClick={() => setDraft(null)}>Cancel</button>
          </div>
        </div>
      )}
      {said && <div className="dw-dim">{said}</div>}
      {places.data && places.data.count === 0 && !draft && <div className="dw-dim">None yet.</div>}
      {(places.data?.places ?? []).map((p) => (
        <div key={p.name} className="map-place">
          <button type="button" className="map-place-name" title={p.note ?? undefined}
            onClick={() => map?.flyTo({ center: [p.lon, p.lat], zoom: Math.max(map.getZoom(), 11) })}>
            {p.name}
          </button>
          <button type="button" className="ui-icon-btn" title={`Remove ${p.name}`} onClick={() => remove(p.name)}>✕</button>
        </div>
      ))}
    </div>
  );
}
