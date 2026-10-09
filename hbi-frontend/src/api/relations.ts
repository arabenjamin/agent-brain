/**
 * Typed relationships between entities (`services/relations.rs`, HBI plan
 * Phase 17). A closed vocabulary (`relations/*.yaml`); the user's edges are
 * final; extracted ones are proposals to accept or reject, and a rejected one
 * is never proposed again.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import { queryClient } from "./queries";

export interface RelType {
  name: string;
  label: string;
  inverse: string;
  definition: string;
  from: string[];
  to: string[];
  symmetric: boolean;
  /** False: only the user sets it. */
  extract: boolean;
}

export interface Relation {
  from: string;
  to: string;
  type: string;
  /** How it reads from the entity asked about ("works for", "employs"). */
  reads: string;
  other: { id: string | null; name: string | null; entity_type: string | null };
  method: "manual" | "llm" | null;
  status: "active" | "proposed" | "rejected" | null;
  /** Note ids that state it. */
  evidence: string[];
  model: string | null;
  created_at: string | null;
  decided_at: string | null;
}

export interface ProposedRelation extends Relation {
  from_name: string | null;
  evidence_text: string | null;
}

export function useRelationTypes() {
  return useQuery({
    queryKey: ["relations", "types"],
    queryFn: () => apiJson<{ types: RelType[] }>("/api/relations/types"),
    staleTime: 3_600_000,
  });
}

export function useEntityRelations(id: string) {
  return useQuery({
    queryKey: ["relations", "entity", id],
    queryFn: () => apiJson<{ entity: string; relations: Relation[]; rejected: number }>(`/api/entities/${encodeURIComponent(id)}/relations`),
  });
}

export function useProposedRelations(limit = 30) {
  return useQuery({
    queryKey: ["relations", "proposed", limit],
    queryFn: () => apiJson<{ total: number; returned: number; proposed: ProposedRelation[] }>(`/api/relations/proposed${query({ limit })}`),
  });
}

async function refresh<T>(p: Promise<T>): Promise<T> {
  const r = await p;
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ["relations"] }),
    queryClient.invalidateQueries({ queryKey: ["entity"] }),
    queryClient.invalidateQueries({ queryKey: ["feed"] }),
  ]);
  return r;
}

export const setRelation = (from: string, to: string, type: string, noteId?: string) =>
  refresh(apiSend("POST", "/api/relations", { from, to, type, note_id: noteId }));
export const decideRelation = (r: { from: string; to: string; type: string }, accept: boolean) =>
  refresh(apiSend("POST", "/api/relations/decide", { from: r.from, to: r.to, type: r.type, accept }));
export const relabelRelation = (r: { from: string; to: string; type: string }, newType: string) =>
  refresh(apiSend("POST", "/api/relations/relabel", { from: r.from, to: r.to, type: r.type, new_type: newType }));
export const removeRelation = (r: { from: string; to: string; type: string }) =>
  refresh(apiSend("DELETE", `/api/relations${query({ from: r.from, to: r.to, type: r.type })}`));
