/**
 * What an entity is (`services/entity_kinds.rs`, HBI plan Phase 19): the
 * coarse type, the fine kind from its reference link, and the questions the
 * brain asks when the two disagree. A type set by hand is final until it is
 * unlocked.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import { refreshFeed } from "./feed";
import { queryClient } from "./queries";

export interface TypeQuestion {
  id: string;
  name: string;
  entity_type: string | null;
  mentions: number;
  qid: string;
  label: string | null;
  description: string | null;
  /** What the reference says it is. */
  kind: string | null;
  kind_type: string | null;
  link_status: "proposed" | "confirmed" | null;
}

export interface EntityTypes {
  total: number;
  questions: TypeQuestion[];
  /** The coarse types offered when setting one by hand. */
  entity_types: string[];
  kinds: { name: string; entity_type: string }[];
}

export const typesKey = ["entity-types"] as const;

export function useEntityTypes(limit = 100) {
  return useQuery({
    queryKey: [...typesKey, limit],
    queryFn: () => apiJson<EntityTypes>(`/api/entities/types${query({ limit })}`),
    staleTime: 60_000,
  });
}

/** After a type changes: the entity, the questions, the feed and every list showing types. */
export async function refreshTypes(id?: string): Promise<void> {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: typesKey }),
    queryClient.invalidateQueries({ queryKey: id ? ["entity", id] : ["entity"] }),
    queryClient.invalidateQueries({ queryKey: ["search"] }),
    refreshFeed(),
  ]);
}

const base = (id: string) => `/api/entities/${encodeURIComponent(id)}/type`;

export async function setEntityType(id: string, entityType: string, kind?: string) {
  await apiSend("PUT", base(id), { entity_type: entityType, kind: kind?.trim() || null });
  await refreshTypes(id);
}

export async function unlockEntityType(id: string) {
  await apiSend("DELETE", base(id));
  await refreshTypes(id);
}

export async function answerTypeQuestion(id: string, answer: "keep" | "reference") {
  await apiSend("POST", `${base(id)}/answer`, { answer });
  await refreshTypes(id);
}
