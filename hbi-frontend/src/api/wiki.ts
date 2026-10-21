/**
 * Reference links: the Wikidata item / Wikipedia article an entity is
 * (the `reference` plugin's `wiki.rs`, HBI plan Phase 18). The model's pick is a proposal the
 * user confirms or rejects; a rejected item is never offered again.
 */
import { useQuery } from "@tanstack/react-query";
import { ApiError, apiJson, apiSend, pluginApi, query } from "./client";
import { queryClient } from "./queries";
import { refreshTypes } from "./types";

/** The reference plugin's routes. */
const REFERENCE = pluginApi("reference");

export interface WikiItem {
  qid: string;
  label: string;
  description: string;
  kinds: string[];
  aliases: string[];
  wiki_title: string | null;
  wiki_url: string | null;
  wikidata_url: string;
}

export interface WikiLink extends WikiItem {
  status: "proposed" | "confirmed" | null;
  method: "llm" | "manual" | null;
  why: string | null;
  /** The search term the model suggested when the name alone found nothing. */
  query: string | null;
  model: string | null;
  at: string | null;
  /** Where it is (Wikidata P625, MAP_PLAN Phase 1): null = not read yet;
   * `lat: null` = read, Wikidata has none. */
  location?: { lat: number | null; lon: number | null; coord_count: number } | null;
}

export interface EntityReference {
  link: WikiLink | null;
  summary: string | null;
  summary_error: string | null;
  /** `proposed` · `none` · null (never looked up). */
  result: string | null;
  checked_at: string | null;
  /** An on-demand lookup is queued or running. */
  pending: boolean;
  /** A claim (a public source) mentions it, so background lookups include it. */
  public: boolean;
  candidates: WikiItem[];
  rejected: { qid: string | null; label: string | null }[];
  contact_set: boolean;
}

export function useEntityReference(id: string) {
  return useQuery({
    queryKey: ["wiki", "entity", id],
    queryFn: () => apiJson<EntityReference>(`${REFERENCE}/entities/${encodeURIComponent(id)}/wiki`),
    // Poll while an on-demand lookup runs.
    refetchInterval: (q) => (q.state.data?.pending ? 4_000 : false),
  });
}

export interface WikiHint {
  label: string | null;
  description: string | null;
  status: "proposed" | "confirmed" | null;
}

/** One-line descriptions for the entities in a review list (only linked ones appear). */
export function useWikiHints(ids: string[]) {
  const key = [...new Set(ids)].sort();
  return useQuery({
    queryKey: ["wiki", "describe", key.join(",")],
    // Hints decorate core review lists: with the reference plugin off there
    // are none, and the list must still render.
    queryFn: () =>
      apiSend<Record<string, WikiHint>>("POST", `${REFERENCE}/wiki/describe`, { ids: key }).catch(
        (e: unknown) => {
          if (e instanceof ApiError && e.status === 404) return {} as Record<string, WikiHint>;
          throw e;
        },
      ),
    enabled: key.length > 0,
    staleTime: 60_000,
  });
}

export interface WikiCoverage {
  public_entities: number;
  buckets: { confirmed: number; proposed: number; none: number; waiting: number };
  linked_on_demand: number;
  contact_set: boolean;
  bucket?: string;
  entities?: { id: string; name: string; entity_type: string; claims: number; label: string | null; description: string | null; checked_at: string | null }[];
}

export function useWikiCoverage(bucket?: string, limit = 100) {
  return useQuery({
    queryKey: ["wiki", "coverage", bucket ?? "", limit],
    queryFn: () => apiJson<WikiCoverage>(`${REFERENCE}/wiki/coverage${query({ bucket, limit })}`),
  });
}

async function refresh<T>(p: Promise<T>): Promise<T> {
  const r = await p;
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ["wiki"] }),
    queryClient.invalidateQueries({ queryKey: ["entities", "candidates"] }),
    // A reference decides the entity's kind, and may raise or settle a type question.
    refreshTypes(),
  ]);
  return r;
}

const base = (id: string) => `${REFERENCE}/entities/${encodeURIComponent(id)}/wiki`;
export const lookUp = (id: string) => refresh(apiSend<{ job_id: string }>("POST", `${base(id)}/lookup`));
export const decideReference = (id: string, qid: string, decision: "confirm" | "reject") =>
  refresh(apiSend("POST", `${base(id)}/decide`, { qid, decision }));
export const setReferenceUrl = (id: string, url: string) => refresh(apiSend("POST", `${base(id)}/url`, { url }));
