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
  shared: number;
  strength: number;
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
  const links = new Map(g.links.map((l) => [key(end(l.source), end(l.target)), l]));
  for (const l of r.links) {
    const k = key(l.source, l.target);
    if (!links.has(k)) links.set(k, { ...l });
  }
  return { nodes: [...nodes.values()], links: [...links.values()] };
}

/** Node size: area grows with mentions, the centre a little larger. */
export function nodeVal(n: NbNode): number {
  return (n.center ? 6 : 2) + Math.sqrt(n.mentions);
}
