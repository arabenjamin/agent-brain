import { useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatEvent, ChatHistoryMessage } from "../../api/chat";
import { streamChat } from "../../api/chat";
import { callTool } from "../../api/mcp";

// ── Types ────────────────────────────────────────────────────────────────────

interface UserMsg {
  kind: "user";
  id: string;
  text: string;
}

interface AssistantMsg {
  kind: "assistant";
  id: string;
  events: ChatEvent[];
  done: boolean;
}

type Msg = UserMsg | AssistantMsg;

interface Session {
  session_id: string;
  started_at: string;
  msg_count: number;
  title: string;
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function uid() {
  return crypto.randomUUID();
}

function toHistory(msgs: Msg[]): ChatHistoryMessage[] {
  const hist: ChatHistoryMessage[] = [];
  for (const m of msgs) {
    if (m.kind === "user") {
      hist.push({ role: "user", content: m.text });
    } else {
      const text = m.events.find((e) => e.type === "message")?.content ?? "";
      if (text) hist.push({ role: "assistant", content: text });
    }
  }
  return hist;
}

function formatDate(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  } catch {
    return "";
  }
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n - 1) + "…" : s;
}

// ── Event bubble ─────────────────────────────────────────────────────────────

function EventBubble({ evt }: { evt: ChatEvent }) {
  if (evt.type === "thinking") {
    return (
      <div className="chat-event thinking">
        <span className="chat-event-label">◌</span>
        {evt.content}
      </div>
    );
  }
  if (evt.type === "tool_call") {
    const argsStr =
      evt.args ? " " + JSON.stringify(evt.args).slice(0, 80) : "";
    return (
      <div className="chat-event tool_call">
        <span className="chat-event-label">⚙</span>
        {evt.tool}
        <span style={{ color: "var(--text-muted)", marginLeft: 4 }}>
          {argsStr}
        </span>
      </div>
    );
  }
  if (evt.type === "tool_result") {
    return (
      <div className={`chat-event tool_result ${evt.success ? "ok" : "err"}`}>
        <span className="chat-event-label">{evt.success ? "✓" : "✗"}</span>
        {evt.tool}
        {evt.preview && (
          <span style={{ color: "var(--text-muted)", marginLeft: 4 }}>
            — {evt.preview.slice(0, 100)}
          </span>
        )}
      </div>
    );
  }
  if (evt.type === "error") {
    return (
      <div className="chat-event error">
        <span className="chat-event-label">!</span>
        {evt.message}
      </div>
    );
  }
  return null;
}

// ── Main component ────────────────────────────────────────────────────────────

