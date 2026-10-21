/**
 * Map layers and hand-placed places (MAP_PLAN Phase 3, the `maps` plugin's
 * `routes.rs`). A layer is a named read-only query from a bundle's
 * `layers/*.yaml`; its points are fetched live, never stored.
 */
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, pluginApi, query } from "./client";
import { queryClient } from "./queries";

const MAPS = pluginApi("maps");

export interface LayerInfo {
  name: string;
  title: string;
  description: string;
  color: string;
  timed: boolean;
  private: boolean;
  /** Drawn when the panel opens. */
  on: boolean;
  owner: { program: string | null; topic: string | null; workflow: string | null } | null;
  plugin: string;
  /** Takes the map's view box (Phase 5b). */
  view: boolean;
  /** Not drawn below this zoom. */
  minzoom: number | null;
  /** Clustered when zoomed out. */
  cluster: boolean;
}

export interface LayerList {
  layers: LayerInfo[];
  /** Layer files that did not load, and why. */
  problems: { plugin: string; file: string; error: string }[];
}

export interface FeatureProps {
  label: string;
  id?: string;
  /** What the drawer opens for it (`entity`, `note`). */
  drawer?: string;
  kind?: string;
  at?: string;
  detail?: string;
  /** What the point is sized by (a count). */
  weight?: number;
}

export interface LayerFeatures {
  type: "FeatureCollection";
  layer: string;
  features: { type: "Feature"; geometry: { type: "Point"; coordinates: [number, number] }; properties: FeatureProps }[];
  count: number;
  /** Rows without a usable point. */
  skipped: number;
  truncated: boolean;
  note: string | null;
  /** Under the layer's minzoom: not queried. */
  hidden?: boolean;
}

export interface HandPlace {
  name: string;
  lat: number;
  lon: number;
  note: string | null;
  created_at: string | null;
  updated_at: string | null;
}

export function useLayers() {
  return useQuery({
    queryKey: ["maps", "layers"],
    queryFn: () => apiJson<LayerList>(`${MAPS}/layers`),
    staleTime: 60_000,
  });
}

/** One layer's points; `since` only reaches a timed layer, `view` (zoom
 * and box, `viewParams`) only a view layer. */
export function useLayerFeatures(name: string, enabled: boolean, since: string | null, view: Record<string, number>) {
  return useQuery({
    queryKey: ["maps", "layer", name, since, view],
    queryFn: () => apiJson<LayerFeatures>(`${MAPS}/layers/${encodeURIComponent(name)}${query({ since, ...view })}`),
    enabled,
    staleTime: 60_000,
    // Keep the points on screen while the next view loads.
    placeholderData: keepPreviousData,
  });
}

export function usePlaces() {
  return useQuery({
    queryKey: ["maps", "places"],
    queryFn: () => apiJson<{ places: HandPlace[]; count: number }>(`${MAPS}/places`),
  });
}

function refreshPlaces() {
  queryClient.invalidateQueries({ queryKey: ["maps", "places"] });
  queryClient.invalidateQueries({ queryKey: ["maps", "layer", "my-places"] });
}

export async function addPlace(p: { name: string; lat: number; lon: number; note?: string }) {
  const r = await apiSend<{ action: "added" | "moved" }>("POST", `${MAPS}/places`, p);
  refreshPlaces();
  return r;
}

export async function deletePlace(name: string) {
  await apiSend("DELETE", `${MAPS}/places/${encodeURIComponent(name)}`);
  refreshPlaces();
}

// ── Watch areas and tracks (MAP_PLAN Phase 4) ──────────────────────────────

export interface AreaCoverage {
  /** `covered`, `partial`, `too early`, `no coverage`, `not collected`. */
  state: string;
  says: string;
  reports: number;
  /** Distinct ships or aircraft seen in the list's window (`minutes`). */
  seen: number;
}

export interface CollectorStatus {
  state: string;
  detail: string | null;
  since: number | null;
  last_message_at: number | null;
  messages: number;
  stored: number;
  errors: number;
  areas: string[];
}

export interface AreaList {
  type: "FeatureCollection";
  features: {
    type: "Feature";
    geometry: { type: "Polygon"; coordinates: number[][][] };
    properties: {
      name: string;
      title: string;
      description: string;
      ships: boolean;
      aircraft: boolean;
      /** The last day's coverage, per watched kind (`ships`, `aircraft`). */
      coverage: Record<string, AreaCoverage>;
      on: boolean;
      /** `yaml` (a bundle file: can only be switched off) or `map` (drawn). */
      from: string;
      bbox: [number, number, number, number];
      center: [number, number];
      sq_deg: number;
    };
  }[];
  problems: string[];
  collectors: Record<string, CollectorStatus>;
  /** Why the tracks store did not open, if it did not. */
  store: string | null;
}

export interface TrackProps {
  source: "ais" | "adsb";
  id: string;
  /** The watch area (absent on a snapshot). */
  area?: string;
  name?: string;
  class?: string;
  speed?: number;
  course?: number;
  altitude?: number;
  destination?: string;
  at: number;
}

export interface Positions {
  type: "FeatureCollection";
  features: { type: "Feature"; geometry: { type: "Point"; coordinates: [number, number] }; properties: TrackProps }[];
  count: number;
  minutes: number;
  truncated: boolean;
  note: string | null;
}

export function useAreas() {
  return useQuery({
    queryKey: ["maps", "areas"],
    queryFn: () => apiJson<AreaList>(`${MAPS}/areas`),
    refetchInterval: 120_000,
  });
}

/** The latest positions, inside `view` (`viewParams`, the box only). */
export function usePositions(minutes: number, enabled: boolean, view: Record<string, number>) {
  // The box only: positions do not change with the zoom itself.
  const box = Object.fromEntries(Object.entries(view).filter(([k]) => k !== "zoom"));
  return useQuery({
    queryKey: ["maps", "positions", minutes, box],
    queryFn: () => apiJson<Positions>(`${MAPS}/areas/positions${query({ minutes, ...box })}`),
    enabled,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  });
}

function refreshAreas() {
  queryClient.invalidateQueries({ queryKey: ["maps", "areas"] });
}

export async function addArea(a: { title: string; bbox: [number, number, number, number]; ships: boolean; aircraft: boolean }) {
  const r = await apiSend<{ created: string }>("POST", `${MAPS}/areas`, a);
  refreshAreas();
  return r;
}

export async function setAreaOn(name: string, enabled: boolean) {
  await apiSend("PUT", `${MAPS}/areas/${encodeURIComponent(name)}`, { enabled });
  refreshAreas();
}

export async function deleteArea(name: string) {
  await apiSend("DELETE", `${MAPS}/areas/${encodeURIComponent(name)}`);
  refreshAreas();
}

export interface Snapshot extends Positions {
  at: number;
  credits_left: number | null;
}

/** One OpenSky look at the view (Phase 5d): shown, not stored. */
export function snapshotAircraft(bbox: [number, number, number, number]) {
  return apiSend<Snapshot>("POST", `${MAPS}/aircraft/snapshot`, { bbox });
}
