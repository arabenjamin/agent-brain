/**
 * Ask: the chat as a modal over any workspace (CORE_AND_PLUGINS_PLAN Phase
 * 5). Ctrl-J toggles it; the URL holds it (`lib/ask.ts`), so the Feed, Home
 * and old `#/chat` links open it on their conversation. The agent picker,
 * the session list and the context dock are ChatPanel's own.
 *
 * It stays mounted while closed, so an answer that is still streaming keeps
 * going and is there when it opens again.
 */
import { closeAsk } from "../../lib/ask";
import ChatPanel from "./ChatPanel";

export default function AskModal({ open, session }: { open: boolean; session?: string }) {
  return (
    <div
      className="ask-backdrop"
      style={open ? undefined : { display: "none" }}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) closeAsk();
      }}
    >
      <div
        className="ask-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Ask the brain"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            closeAsk();
          }
        }}
      >
        <div className="ask-bar">
          <span className="ask-title">🧠 Ask</span>
          <span className="ask-hint">Ctrl-J or Esc to close · it keeps answering while closed</span>
          <button type="button" className="ask-close" onClick={closeAsk} title="Close (Esc)" aria-label="Close">
            ×
          </button>
        </div>
        <div className="ask-body">
          <ChatPanel visible={open} session={session} />
        </div>
      </div>
    </div>
  );
}
