/**
 * Map layers and hand-placed places (MAP_PLAN Phase 3, the `maps` plugin's
 * `routes.rs`). A layer is a named read-only query from a bundle's
 * `layers/*.yaml`; its points are fetched live, never stored.
 */
import { useQuery } from "@tanstack/react-query";
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

/** One layer's points; `since` only reaches a timed layer. */
export function useLayerFeatures(name: string, enabled: boolean, since: string | null) {
  return useQuery({
    queryKey: ["maps", "layer", name, since],
    queryFn: () => apiJson<LayerFeatures>(`${MAPS}/layers/${encodeURIComponent(name)}${query({ since })}`),
    enabled,
    staleTime: 60_000,
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
