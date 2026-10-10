/**
 * Entity resolution: merge proposals, a person's answers to them, and undo
 * (`clients/entities.rs`). Every write invalidates the entity, search and
 * candidate caches, since a merge changes all three.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import { queryClient } from "./queries";
import type { EntityDetail, MergedAway, SearchResponse } from "./search";

export interface CandidateSide {
  id: string;
  name: string;
  entity_type: string;
  mentions: number;
}

export interface MergeCandidate {
  from: CandidateSide;
  into: CandidateSide;
  /** Why the pair was proposed. */
  reason: "surname" | "acronym" | "title" | "plural" | "demonym" | "wikidata";
  shared_notes: number;
  /** Other names the same `from` could also be. */
  rivals: number;
  /** The local alias judge's opinion (`services/alias_judge.rs`); null until judged. */
  judge: AliasVerdict | null;
}

export interface AliasVerdict {
  /** `different` also covers "the texts don't say enough to tell". */
  verdict: "same" | "related" | "different";
  p_same: number;
  p_related: number;
  p_different: number;
  model: string;
  version: string;
  at: string;
}

export interface CandidatesResponse {
  /** All open proposals (before `limit`). */
  total: number;
  candidates: MergeCandidate[];
  /** Entities looked at (mentioned, not url/date). */
  checked: number;
}

export const REASON_LABEL: Record<MergeCandidate["reason"], string> = {
  surname: "last name of",
  acronym: "acronym of",
  title: "title + surname of",
  plural: "plural of",
  demonym: "nationality word for",
  wikidata: "same Wikidata item as",
};

export function useMergeCandidates(limit: number, entity?: string, pollMs?: number) {
  return useQuery({
    queryKey: ["entities", "candidates", entity ?? "", limit],
    queryFn: () => apiJson<CandidatesResponse>(`/api/entities/candidates${query({ limit, entity })}`),
    refetchInterval: pollMs,
  });
}

/** Entities whose name (or an alias) matches `q`, best first. */
export async function findEntities(q: string) {
  const r = await apiJson<SearchResponse>(`/api/search${query({ q, mode: "keyword", limit: 1 })}`);
  return r.entities;
}

function invalidate() {
  return Promise.all([
    queryClient.invalidateQueries({ queryKey: ["entities"] }),
    queryClient.invalidateQueries({ queryKey: ["entity"] }),
    queryClient.invalidateQueries({ queryKey: ["search"] }),
    queryClient.invalidateQueries({ queryKey: ["knowledge"] }),
  ]);
}

export async function mergeEntities(fromId: string, intoId: string) {
  const r = await apiSend<{ merged: { mentions_moved: number }; into_id: string }>(
    "POST", "/api/entities/merge", { from_id: fromId, into_id: intoId });
  await invalidate();
  return r;
}

export async function decideEntities(aId: string, bId: string, verdict: "different" | "related") {
  await apiSend("POST", "/api/entities/decide", { a_id: aId, b_id: bId, verdict });
  await invalidate();
}

export async function unmergeAlias(name: string) {
  const r = await apiSend<{ unmerged: { restored_id: string; mentions_moved: number }; still_in_seed: boolean }>(
    "POST", `/api/entities/aliases/${encodeURIComponent(name)}/unmerge`);
  await invalidate();
  return r;
}

/** An entity id that no longer exists because it was merged away. */
export function isMergedAway(d: EntityDetail | MergedAway | undefined): d is MergedAway {
  return !!d && "merged_into" in d;
}
