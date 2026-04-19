import { useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import "highlight.js/styles/github-dark.css";
import type { ChatEvent, ChatHistoryMessage } from "../../api/chat";
import { streamChat } from "../../api/chat";
import { callTool } from "../../api/mcp";
import { getBrainUrl, getApiKey } from "../../api/config";

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
  reflecting?: boolean;
  reflection?: string;
}

type Msg = UserMsg | AssistantMsg;

interface Session {
  session_id: string;
  started_at: string;
  msg_count: number;
  title: string;
}

interface CatalogModel {
  name: string;
  provider: string; // lowercase, e.g. "ollama"
  model: string;
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function uid() {
  if (typeof crypto !== "undefined" && crypto.randomUUID) {
    return crypto.randomUUID();
  }
  // Fallback for non-secure contexts (e.g. accessing via IP on local network)
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
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

function EventBubble({ evt, isActive }: { evt: ChatEvent; isActive?: boolean }) {
  const [expanded, setExpanded] = useState(false);

  if (evt.type === "thinking") {
    const text = evt.content ?? "";
    const isDiagnostic = text.startsWith("⚙ provider=");
    const collapsible = !isDiagnostic && text.length > 160;
    return (
      <div
        className={`chat-event thinking${collapsible ? " collapsible" : ""}${expanded ? " expanded" : ""}`}
        onClick={collapsible ? () => setExpanded((v) => !v) : undefined}
      >
        <span className="chat-event-label">◌</span>
        <span className="event-body">
          {expanded || !collapsible ? text : text.slice(0, 160) + "…"}
        </span>
        {isDiagnostic && isActive && <span className="spinner" title="In progress" />}
        {collapsible && <span className="event-chevron">{expanded ? "▲" : "▼"}</span>}
      </div>
    );
  }

  if (evt.type === "tool_call") {
    const fullArgs = evt.args ? JSON.stringify(evt.args, null, 2) : "";
    const summaryArgs = evt.args ? JSON.stringify(evt.args) : "";
    const collapsible = summaryArgs.length > 0;
    return (
      <div
        className={`chat-event tool_call${collapsible ? " collapsible" : ""}${expanded ? " expanded" : ""}`}
        onClick={collapsible ? () => setExpanded((v) => !v) : undefined}
      >
        <span className="chat-event-label">⚙</span>
        <span>{evt.tool}</span>
        {!expanded && summaryArgs && (
          <span className="event-muted"> {summaryArgs.slice(0, 60)}{summaryArgs.length > 60 ? "…" : ""}</span>
        )}
        {expanded && fullArgs && (
          <pre className="event-pre">{fullArgs}</pre>
        )}
        {collapsible && <span className="event-chevron">{expanded ? "▲" : "▼"}</span>}
      </div>
    );
  }

  if (evt.type === "tool_result") {
    const preview = evt.preview ?? "";
    const collapsible = preview.length > 80;
    return (
      <div
        className={`chat-event tool_result ${evt.success ? "ok" : "err"}${collapsible ? " collapsible" : ""}${expanded ? " expanded" : ""}`}
        onClick={collapsible ? () => setExpanded((v) => !v) : undefined}
      >
        <span className="chat-event-label">{evt.success ? "✓" : "✗"}</span>
        <span>{evt.tool}</span>
        {preview && !expanded && (
          <span className="event-muted"> — {preview.slice(0, 80)}{collapsible ? "…" : ""}</span>
        )}
        {preview && expanded && (
          <pre className="event-pre">{preview}</pre>
        )}
        {collapsible && <span className="event-chevron">{expanded ? "▲" : "▼"}</span>}
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
  const [profiles, setProfiles] = useState<string[]>([]);
  const [contextProfile, setContextProfile] = useState("general");
  // null = research mode off; string = research mode on with that provider
  const [researchProvider, setResearchProvider] = useState<string | null>(null);
  // Model selector: "provider::name" composite key, e.g. "ollama::qwen3.5:4b"
  const [activeModelKey, setActiveModelKey] = useState("");
  const [catalogModels, setCatalogModels] = useState<CatalogModel[]>([]);
  const [switchingModel, setSwitchingModel] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Auto-scroll to bottom on new messages.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [msgs]);

  // Load session list via REST (GET /api/sessions).
  const loadSessions = useCallback(async () => {
    setLoadingSessions(true);
    try {
      const res = await fetch(`${getBrainUrl()}/api/sessions?limit=50`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const data = (await res.json()) as { sessions?: Session[] };
      setSessions(data.sessions ?? []);
    } catch {
      // ignore — brain may not be connected yet
    } finally {
      setLoadingSessions(false);
    }
  }, []);

  useEffect(() => {
    loadSessions();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Load context profiles for the selector via REST (GET /api/contexts).
  useEffect(() => {
    fetch(`${getBrainUrl()}/api/contexts`, {
      headers: { Authorization: `Bearer ${getApiKey()}` },
    })
      .then((r) => r.json())
      .then((data: { profiles?: Array<{ name: string }> }) => {
        const names = (data.profiles ?? []).map((p) => p.name).sort();
        if (names.length > 0) setProfiles(names);
      })
      .catch(() => {/* ignore — brain may not be connected yet */});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Load active model + catalog from the backend via REST (GET /api/models).
  const loadModels = useCallback(async () => {
    try {
      const res = await fetch(`${getBrainUrl()}/api/models`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const data = (await res.json()) as {
        active_provider?: string;
        active_model?: string;
        catalog_models?: CatalogModel[];
      };
      const provider = (data.active_provider ?? "").toLowerCase();
      const model = data.active_model ?? "";
      if (provider && model) setActiveModelKey(`${provider}::${model}`);
      setCatalogModels(data.catalog_models ?? []);
    } catch {
      // ignore — brain may not be connected yet
    }
  }, []);

  useEffect(() => { loadModels(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const reloadModels = useCallback(async () => {
    setSwitchingModel(true);
    try {
      await callTool("reload_models", {});
      await loadModels();
    } catch {
      // ignore
    } finally {
      setSwitchingModel(false);
    }
  }, [loadModels]);

  const handleModelChange = useCallback(async (value: string) => {
    const sep = value.indexOf("::");
    if (sep === -1) return;
    const provider = value.slice(0, sep);
    const model = value.slice(sep + 2);
    setActiveModelKey(value);
    setSwitchingModel(true);
    try {
      await callTool("use_model", { provider, model });
    } catch {
      // ignore
    } finally {
      setSwitchingModel(false);
    }
  }, []);

  // Switch to an existing session — restore its messages from the server.
  const switchSession = useCallback(async (sid: string) => {
    if (sid === sessionId && msgs.length > 0) return;
    setSessionId(sid);
    setMsgs([]);
    try {
      const res = await fetch(`${getBrainUrl()}/api/sessions/${sid}/entries?limit=200`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const parsed = (await res.json()) as {
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
      contextProfile,
      synthesisProvider: researchProvider ?? undefined,
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
  }, [input, msgs, streaming, sessionId, loadSessions, contextProfile, researchProvider]);

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

  // Find the last user message before a given assistant message id.
  const getPriorUserMsg = (asstId: string): string => {
    const idx = msgs.findIndex((m) => m.id === asstId);
    for (let i = idx - 1; i >= 0; i--) {
      if (msgs[i].kind === "user") return (msgs[i] as UserMsg).text;
    }
    return "";
  };

  const handleReflect = useCallback(async (msgId: string, responseText: string) => {
    const userQuery = getPriorUserMsg(msgId);
    setMsgs((prev) => prev.map((m) =>
      m.kind === "assistant" && m.id === msgId ? { ...m, reflecting: true } : m
    ));
    try {
      const raw = await callTool("reflect_on_work", {
        goal: userQuery || "chat response quality",
        current_state: responseText.slice(0, 4000),
      });
      const data = JSON.parse(raw);
      const reflectionText: string = data.critique ?? data.reflection ?? data.analysis ?? raw;
      await callTool("store_note", {
        content: `Chat reflection\nQ: ${userQuery}\n\nResponse summary: ${responseText.slice(0, 800)}\n\nReflection: ${reflectionText}`,
        note_type: "reflection",
      });
      setMsgs((prev) => prev.map((m) =>
        m.kind === "assistant" && m.id === msgId
          ? { ...m, reflecting: false, reflection: reflectionText }
          : m
      ));
    } catch (e) {
      setMsgs((prev) => prev.map((m) =>
        m.kind === "assistant" && m.id === msgId
          ? { ...m, reflecting: false, reflection: `Error: ${e}` }
          : m
      ));
    }
  }, [msgs]); // eslint-disable-line react-hooks/exhaustive-deps

  const exportChat = () => {
    if (msgs.length === 0) return;

    let markdown = `# Chat Export - Session ${sessionId}\n\n`;
    
    for (const m of msgs) {
      if (m.kind === "user") {
        markdown += `## User\n${m.text}\n\n`;
      } else {
        const text = m.events.find((e) => e.type === "message")?.content
          ?? m.events.filter((e) => e.type === "token").map((e) => e.content ?? "").join("");
        markdown += `## Agent Brain\n${text}\n\n`;
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
    const tokenText = m.events
      .filter((e) => e.type === "token")
      .map((e) => e.content ?? "")
      .join("");
    const displayText = finalText || tokenText;
    const streamEvents = nonDoneEvents.filter((e) => e.type !== "message" && e.type !== "token");
    const showTyping = !m.done && streaming && displayText.length === 0 && streamEvents.length === 0;

    return (
      <div key={m.id} className="chat-msg assistant">
        {streamEvents.map((evt, i) => (
          <EventBubble key={i} evt={evt} isActive={!m.done && streaming && i === 0} />
        ))}
        {showTyping && (
          <div className="typing-indicator">
            <span /><span /><span />
          </div>
        )}
        {displayText && (
          <div className="chat-msg-bubble markdown-body" style={{ position: "relative", paddingRight: "36px" }}>
            <button
              className="copy-btn"
              onClick={() => navigator.clipboard.writeText(displayText)}
              title="Copy text"
              aria-label="Copy text"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
                <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
              </svg>
            </button>
            <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>{displayText}</ReactMarkdown>
          </div>
        )}
        {m.done && displayText && (
          <div className="chat-msg-meta">
            {m.reflecting ? (
              <span className="event-muted" style={{ fontSize: 10 }}>Reflecting…</span>
            ) : !m.reflection ? (
              <button
                className="btn-ghost"
                style={{ fontSize: 10, padding: "1px 6px" }}
                onClick={() => handleReflect(m.id, displayText)}
                title="Reflect on this response and store as a note"
              >
                ↺ Reflect
              </button>
            ) : null}
          </div>
        )}
        {m.reflection && (
          <div className="chat-event thinking" style={{ marginTop: 4, fontSize: 11 }}>
            <span className="chat-event-label">↺</span>
            <span className="event-body">{m.reflection}</span>
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
            <div className="input-row-actions">
              <div className="input-row-selectors">
                {activeModelKey && (
                  <>
                  <select
                    className="profile-select model-select"
                    value={activeModelKey}
                    onChange={(e) => handleModelChange(e.target.value)}
                    disabled={streaming || switchingModel || catalogModels.length === 0}
                    title={switchingModel ? "Switching model…" : "Active LLM model"}
                  >
                    {catalogModels.length === 0 ? (
                      <option value={activeModelKey}>{activeModelKey.replace("::", " / ")}</option>
                    ) : (
                      Object.entries(
                        catalogModels.reduce<Record<string, CatalogModel[]>>((acc, m) => {
                          (acc[m.provider] ??= []).push(m);
                          return acc;
                        }, {})
                      ).map(([provider, models]) => (
                        <optgroup key={provider} label={provider}>
                          {models.map((m) => (
                            <option key={`${m.provider}::${m.name}`} value={`${m.provider}::${m.name}`}>
                              {m.name}
                            </option>
                          ))}
                        </optgroup>
                      ))
                    )}
                  </select>
                  <button
                    className="reload-models-btn"
                    onClick={reloadModels}
                    disabled={streaming || switchingModel}
                    title="Reload models from models.yaml"
                  >↺</button>
                  </>
                )}
                <select
                  className="profile-select"
                  value={contextProfile}
                  onChange={(e) => setContextProfile(e.target.value)}
                  disabled={streaming}
                  title="Context profile — limits tools sent to the model"
                >
                  {(profiles.length > 0 ? profiles : ["general"]).map((p) => (
                    <option key={p} value={p}>{p}</option>
                  ))}
                </select>
                <button
                  className={`research-toggle${researchProvider !== null ? " active" : ""}`}
                  onClick={() => setResearchProvider((v) => v === null ? "gemini" : null)}
                  disabled={streaming}
                  title="Research mode: local model gathers data, strong model synthesizes"
                >
                  ⚗ Research
                </button>
                {researchProvider !== null && (
                  <select
                    className="profile-select"
                    value={researchProvider}
                    onChange={(e) => setResearchProvider(e.target.value)}
                    disabled={streaming}
                    title="Model used to synthesize research findings"
                  >
                    <option value="gemini">Synthesize: Gemini</option>
                    <option value="anthropic">Synthesize: Claude</option>
                  </select>
                )}
              </div>
              <button className="btn" onClick={send} disabled={streaming || !input.trim()}>
                Send
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