export default function ChatPanel() {
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [sessionId, setSessionId] = useState<string>(() => uid());
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loadingSessions, setLoadingSessions] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Auto-scroll to bottom on new messages.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [msgs]);

  // Load session list from server.
  const loadSessions = useCallback(async () => {
    setLoadingSessions(true);
    try {
      const raw = await callTool("list_sessions", { limit: 50 });
      const parsed = JSON.parse(raw) as { sessions?: Session[] };
      setSessions(parsed.sessions ?? []);
    } catch {
      // ignore — brain may not be connected yet
    } finally {
      setLoadingSessions(false);
    }
  }, []);

  useEffect(() => {
    loadSessions();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Switch to an existing session — restore its messages from the server.
  const switchSession = useCallback(async (sid: string) => {
    if (sid === sessionId && msgs.length > 0) return;
    setSessionId(sid);
    setMsgs([]);
    try {
      const raw = await callTool("get_context", { session_id: sid, limit: 200 });
      const parsed = JSON.parse(raw) as {
        entries?: Array<{ role: string; content: string }>;
      };
      const entries = parsed.entries ?? [];
      const restored: Msg[] = entries
        .filter((e) => e.role === "user" || e.role === "assistant")
        .map((e) => {
          if (e.role === "user") {
            return { kind: "user" as const, id: uid(), text: e.content };
          }
          return {
            kind: "assistant" as const,
            id: uid(),
            events: [{ type: "message" as const, content: e.content }],
            done: true,
          };
        });
      setMsgs(restored);
    } catch {
      setMsgs([]);
    }
  }, [sessionId, msgs.length]);

  // Start a fresh session.
  const newChat = useCallback(() => {
    setSessionId(uid());
    setMsgs([]);
  }, []);

  const send = useCallback(async () => {
    const text = input.trim();
    if (!text || streaming) return;

    const userMsg: UserMsg = { kind: "user", id: uid(), text };
    const asstId = uid();
    const asstMsg: AssistantMsg = { kind: "assistant", id: asstId, events: [], done: false };

    setMsgs((prev) => [...prev, userMsg, asstMsg]);
    setInput("");
    setStreaming(true);

    const history = toHistory(msgs);
    const abort = new AbortController();
    abortRef.current = abort;

    await streamChat({
      message: text,
      history,
      sessionId,
      signal: abort.signal,
      onEvent: (evt) => {
        setMsgs((prev) =>
          prev.map((m) => {
            if (m.kind !== "assistant" || m.id !== asstId) return m;
            return {
              ...m,
              events: [...m.events, evt],
              done: evt.type === "done",
            };
          })
        );
      },
    });

    setStreaming(false);
    abortRef.current = null;
    inputRef.current?.focus();

    // Refresh the session list so this session appears / updates.
    loadSessions();
  }, [input, msgs, streaming, sessionId, loadSessions]);

  const handleKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  };

  const stop = () => {
    abortRef.current?.abort();
    setStreaming(false);
  };

  const exportChat = () => {
    if (msgs.length === 0) return;

    let markdown = `# Chat Export - Session ${sessionId}\n\n`;
    
    for (const m of msgs) {
      if (m.kind === "user") {
        markdown += `## User\n${m.text}\n\n`;
      } else {
        const finalText = m.events.find((e) => e.type === "message")?.content ?? "";
        markdown += `## Agent Brain\n${finalText}\n\n`;
      }
    }

    const blob = new Blob([markdown], { type: "text/markdown;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `chat-export-${sessionId.slice(0, 8)}.md`;
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    URL.revokeObjectURL(url);
  };

  // ── Render ──

  const renderMsg = (m: Msg) => {
    if (m.kind === "user") {
      return (
        <div key={m.id} className="chat-msg user">
          <div className="chat-msg-bubble">{m.text}</div>
        </div>
      );
    }

    const nonDoneEvents = m.events.filter((e) => e.type !== "done");
    const finalText = m.events.find((e) => e.type === "message")?.content ?? "";
    const streamEvents = nonDoneEvents.filter((e) => e.type !== "message");
    const showTyping = !m.done && streaming && m.events.length === 0;

    return (
      <div key={m.id} className="chat-msg assistant">
        {streamEvents.map((evt, i) => (
          <EventBubble key={i} evt={evt} />
        ))}
        {showTyping && (
          <div className="typing-indicator">
            <span /><span /><span />
          </div>
        )}
        {finalText && (
          <div className="chat-msg-bubble markdown-body" style={{ position: "relative", paddingRight: "36px" }}>
            <button
              className="copy-btn"
              onClick={() => navigator.clipboard.writeText(finalText)}
              title="Copy text"
              aria-label="Copy text"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
                <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
              </svg>
            </button>
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{finalText}</ReactMarkdown>
          </div>
        )}
      </div>
    );
  };

  return (
    <div className="panel">
      <div className="panel-header" style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <span>🧠 Chat</span>
        <div style={{ display: "flex", gap: "8px" }}>
          <button 
            className="btn" 
            style={{ padding: "3px 10px", fontSize: 11, background: "transparent", border: "1px solid var(--border)" }}
            onClick={exportChat}
            disabled={msgs.length === 0}
            title="Export conversation as Markdown"
          >
            Export
          </button>
          {streaming && (
            <button
              className="btn danger"
              style={{ padding: "3px 10px", fontSize: 11 }}
              onClick={stop}
            >
              Stop
            </button>
          )}
        </div>
      </div>

      <div className="chat-body">
        {/* ── Session sidebar ── */}
        <div className="session-sidebar">
          <div className="session-sidebar-header">
            History
            <button
              className="session-refresh-btn"
              onClick={loadSessions}
              title="Refresh sessions"
            >
              {loadingSessions ? "…" : "↻"}
            </button>
          </div>

          <button className="session-new-btn" onClick={newChat}>
            + New chat
          </button>

          <div className="session-list scroll">
            {sessions.length === 0 && !loadingSessions && (
              <div className="session-empty">No history yet</div>
            )}
            {sessions.map((s) => (
              <div
                key={s.session_id}
                className={`session-item${s.session_id === sessionId ? " active" : ""}`}
                onClick={() => switchSession(s.session_id)}
              >
                <div className="session-item-title">
                  {truncate(s.title, 38)}
                </div>
                <div className="session-item-meta">
                  {formatDate(s.started_at)}
                  {s.msg_count > 0 && ` · ${s.msg_count} msgs`}
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* ── Chat area ── */}
        <div className="chat-area">
          <div className="chat-messages">
            {msgs.length === 0 && (
              <div className="empty-state" style={{ marginTop: 60 }}>
                <span className="icon">🤖</span>
                <span>Send a message to start a conversation</span>
                <span style={{ fontSize: 11, color: "var(--text-muted)" }}>
                  Shift+Enter for newline · Enter to send
                </span>
              </div>
            )}
            {msgs.map(renderMsg)}
            <div ref={bottomRef} />
          </div>

          <div className="input-row">
            <textarea
              ref={inputRef}
              rows={2}
              placeholder="Ask anything… (Enter to send, Shift+Enter for newline)"
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={handleKey}
              disabled={streaming}
              style={{ resize: "none" }}
            />
            <button className="btn" onClick={send} disabled={streaming || !input.trim()}>
              Send
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
