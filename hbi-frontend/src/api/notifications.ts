/**
 * Agent notifications (`notify_user`).
 *
 * The brain writes an `:AgentNotification` node whenever a chain — usually a
 * scheduled task — ends in `notify_user`. The nav badge and the in-chat banner
 * are two views of that same list, so both read it through here: when they
 * fetched independently they could (and did) disagree, leaving a badge that
 * announced messages the chat panel never showed.
 */
import { apiJson, apiRequest } from "./client";

export interface AgentNotification {
  id: string;
  message: string;
  /** Short label, e.g. "Todo Review 2026-08-19". Empty string when unset. */
  context: string;
  /** Session to continue. Empty string when the notification opened no session. */
  related_session_id: string;
  created_at: string;
  read: boolean;
}

export async function fetchUnreadNotifications(): Promise<AgentNotification[]> {
  const data = await apiJson<{ notifications?: AgentNotification[] }>("/api/notifications?unread=true");
  return data.notifications ?? [];
}

export async function markNotificationRead(id: string): Promise<void> {
  await apiRequest(`/api/notifications/${id}/read`, { method: "POST" });
}
