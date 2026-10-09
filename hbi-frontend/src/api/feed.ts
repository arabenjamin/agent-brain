/**
 * The Feed (`GET /api/feed`, HBI plan §9.3): one list of what wants the user.
 * Items to act on, notifications to read, and one digest card per system
 * signal. `total` is its row count, and is the one number the nav badge,
 * Home's Needs-you tile and the Feed header all show.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiRequest } from "./client";
import { queryClient } from "./queries";
import { goTo } from "../nav";
import type { Todo } from "../components/todos/model";
import type { AgentNotification } from "./notifications";

export interface FeedItem extends Todo {
  overdue: boolean;
}

export interface FeedDigest {
  kind: "entity_merges" | "failed_tasks" | "dead_jobs" | "broken_schedules" | string;
  title: string;
  count: number;
  /** Work-view params the card drills to. */
  drill: Record<string, string>;
}

export interface Feed {
  total: number;
  items: FeedItem[];
  /** Items hidden until their snooze ends. */
  snoozed: number;
  reads: AgentNotification[];
  digests: FeedDigest[];
  /** Parts that could not be read; the rest is still shown. */
  errors: { part: string; error: string }[];
}

export const feedKey = ["feed"] as const;

export function useFeed(pollMs = 30_000) {
  return useQuery({
    queryKey: feedKey,
    queryFn: () => apiJson<Feed>("/api/feed"),
    refetchInterval: pollMs,
  });
}

export function refreshFeed(): Promise<void> {
  return queryClient.invalidateQueries({ queryKey: feedKey });
}

async function putTodo(id: string, body: Record<string, unknown>): Promise<void> {
  const res = await apiRequest(`/todos/${encodeURIComponent(id)}`, { method: "PUT", body: JSON.stringify(body) });
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new Error((data as { error?: string }).error ?? `HTTP ${res.status}`);
  }
}

export async function completeItem(id: string): Promise<void> {
  await putTodo(id, { status: "done" });
  await refreshFeed();
}

/** Hide an item from the Feed until `until`. */
export async function snoozeItem(id: string, until: Date): Promise<void> {
  await putTodo(id, { snoozed_until: until.toISOString() });
  await refreshFeed();
}

/** Give an item a chat thread of its own (`todo-<id>`), if it has none. */
export async function ensureThread(item: Todo): Promise<string> {
  if (item.session_id) return item.session_id;
  const sid = `todo-${item.id}`;
  await putTodo(item.id, { session_id: sid });
  await refreshFeed();
  return sid;
}

/** Open a conversation in Chat (`#/chat?session=<id>`). */
export function openSession(sid: string) {
  goTo("chat", { session: sid });
}

export async function markRead(id: string): Promise<void> {
  await apiRequest(`/api/notifications/${encodeURIComponent(id)}/read`, { method: "POST" });
  await refreshFeed();
}
