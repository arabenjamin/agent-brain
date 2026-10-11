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
  kind: "entity_merges" | "failed_tasks" | "dead_jobs" | "broken_workflows" | string;
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

/** What happened when an item was offered a run. */
export interface RunOutcome {
  outcome: "started" | "awaiting_approval" | "not_due" | "not_ready" | "assigned_to_user" | "done";
  reason?: string;
  task_id?: string;
  agent?: string;
  run?: number;
}

async function postItem(id: string, action: string, body: Record<string, unknown> = {}): Promise<RunOutcome> {
  const res = await apiRequest(`/api/items/${encodeURIComponent(id)}/${action}`, { method: "POST", body: JSON.stringify(body) });
  const data = (await res.json().catch(() => ({}))) as { error?: string; run?: RunOutcome };
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  await refreshFeed();
  await queryClient.invalidateQueries({ queryKey: ["work"] });
  return data.run ?? { outcome: "not_ready" };
}

/** Hand an item to an agent (`agent:<name>`) or take it back (`user`). */
export function assignItem(id: string, assignee: string, prompt?: string, model?: string): Promise<RunOutcome> {
  const body: Record<string, unknown> = { assignee };
  if (prompt !== undefined) body.prompt = prompt;
  if (model !== undefined) body.model = model;
  return postItem(id, "assign", body);
}

/** Start a run waiting for approval, or accept a result waiting for review. */
export function approveItem(id: string): Promise<RunOutcome> {
  return postItem(id, "approve");
}

/** Send a result back to its agent with a comment; runs it again. */
export function sendBackItem(id: string, comment: string): Promise<RunOutcome> {
  return postItem(id, "send-back", { comment });
}

/** A run outcome, in words. */
export function outcomeText(o: RunOutcome): string {
  switch (o.outcome) {
    case "started": return `${o.agent} started run ${o.run}`;
    case "awaiting_approval": return `Waiting for your approval: ${o.reason}`;
    case "not_due": return "Assigned; it runs on its due date";
    case "not_ready": return `Not started: ${o.reason}`;
    case "assigned_to_user": return "It's yours again";
    case "done": return "Done";
  }
}

