/**
 * How entities relate today (`clients/related.rs`, HBI plan Phase 16): through
 * the notes that mention both. Ranked by strength (shared notes over the
 * geometric mean of each one's mentions, both shrunk by 3) so a hub mentioned
 * everywhere doesn't top every list; every pair drills to its notes.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, query } from "./client";
import type { Relation } from "./relations";

export interface RelatedEntity {
  id: string;
  name: string | null;
  entity_type: string | null;
  /** Notes that mention both. */
  shared: number;
  /** Notes that mention this one. */
  mentions: number;
  strength: number;
}

export interface RelatedResponse {
  entity: string;
  topic: string | null;
  /** Every entity it shares a note with. */
  total: number;
  returned: number;
  related: RelatedEntity[];
  method: string;
}

export interface PairResponse {
  /** Typed relationships between the two, either direction, rejected included. */
  relations: Relation[];
  a: { id: string; name: string | null };
  b: { id: string; name: string | null };
  topic: string | null;
  shared: number;
  returned: number;
  notes: { id: string; note_type: string | null; source_context: string | null; created_at: string | null; preview: string; preview_truncated: boolean }[];
}

export interface NeighbourhoodNode {
  id: string;
  name: string | null;
  entity_type: string | null;
  mentions: number;
  /** The topic most of the notes mentioning it are mainly in. */
  topic: string | null;
  hidden_from: string[];
}

export interface NeighbourhoodResponse {
  /** Typed relationships among the nodes (active and proposed). */
  relations: { source: string; target: string; type: string; label: string; status: "active" | "proposed"; method: string | null }[];
  center: string;
  topic: string | null;
  total_related: number;
  nodes: NeighbourhoodNode[];
  links: { source: string; target: string; shared: number; strength: number }[];
}

export function useRelated(id: string, topic?: string, limit = 15) {
  return useQuery({
    queryKey: ["entity", "related", id, topic ?? "", limit],
    queryFn: () => apiJson<RelatedResponse>(`/api/entities/${encodeURIComponent(id)}/related${query({ topic, limit })}`),
    staleTime: 60_000,
  });
}

export function usePair(a: string | null, b: string | null, topic?: string) {
  return useQuery({
    queryKey: ["entity", "pair", a ?? "", b ?? "", topic ?? ""],
    queryFn: () => apiJson<PairResponse>(`/api/entities/pair${query({ a: a ?? "", b: b ?? "", topic })}`),
    enabled: !!a && !!b,
    staleTime: 60_000,
  });
}

export function fetchNeighbourhood(id: string, topic?: string) {
  return apiJson<NeighbourhoodResponse>(`/api/entities/${encodeURIComponent(id)}/neighbourhood${query({ topic })}`);
}
