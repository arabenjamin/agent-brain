/**
 * Moving between workspaces. A workspace's filters (`since`, `status`, …)
 * belong to it and are dropped on the way out; an open drawer is kept, so a
 * note stays open while the user looks at something else.
 */
import { DRAWER_KINDS } from "./components/drawer/entities";
import { currentRoute, formatHash, navigate } from "./router";

type Params = Record<string, string | number | undefined | null>;

function clean(params: Params): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== "") out[k] = String(v);
  }
  return out;
}

/** The params a move keeps: the open drawer entity, if any. */
export function keptParams(current: Record<string, string>): Record<string, string> {
  const kept: Record<string, string> = {};
  for (const k of DRAWER_KINDS) if (current[k]) kept[k] = current[k];
  return kept;
}

/** Go to a workspace (and view/filters), keeping only the open drawer. */
export function goTo(path: string, params: Params = {}): void {
  const cur = currentRoute();
  const next: Record<string, string | null> = {};
  for (const k of Object.keys(cur.params)) next[k] = null;
  Object.assign(next, keptParams(cur.params), clean(params));
  navigate({ path, params: next });
}

/** An `href` for a link to a workspace view — middle-click opens a tab. */
export function hrefTo(path: string, params: Params = {}): string {
  return formatHash({ path, params: clean(params) });
}
