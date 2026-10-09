/**
 * The map's neighbourhood graph (HBI plan Phase 16): an entity, its strongest
 * neighbours, and the pairs among them that share notes. Expanding a node
 * merges its own neighbourhood in. Pure, so it is tested without a canvas.
 */
import type { NeighbourhoodResponse } from "../api/related";

export interface NbNode {
  id: string;
  label: string;
  entityType: string | null;
  mentions: number;
  topic: string | null;
  hiddenFrom: string[];
  /** The entity the view started from. */
  center: boolean;
  /** Its own neighbours have been loaded. */
  expanded: boolean;
  x?: number;
  y?: number;
  fx?: number;
  fy?: number;
}

export interface NbLink {
  source: string;
  target: string;
  /** `co`: named in the same notes; `typed`: a relationship (Phase 17). */
  kind: "co" | "typed";
  shared: number;
  strength: number;
  /** typed: how it reads ("works for"), its type and status. */
  label?: string;
  type?: string;
  status?: "active" | "proposed";
}

export interface NbGraph {
  nodes: NbNode[];
  links: NbLink[];
}

const key = (a: string, b: string) => (a < b ? `${a}|${b}` : `${b}|${a}`);
/** react-force-graph replaces link ends with node objects once laid out. */
const end = (v: unknown): string => (typeof v === "string" ? v : (v as { id: string }).id);

/** Merge one neighbourhood response into the graph; existing nodes keep their positions. */
export function mergeNeighbourhood(g: NbGraph, r: NeighbourhoodResponse, center: string): NbGraph {
  const nodes = new Map(g.nodes.map((n) => [n.id, n]));
  for (const n of r.nodes) {
    const old = nodes.get(n.id);
    nodes.set(n.id, {
      ...(old ?? {}),
      id: n.id,
      label: n.name ?? n.id,
      entityType: n.entity_type,
      mentions: n.mentions,
      topic: n.topic,
      hiddenFrom: n.hidden_from,
      center: old?.center ?? n.id === center,
      expanded: (old?.expanded ?? false) || n.id === r.center,
    });
  }
  const lkey = (l: { source: unknown; target: unknown; kind: string; type?: string }) =>
    `${l.kind}|${key(end(l.source), end(l.target))}|${l.type ?? ""}`;
  const links = new Map(g.links.map((l) => [lkey(l), l]));
  for (const l of r.links) {
    const nl: NbLink = { ...l, kind: "co" };
    if (!links.has(lkey(nl))) links.set(lkey(nl), nl);
  }
  for (const t of r.relations ?? []) {
    // Directed: source and target keep the relationship's direction.
    const nl: NbLink = { source: t.source, target: t.target, kind: "typed", shared: 0, strength: 0, label: t.label, type: t.type, status: t.status };
    const k = `typed|${t.source}>${t.target}|${t.type}`;
    if (![...links.values()].some((x) => x.kind === "typed" && end(x.source) === t.source && end(x.target) === t.target && x.type === t.type)) links.set(k, nl);
  }
  return { nodes: [...nodes.values()], links: [...links.values()] };
}

/** Node size: area grows with mentions, the centre a little larger. */
export function nodeVal(n: NbNode): number {
  return (n.center ? 6 : 2) + Math.sqrt(n.mentions);
}
