/**
 * The topic map's graph, built from `/api/topics/map` (pure, so it is tested
 * without a canvas). See `components/knowledge/TopicMap.tsx`.
 */
import type { MapEntity, TopicMap as MapData } from "../api/topics";

export type SizeBy = "connections" | "certainty" | "activity";

/** Entities named at any zoom; the rest appear on zooming in. */
export const LABEL_BUDGET = 30;

/** One hue per topic, in the order the map returns them. */
const HUES = [212, 28, 140, 280, 355, 48, 180, 320, 95, 250, 10, 160];

export interface MapNode {
  id: string;
  kind: "topic" | "entity";
  label: string;
  hue: number;
  /** Area-ish: what `nodeVal` gets. */
  val: number;
  topics: number;
  detail: string;
  /** Drawn with its name at any zoom (topics, and the largest entities). */
  labelled: boolean;
  x?: number;
  y?: number;
  fx?: number;
  fy?: number;
}

export interface MapLink {
  source: string;
  target: string;
  notes: number;
}

export function metric(e: MapEntity, by: SizeBy): number {
  if (by === "connections") return e.mentions;
  if (by === "activity") return e.week;
  return e.claims > 0 && e.certainty !== null ? e.certainty : 0;
}

/** Build the graph. Pure, so it can be tested without a canvas. */
export function buildMap(d: MapData, by: SizeBy): { nodes: MapNode[]; links: MapLink[] } {
  const hue = new Map(d.topics.map((t, i) => [t.name, HUES[i % HUES.length]]));
  const byEntity = new Map<string, { topic: string; notes: number }[]>();
  for (const l of d.links) {
    const list = byEntity.get(l.entity) ?? [];
    list.push({ topic: l.topic, notes: l.notes });
    byEntity.set(l.entity, list);
  }
  const ents = d.entities.filter((e) => byEntity.has(e.id));
  const peak = Math.max(1e-9, ...ents.map((e) => metric(e, by)));
  const topicPeak = Math.max(1, ...d.topics.map((t) => (by === "activity" ? t.week : t.notes)));

  const nodes: MapNode[] = d.topics.map((t) => ({
    id: `topic:${t.name}`,
    kind: "topic",
    label: t.label,
    hue: hue.get(t.name)!,
    // Hubs stay big enough to read; by certainty they keep a fixed size (a topic has no claim status).
    val: by === "certainty" ? 60 : 30 + 90 * Math.sqrt((by === "activity" ? t.week : t.notes) / topicPeak),
    topics: 1,
    detail: `${t.notes.toLocaleString()} notes · ${t.week} this week`,
    labelled: true,
  }));
  for (const e of ents) {
    const owners = byEntity.get(e.id)!.sort((a, b) => b.notes - a.notes);
    const m = metric(e, by);
    nodes.push({
      id: e.id,
      kind: "entity",
      label: e.name,
      hue: hue.get(owners[0].topic) ?? 0,
      val: 1 + 24 * Math.sqrt(m / peak),
      topics: owners.length,
      detail:
        `${e.entity_type ?? "entity"} · ${e.mentions} notes · ${e.week} this week · ` +
        (e.claims > 0 ? `${e.claims} claims, certainty ${(e.certainty ?? 0).toFixed(2)}` : "no claims") +
        (owners.length > 1 ? ` · bridges ${owners.length} topics` : ""),
      labelled: false,
    });
  }
  // Name only the biggest entities until zoomed in: labels overlap otherwise.
  const ranked = nodes.filter((n) => n.kind === "entity").sort((a, b) => b.val - a.val);
  for (const n of ranked.slice(0, LABEL_BUDGET)) n.labelled = true;
  const links: MapLink[] = d.links
    .filter((l) => ents.some((e) => e.id === l.entity))
    .map((l) => ({ source: `topic:${l.topic}`, target: l.entity, notes: l.notes }));
  return { nodes, links };
}

