/**
 * Topics (`clients/topics.rs`): the domains the brain tracks, how many notes
 * each holds, the topic map, and one topic's detail. Notes are classified by
 * the local model while the brain is idle, so counts grow during a backfill;
 * `coverage` says how far along it is.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import { queryClient } from "./queries";

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
  /** Entities the user keeps out of a topic, per topic (names: the first 20). */
  hidden_by_user: { topic: string; count: number; names: string[] }[];
}

/** A rule that puts a note in an `assign: "rules"` topic: every field given must match. */
export interface MemberRule {
  note_type?: string;
  source_context?: string;
  /** Case-insensitive, in the note's first 1,500 characters. */
  content_contains?: string;
}

/** A topic's definition: what the classifier reads (name + description). */
export interface TopicDef {
  name: string;
  label: string;
  description: string;
  source_lists: string[];
  /** `classifier`: the local model decides. `rules`: only `members` rules and the user. */
  assign?: "classifier" | "rules";
  members?: MemberRule[];
  /** A page about the topic (a Wikipedia article, say), set by the user. Not part of the definition. */
  reference?: string | null;
}

export interface TopicDetail {
  topic: Omit<TopicSummary, "schedules"> & {
    source_lists: string[];
    /** `yaml`: its topics/*.yaml file applies; `runtime`: edited here, and the file no longer applies. */
    managed_by: "yaml" | "runtime" | null;
    updated_at: string | null;
    assign: "classifier" | "rules";
    members: MemberRule[];
    reference: string | null;
  };
  /** Its topics/*.yaml definition, if a file declares it (what Reset restores). */
  seed: TopicDef | null;
  /** Notes not yet classified under the current definitions (all topics). */
  reclassify: { version: string; waiting: number };
  /** The schedules that declare this topic. */
  schedules: { name: string; id: string; enabled?: boolean; kind?: string | null }[];
  entities: { id: string; name: string; entity_type: string | null; notes: number }[];
  /** Entities the user keeps out of this topic; `notes` is how many of its notes mention each. */
  hidden: { id: string; name: string; entity_type: string | null; notes: number; at: string | null }[];
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

export interface TopicSaved {
  saved: true;
  topic: TopicDef;
  version: string;
  /** Whether the definitions changed, so notes are classified again. */
  reclassify: boolean;
  note: string;
}

async function refresh<T>(p: Promise<T>): Promise<T> {
  const r = await p;
  // Topics feed the map, schedule groups and forms.
  await Promise.all([queryClient.invalidateQueries({ queryKey: ["topics"] }), queryClient.invalidateQueries({ queryKey: ["work"] })]);
  return r;
}

const path = (name: string) => `/api/topics/${encodeURIComponent(name)}`;

export const createTopic = (def: TopicDef) => refresh(apiSend<TopicSaved>("POST", "/api/topics", def));
export const updateTopic = (name: string, def: Omit<TopicDef, "name">) => refresh(apiSend<TopicSaved>("PUT", path(name), def));
export const resetTopic = (name: string) => refresh(apiSend<TopicSaved>("POST", `${path(name)}/reset`));
export const deleteTopic = (name: string) =>
  refresh(apiSend<{ deleted: { name: string; notes: number; retired_seed: boolean } }>("DELETE", path(name)));

/** The server's limits (services/topics.rs): checked there too. */
export const MAX_DESCRIPTION = 500;
export const TOPIC_NAME = /^[a-z0-9][a-z0-9-]{1,39}$/;

async function refreshKnowledge(noteId?: string) {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ["topics"] }),
    queryClient.invalidateQueries({ queryKey: ["entity"] }),
    queryClient.invalidateQueries({ queryKey: ["search"] }),
    ...(noteId ? [queryClient.invalidateQueries({ queryKey: ["note", noteId] })] : []),
  ]);
}

/** Set a note's topics by hand, main first (empty: fits no topic). The classifier then leaves it alone. */
export async function setNoteTopics(noteId: string, topics: string[]) {
  const r = await apiSend<{ topics: string[]; locked: true }>("PUT", `/api/notes/${encodeURIComponent(noteId)}/topics`, { topics });
  await refreshKnowledge(noteId);
  return r;
}

/** Hand a note's topics back to the classifier. */
export async function unlockNoteTopics(noteId: string) {
  const r = await apiSend<{ locked: false; note: string }>("DELETE", `/api/notes/${encodeURIComponent(noteId)}/topics/lock`);
  await refreshKnowledge(noteId);
  return r;
}

/** Keep an entity out of a topic (its map and lists), or let it back in. */
export async function setEntityHidden(entityId: string, topic: string, hidden: boolean) {
  const r = await apiSend<{ hidden: boolean }>(
    hidden ? "PUT" : "DELETE",
    `/api/entities/${encodeURIComponent(entityId)}/topics/${encodeURIComponent(topic)}/hidden`,
  );
  await refreshKnowledge();
  return r;
}
