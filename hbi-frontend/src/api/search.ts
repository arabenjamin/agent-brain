/**
 * Knowledge search (`clients/search.rs`): semantic / keyword / id, entity and
 * id lookup. Searching never changes what the brain ranks on: the semantic
 * path is the non-perturbing one.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";

export type SearchMode = "auto" | "semantic" | "keyword" | "id";
/** What the server ran: a search mode, or `topic` (a topic's newest notes, no query). */
export type ServedMode = Exclude<SearchMode, "auto"> | "topic";

/** Why a note is in the results. Ranks are 1-based within each stage. */
export interface MatchTrace {
  rank: number;
  vector_rank?: number | null;
  vector_score?: number | null;
  bm25_rank?: number | null;
  bm25_score?: number | null;
  via_chunk?: string | null;
  added_by?: "graph" | "entity" | "substring_fallback" | null;
}

export interface SearchNote {
  id: string;
  note_type?: string | null;
  preview: string;
  preview_truncated: boolean;
  created_at?: string | null;
  claim_status?: string | null;
  source_context?: string | null;
  verification?: string | null;
  /** Topics, main first (`services/topics.rs`). */
  topics?: string[];
  match: MatchTrace;
}

export interface EntityHit {
  id: string;
  name: string;
  entity_type?: string | null;
  mentions: number;
}

export type DrawerableKind = "note" | "entity" | "task" | "job" | "schedule" | "todo";

export interface IdMatch {
  kind: DrawerableKind;
  id: string;
  label?: string | null;
}

export interface SearchResponse {
  query: string;
  mode: ServedMode;
  requested_mode: string;
  type?: string | null;
  limit: number;
  /** The topic the search was narrowed to, if any. */
  topic?: string | null;
  /** With a topic: how many matches were searched, and how many were in it. */
  topic_filtered?: { searched: number; in_topic: number } | null;
  notes: SearchNote[];
  entities: EntityHit[];
  truncated?: boolean;
  /** Semantic mode: false when the query could not be embedded (keyword-only). */
  vector_available?: boolean | null;
  id_match?: IdMatch | null;
  error?: string | null;
}

export interface SearchParams {
  q: string;
  mode?: SearchMode;
  type?: string;
  hops?: number;
  limit?: number;
  /** Narrow to one topic. With no query, lists the topic's newest notes. */
  topic?: string;
}

export function useSearch(p: SearchParams) {
  const q = p.q.trim();
  return useQuery({
    queryKey: ["search", q, p.mode ?? "auto", p.type ?? "", p.hops ?? 0, p.limit ?? 30, p.topic ?? ""],
    queryFn: () =>
      apiJson<SearchResponse>(`/api/search${query({ q, mode: p.mode, type: p.type, hops: p.hops || undefined, limit: p.limit, topic: p.topic })}`),
    enabled: q.length > 0 || !!p.topic,
    staleTime: 60_000,
    retry: false,
  });
}

export interface EntityAlias {
  name: string | null;
  /** `seed` (entities/*.yaml) · `normalise` (spelling) · `proposal` (a person merged it). */
  method: string | null;
  merged_at: string | null;
  /** Resolves only when the note has it in capitals (US, not "us"). */
  case_sensitive: boolean;
  /** Mentions that came in through this name (what an undo moves back). */
  mentions: number;
}

export interface EntityDetail {
  entity: {
    id: string; name: string | null; entity_type: string | null; created_at: string | null; mentions: number;
    /** `seed` when the curated seed fixed the name and type. */
    type_source?: string | null;
    /** Split back out by an undo: never folded again automatically. */
    distinct?: boolean;
  };
  aliases?: EntityAlias[];
  notes: Omit<SearchNote, "match">[];
  notes_shown: number;
  co_mentioned: { id: string; name: string | null; entity_type: string | null; shared: number }[];
}

export interface MergedAway {
  merged_into: { alias: string | null; id: string | null; name: string | null };
}

export function useEntity(id: string) {
  return useQuery({
    queryKey: ["entity", id],
    // A merged-away id answers with where it went instead of the entity.
    queryFn: () => apiJson<EntityDetail | MergedAway>(`/api/entities/${encodeURIComponent(id)}`),
    retry: false,
  });
}

/** What kind of thing an id names (note, task, job, …). */
export function lookupId(id: string) {
  return apiJson<IdMatch>(`/api/lookup/${encodeURIComponent(id)}`);
}

export interface LookupMany {
  /** `prefix` is set when a short id (an 8+ character prefix) matched. */
  found: (IdMatch & { prefix?: string })[];
  missing: string[];
  truncated: boolean;
  max: number;
}

/** What many ids (or 8+ character prefixes) name, in one call. */
export function useLookupMany(ids: string[]) {
  const sorted = [...new Set(ids)].sort();
  return useQuery({
    queryKey: ["lookup", sorted],
    queryFn: () => apiSend<LookupMany>("POST", "/api/lookup", { ids: sorted }),
    enabled: sorted.length > 0,
    staleTime: 5 * 60_000,
  });
}
