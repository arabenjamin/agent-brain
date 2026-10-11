/**
 * Typed relationships between entities (HBI plan Phase 17), served by the
 * epistemics plugin (`plugins/epistemics/crate/src/relations.rs`). A closed
 * vocabulary (the bundle's `relations.yaml`); the user's edges are final;
 * extracted ones are proposals to accept or reject, and a rejected one is
 * never proposed again. With the plugin off its routes answer 404, and these
 * hooks answer empty: the views that show relationships still render.
 */
import { useQuery } from "@tanstack/react-query";
import { ApiError, apiJson, apiSend, pluginApi, query } from "./client";
import { queryClient } from "./queries";

/** Typed relationships are the epistemics plugin's (`plugins/epistemics/`). */
export const EPISTEMICS = pluginApi("epistemics");

/** `fallback` when the epistemics plugin is off (its routes answer 404). */
export function orWhenOff<T>(p: Promise<T>, fallback: T): Promise<T> {
  return p.catch((e: unknown) => {
    if (e instanceof ApiError && e.status === 404) return fallback;
    throw e;
  });
}

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
    queryFn: () => orWhenOff(apiJson<{ types: RelType[] }>(`${EPISTEMICS}/relations/types`), { types: [] }),
    staleTime: 3_600_000,
  });
}

export function useEntityRelations(id: string) {
  return useQuery({
    queryKey: ["relations", "entity", id],
    queryFn: () =>
      orWhenOff(
        apiJson<{ entity: string; relations: Relation[]; rejected: number }>(`${EPISTEMICS}/entities/${encodeURIComponent(id)}/relations`),
        { entity: id, relations: [], rejected: 0 },
      ),
  });
}

export function useProposedRelations(limit = 30) {
  return useQuery({
    queryKey: ["relations", "proposed", limit],
    queryFn: () =>
      orWhenOff(
        apiJson<{ total: number; returned: number; proposed: ProposedRelation[] }>(`${EPISTEMICS}/relations/proposed${query({ limit })}`),
        { total: 0, returned: 0, proposed: [] },
      ),
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
  refresh(apiSend("POST", `${EPISTEMICS}/relations`, { from, to, type, note_id: noteId }));
export const decideRelation = (r: { from: string; to: string; type: string }, accept: boolean) =>
  refresh(apiSend("POST", `${EPISTEMICS}/relations/decide`, { from: r.from, to: r.to, type: r.type, accept }));
export const relabelRelation = (r: { from: string; to: string; type: string }, newType: string) =>
  refresh(apiSend("POST", `${EPISTEMICS}/relations/relabel`, { from: r.from, to: r.to, type: r.type, new_type: newType }));
export const removeRelation = (r: { from: string; to: string; type: string }) =>
  refresh(apiSend("DELETE", `${EPISTEMICS}/relations${query({ from: r.from, to: r.to, type: r.type })}`));
