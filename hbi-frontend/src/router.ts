/**
 * Hash routing: `#/<workspace>?<param>=<value>&…`.
 *
 * The URL is the single source of truth for which workspace is shown and
 * which entity the drawer has open, so a notification, a chat citation or a
 * pasted link can land on a specific note. Hash (not path) routing because
 * nginx serves a static bundle with no SPA fallback, and because a hash change
 * never reloads the page — ChatPanel's in-flight stream survives navigation.
 */
import { useSyncExternalStore } from "react";

export interface Route {
  /** First path segment, e.g. "knowledge". Empty string for `#/` or no hash. */
  path: string;
  params: Record<string, string>;
}

export function parseHash(hash: string): Route {
  const raw = hash.replace(/^#/, "").replace(/^\//, "");
  const q = raw.indexOf("?");
  const pathPart = q === -1 ? raw : raw.slice(0, q);
  const queryPart = q === -1 ? "" : raw.slice(q + 1);
  const params: Record<string, string> = {};
  for (const [k, v] of new URLSearchParams(queryPart)) {
    if (k) params[k] = v;
  }
  return { path: decodeURIComponent(pathPart.split("/")[0] ?? ""), params };
}

export function formatHash(route: Route): string {
  const qs = new URLSearchParams();
  for (const k of Object.keys(route.params).sort()) {
    const v = route.params[k];
    if (v !== undefined && v !== "") qs.set(k, v);
  }
  const s = qs.toString();
  return `#/${encodeURIComponent(route.path)}${s ? `?${s}` : ""}`;
}

/**
 * Apply a partial update to a route. `params` entries set to `null` are
 * removed; omitted ones are kept.
 */
export function applyUpdate(
  current: Route,
  update: { path?: string; params?: Record<string, string | null> },
): Route {
  const params = { ...current.params };
  for (const [k, v] of Object.entries(update.params ?? {})) {
    if (v === null) delete params[k];
    else params[k] = v;
  }
  return { path: update.path ?? current.path, params };
}

// ── Store over window.location.hash ──────────────────────────────────────────

let cachedHash: string | null = null;
let cachedRoute: Route = { path: "", params: {} };

function snapshot(): Route {
  const h = window.location.hash;
  if (h !== cachedHash) {
    cachedHash = h;
    cachedRoute = parseHash(h);
  }
  return cachedRoute;
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener("hashchange", onChange);
  return () => window.removeEventListener("hashchange", onChange);
}

export function currentRoute(): Route {
  return snapshot();
}

export function useRoute(): Route {
  return useSyncExternalStore(subscribe, snapshot);
}

/**
 * Navigate. Pushes a history entry by default so Back works; `replace` for
 * corrections that shouldn't (e.g. normalising an unknown workspace).
 */
export function navigate(
  update: { path?: string; params?: Record<string, string | null> },
  opts: { replace?: boolean } = {},
): void {
  const next = formatHash(applyUpdate(snapshot(), update));
  if (next === window.location.hash) return;
  if (opts.replace) {
    history.replaceState(null, "", next);
    // replaceState doesn't fire hashchange; tell subscribers ourselves.
    window.dispatchEvent(new HashChangeEvent("hashchange"));
  } else {
    window.location.hash = next;
  }
}

/** A shareable absolute link to a route (for "copy link"). */
export function routeHref(route: Route): string {
  return `${window.location.origin}${window.location.pathname}${formatHash(route)}`;
}
