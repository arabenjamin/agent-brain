/**
 * Topics (`clients/topics.rs`): the domains the brain tracks, how many notes
 * each holds, the topic map, and one topic's detail. Notes are classified by
 * the local model while the brain is idle, so counts grow during a backfill;
 * `coverage` says how far along it is.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, query } from "./client";

export interface TopicSummary {
  name: string;
  label: string;
  description: string;
  /** Names of the schedules that declare this topic. */
  schedules: string[];
  source_lists: string[];
  /** Notes in the topic (as main or second topic). */
  notes: number;
  /** Notes whose main topic this is. */
  main: number;
  /** Notes in the topic written in the last 7 days. */
  week: number;
}

/** Every top-level note is in exactly one of these buckets. */
export interface TopicCoverage {
  notes: number;
  with_topic: number;
  /** Classified, and fits no topic. */
  no_topic: number;
  /** Not classified yet for the current topic definitions. */
  waiting: number;
  /** The classifier failed on it 3 times; left alone. */
  failed: number;
  /** Activity records the scope (`topics/_scope.yaml`) leaves out. */
  skipped: number;
}

export interface TopicsResponse {
  version: string;
  topics: TopicSummary[];
  coverage: TopicCoverage;
  skip_rules: { note_type?: string | null; source_context?: string | null }[];
  /** What the notes that fit no topic mention most: candidates for a new topic. */
  no_topic_entities: { id: string; name: string; notes: number }[];
}

export interface MapEntity {
  id: string;
  name: string;
  entity_type: string | null;
  /** Notes mentioning it (anywhere). */
  mentions: number;
  /** Notes mentioning it, last 7 days. */
  week: number;
  /** Claims mentioning it, and their mean certainty (0 refuted … 1 primary sources). */
  claims: number;
  certainty: number | null;
}

export interface TopicMap {
  topics: { name: string; label: string; notes: number; week: number }[];
  entities: MapEntity[];
  /** Topic → entity, weighted by the topic's notes that mention the entity. */
  links: { topic: string; entity: string; notes: number }[];
  per_topic: number;
  hidden_entity_types: string[];
}

export interface TopicDetail {
  topic: Omit<TopicSummary, "schedules"> & { source_lists: string[] };
  /** The schedules that declare this topic. */
  schedules: { name: string; id: string; enabled?: boolean; kind?: string | null }[];
  entities: { id: string; name: string; entity_type: string | null; notes: number }[];
  notes: { id: string; note_type: string | null; preview: string; created_at: string | null; main: boolean }[];
  notes_shown: number;
}

export function useTopics(pollMs?: number) {
  return useQuery({
    queryKey: ["topics", "list"],
    queryFn: () => apiJson<TopicsResponse>("/api/topics"),
    refetchInterval: pollMs,
    staleTime: 30_000,
  });
}

export function useTopicMap(perTopic: number) {
  return useQuery({
    queryKey: ["topics", "map", perTopic],
    queryFn: () => apiJson<TopicMap>(`/api/topics/map${query({ per_topic: perTopic })}`),
    staleTime: 60_000,
  });
}

export function useTopic(name: string) {
  return useQuery({
    queryKey: ["topics", "one", name],
    queryFn: () => apiJson<TopicDetail>(`/api/topics/${encodeURIComponent(name)}`),
    retry: false,
  });
}

/** Share of the notes the backfill has been through. */
export function assessedShare(c: TopicCoverage): number {
  const inScope = c.notes - (c.skipped ?? 0);
  return inScope > 0 ? (c.with_topic + c.no_topic + c.failed) / inScope : 1;
}
