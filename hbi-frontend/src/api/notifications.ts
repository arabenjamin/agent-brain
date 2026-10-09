/**
 * Agent notifications (`notify_user`). The brain writes an
 * `:AgentNotification` whenever a chain ends in `notify_user`. They are shown
 * as Feed rows (`api/feed.ts`): a read, or, with `needs: "action"`, the item
 * it created. Marking one read is in `api/feed.ts` (`markRead`).
 */

export interface AgentNotification {
  id: string;
  message: string;
  /** Short label, e.g. "Todo Review 2026-08-19". Empty string when unset. */
  context: string;
  /** Session to continue. Empty string when the notification opened no session. */
  related_session_id: string;
  created_at: string;
  read: boolean;
  /** It asked for something; its item is the Feed row. */
  needs_action?: boolean;
  item_id?: string | null;
}
