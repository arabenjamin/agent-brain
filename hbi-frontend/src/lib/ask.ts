/**
 * The Ask modal's state lives in the URL (CORE_AND_PLUGINS_PLAN Phase 5):
 * `ask=1` opens it on the current conversation, `ask=<session id>` on that
 * one. It rides on whatever workspace is showing, so a thread opened from
 * the Feed opens over the Feed, Back closes it, and a link can land in it.
 */
import { currentRoute, navigate } from "../router";

export interface AskState {
  open: boolean;
  /** A conversation to open, when the URL names one. */
  session?: string;
}

const OPEN = "1";

export function askState(params: Record<string, string>): AskState {
  const v = params.ask;
  if (!v) return { open: false };
  return v === OPEN ? { open: true } : { open: true, session: v };
}

/** Open the modal, on `session` when given. */
export function openAsk(session?: string): void {
  navigate({ params: { ask: session || OPEN } });
}

export function closeAsk(): void {
  navigate({ params: { ask: null } });
}

export function toggleAsk(): void {
  if (askState(currentRoute().params).open) closeAsk();
  else openAsk();
}
