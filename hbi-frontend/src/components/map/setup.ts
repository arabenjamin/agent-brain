/**
 * One-time MapLibre setup shared by every map: the tile worker (MapLibre 6
 * decodes tiles in an ES-module worker that imports a shared chunk; Vite
 * bundles it as a worker and hands back its URL) and the `pmtiles://`
 * protocol the self-hosted basemap is read through.
 */
import { addProtocol, setWorkerUrl } from "maplibre-gl";
import workerUrl from "maplibre-gl/dist/maplibre-gl-worker.mjs?worker&url";
import { Protocol } from "pmtiles";
import "maplibre-gl/dist/maplibre-gl.css";

let done = false;

export function ensureMapSetup(): void {
  if (done) return;
  setWorkerUrl(workerUrl);
  addProtocol("pmtiles", new Protocol().tile);
  done = true;
}
