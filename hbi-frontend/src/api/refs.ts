/**
 * Todo tags as references (`clients/refs.rs`): a tag is `kind:name` and names
 * something the brain has — a workflow, an agent, a program, a topic, an
 * entity, a tool or a skill. A tag says what the item is *for*; who does it
 * stays the item's assignee, and the program it counts on stays `program`.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";
import type { DrawerKind } from "../components/drawer/entities";
import type { Todo } from "../components/todos/model";

export const REF_KINDS = ["workflow", "agent", "program", "topic", "entity", "tool", "skill"] as const;
export type RefKind = (typeof REF_KINDS)[number];

export const REF_KIND_LABELS: Record<RefKind, string> = {
  workflow: "Workflows", agent: "Agents", program: "Programs", topic: "Topics",
  entity: "Entities", tool: "Tools", skill: "Skills",
};

export interface Ref {
  ref: string;
  kind: RefKind | null;
  name?: string;
  label: string;
  detail?: string;
  /** The drawer that opens it, when it has one. */
  open?: { kind: DrawerKind; id: string } | null;
  /** From resolve: false for a tag naming nothing. */
  exists?: boolean;
  /** A free word from before 2026-10-20: kept until removed, not addable. */
  legacy?: boolean;
}

/** `kind:name` → its kind, when it is one. */
export function refKind(tag: string): RefKind | null {
  const k = tag.split(":", 1)[0];
  return tag.includes(":") && (REF_KINDS as readonly string[]).includes(k) ? (k as RefKind) : null;
}

export function useRefSearch(q: string, enabled = true) {
  return useQuery({
    queryKey: ["refs", "search", q],
    queryFn: () => apiJson<{ refs: Ref[]; note?: string | null }>(`/api/refs${query({ q, limit: 6 })}`),
    enabled,
    staleTime: 30_000,
  });
}

/** Refs a title and description name, found in code (no model). */
export function useRefSuggestions(text: string) {
  return useQuery({
    queryKey: ["refs", "suggest", text],
    queryFn: () => apiSend<{ refs: Ref[] }>("POST", "/api/refs/suggest", { text }),
    enabled: text.trim().length >= 3,
    staleTime: 60_000,
  });
}

/** Labels and drawer links for saved tags, in their order. */
export function useResolvedRefs(tags: string[]) {
  return useQuery({
    queryKey: ["refs", "resolve", ...tags],
    queryFn: () => apiJson<{ refs: Ref[] }>(`/api/refs/resolve${query({ refs: tags.join(",") })}`),
    enabled: tags.length > 0,
    staleTime: 60_000,
  });
}

/** The items tagged with one ref, open first. */
export function useTodosFor(ref: string) {
  return useQuery({
    queryKey: ["refs", "todos", ref],
    queryFn: () => apiJson<{ todos: Todo[] }>(`/api/refs/todos${query({ ref })}`),
    staleTime: 30_000,
  });
}

/** A tag in a line of text: its name without the kind (`daily-news`). */
export function tagLabel(tag: string): string {
  return refKind(tag) ? tag.slice(tag.indexOf(":") + 1) : tag;
}
