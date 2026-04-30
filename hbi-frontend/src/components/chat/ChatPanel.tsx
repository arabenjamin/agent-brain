import React, { useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import "highlight.js/styles/github-dark.css";
import type { ChatEvent, ChatHistoryMessage } from "../../api/chat";
import { streamChat } from "../../api/chat";
import { callTool } from "../../api/mcp";
import { getBrainUrl, getApiKey } from "../../api/config";

interface AgentNotification {
  id: string;
  message: string;
  context: string;
  related_session_id: string;
  created_at: string;
  read: boolean;
}

// ── Types ────────────────────────────────────────────────────────────────────

interface UserMsg {
  kind: "user";
  id: string;
  text: string;
  ts: string;
}

interface AssistantMsg {
  kind: "assistant";
  id: string;
  events: ChatEvent[];
  done: boolean;
  reflecting?: boolean;
  reflection?: string;
  ts: string;
  model?: string;
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
  provider: string;
  model: string;
}

interface AvailableProvider {
  type: string;
  name: string;
  cost: string;
}

interface ModelUsageStat {
  model: string;
  total_calls: number;
  successes: number;
  failures: number;
  success_rate: number;
  avg_duration_ms: number | null;
  total_tokens_in: number | null;
  total_tokens_out: number | null;
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function uid() {
  if (typeof crypto !== "undefined" && crypto.randomUUID) {
    return crypto.randomUUID();
  }
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

function formatTime(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
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

// ── Agent notification banner ─────────────────────────────────────────────────

function NotificationBanner({
  notifications,
  onResume,
  onDismiss,
}: {
  notifications: AgentNotification[];
  onResume: (sessionId: string) => void;
  onDismiss: (id: string) => void;
}) {
  if (notifications.length === 0) return null;
  return (
    <div className="agent-notifications">
      {notifications.map((n) => (
        <div key={n.id} className="agent-notification">
          <div className="agent-notification-header">
            <span className="agent-notification-label">🤖 Agent message</span>
            {n.context && <span className="agent-notification-context">{n.context}</span>}
            <button
              className="agent-notification-dismiss"
              onClick={() => onDismiss(n.id)}
              title="Dismiss"
            >
              ×
            </button>
          </div>
          <div className="agent-notification-body">{n.message}</div>
          {n.related_session_id && (
            <button
              className="btn"
              style={{ fontSize: 11, padding: "3px 10px", marginTop: 6 }}
              onClick={() => onResume(n.related_session_id)}
            >
              Continue conversation
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

// ── Thread drawer types ────────────────────────────────────────────────────────

interface ThreadMsg {
  id: string;
  role: "user" | "assistant";
  content: string;
  events?: ChatEvent[];
  done?: boolean;
}

interface ThreadState {
  sessionId: string;
  title: string;
  msgs: ThreadMsg[];
  input: string;
  streaming: boolean;
}

// ── Thread Drawer component ────────────────────────────────────────────────────

function ThreadDrawer({
  thread,
  onClose,
  onInputChange,
  onSend,
}: {
  thread: ThreadState;
  onClose: () => void;
  onInputChange: (v: string) => void;
  onSend: () => void;
}) {
  const bottomRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [thread.msgs]);

  const handleKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      onSend();
    }
  };

  return (
    <div className="thread-drawer">
      <div className="thread-drawer-header">
        <span className="thread-drawer-label">Thread</span>
        <span className="thread-drawer-title">{thread.title}</span>
        <button className="thread-drawer-close" onClick={onClose} title="Close thread">×</button>
      </div>
      <div className="thread-drawer-messages">
        {thread.msgs.map((m) => (
          <div key={m.id} className={`thread-msg ${m.role}`}>
            <div className="thread-msg-bubble markdown-body" style={m.role === "assistant" ? { fontSize: 12 } : undefined}>
              {m.role === "assistant" && m.events && m.events.length > 0 ? (
                <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
                  {m.events.filter(e => e.type === "token").map(e => e.content ?? "").join("") ||
                   m.events.find(e => e.type === "message")?.content ||
                   m.content}
                </ReactMarkdown>
              ) : (
                <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
                  {m.content}
                </ReactMarkdown>
              )}
            </div>
          </div>
        ))}
        {thread.streaming && (
          <div className="thread-msg assistant">
            <div className="thread-msg-bubble">
              <div className="typing-indicator"><span /><span /><span /></div>
            </div>
          </div>
        )}
        <div ref={bottomRef} />
      </div>
      <div className="thread-drawer-input-row">
        <textarea
          rows={2}
          placeholder="Reply in thread…"
          value={thread.input}
          onChange={(e) => onInputChange(e.target.value)}
          onKeyDown={handleKey}
          disabled={thread.streaming}
          style={{ resize: "none" }}
        />
        <button className="btn" onClick={onSend} disabled={thread.streaming || !thread.input.trim()}>
          Send
        </button>
      </div>
    </div>
  );
}

// ── Chat Settings Bar ─────────────────────────────────────────────────────────

function ChatSettingsBar({
  activeModelKey,
  activeProvider,
  catalogModels,
  availableProviders,
  modelUsage,
  switchingModel,
  streaming,
  contextProfile,
  profiles,
  researchProvider,
  onModelChange,
  onReloadModels,
  onProfileChange,
  onResearchToggle,
  onResearchProviderChange,
}: {
  activeModelKey: string;
  activeProvider: string;
  catalogModels: CatalogModel[];
  availableProviders: AvailableProvider[];
  modelUsage: ModelUsageStat[];
  switchingModel: boolean;
  streaming: boolean;
  contextProfile: string;
  profiles: string[];
  researchProvider: string | null;
  onModelChange: (provider: string, model: string) => void;
  onReloadModels: () => void;
  onProfileChange: (v: string) => void;
  onResearchToggle: () => void;
  onResearchProviderChange: (v: string) => void;
}) {
  // Parse current provider/model from the composite key
  const sep = activeModelKey.indexOf("::");
  const currentModel = sep >= 0 ? activeModelKey.slice(sep + 2) : activeModelKey;
  const currentProvider = sep >= 0 ? activeModelKey.slice(0, sep) : activeProvider;

  const [editModel, setEditModel] = useState(currentModel);
  const [selectedProvider, setSelectedProvider] = useState(currentProvider);

  // Sync local edits when active model changes externally
  useEffect(() => { setEditModel(currentModel); }, [currentModel]);
  useEffect(() => { setSelectedProvider(currentProvider); }, [currentProvider]);

  const hasCatalog = catalogModels.length > 0;

  const handleProviderChange = (p: string) => {
    setSelectedProvider(p);
    // If catalog exists, auto-pick the first model for this provider
    if (hasCatalog) {
      const first = catalogModels.find((m) => m.provider === p);
      if (first) onModelChange(p, first.name);
    }
  };

  const applyModel = () => {
    if (editModel.trim()) onModelChange(selectedProvider, editModel.trim());
  };

  const handleModelInputKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") applyModel();
  };

  // Find usage stat for active model (unused in render but kept for future tooltip)
  void modelUsage.find((s) => s.model === currentModel);

  return (
    <div className="chat-settings-bar">
      {/* Model usage strip */}
      {modelUsage.length > 0 && (
        <div className="model-usage-strip">
          {modelUsage.slice(0, 5).map((s) => (
            <span
              key={s.model}
              className={`model-usage-chip${s.model === currentModel ? " active" : ""}`}
              title={`${s.total_calls} calls · ${s.successes} ok · avg ${s.avg_duration_ms != null ? Math.round(s.avg_duration_ms) : "?"}ms`}
            >
              <span className="model-usage-name">{s.model.split(":")[0]}</span>
              <span className="model-usage-count">{s.total_calls}</span>
              <span className={`model-usage-rate${s.success_rate < 0.8 ? " warn" : ""}`}>
                {Math.round(s.success_rate * 100)}%
              </span>
            </span>
          ))}
        </div>
      )}

      <div className="chat-settings-row">
        {/* Provider selector */}
        {availableProviders.length > 0 && (
          <div className="chat-settings-group">
            <span className="chat-settings-label">Provider</span>
            <select
              className="profile-select"
              value={selectedProvider}
              onChange={(e) => handleProviderChange(e.target.value)}
              disabled={streaming || switchingModel}
              title="LLM provider"
            >
              {availableProviders.map((p) => (
                <option key={p.type} value={p.type}>{p.name}</option>
              ))}
            </select>
          </div>
        )}

        {/* Model selector — catalog select when available, text input otherwise */}
        <div className="chat-settings-group">
          <span className="chat-settings-label">Model</span>
          {hasCatalog ? (
            <select
              className="profile-select model-select"
              value={`${selectedProvider}::${currentModel}`}
              onChange={(e) => {
                const s = e.target.value.indexOf("::");
                if (s >= 0) onModelChange(e.target.value.slice(0, s), e.target.value.slice(s + 2));
              }}
              disabled={streaming || switchingModel}
              title="Active LLM model"
            >
              {Object.entries(
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
              ))}
            </select>
          ) : (
            <input
              type="text"
              className="model-name-input"
              value={editModel}
              onChange={(e) => setEditModel(e.target.value)}
              onBlur={applyModel}
              onKeyDown={handleModelInputKey}
              disabled={streaming || switchingModel}
              placeholder="model name"
              title="Type a model name and press Enter"
              style={{ width: 180 }}
            />
          )}
          <button
            className="reload-models-btn"
            onClick={onReloadModels}
            disabled={streaming || switchingModel}
            title="Reload models from models.yaml"
          >↺</button>
        </div>

        {/* Context profile */}
        <div className="chat-settings-group">
          <span className="chat-settings-label">Profile</span>
          <select
            className="profile-select"
            value={contextProfile}
            onChange={(e) => onProfileChange(e.target.value)}
            disabled={streaming}
            title="Context profile — limits tools sent to the model"
          >
            {(profiles.length > 0 ? profiles : ["general"]).map((p) => (
              <option key={p} value={p}>{p}</option>
            ))}
          </select>
        </div>

        {/* Research mode */}
        <div className="chat-settings-group">
          <button
            className={`research-toggle${researchProvider !== null ? " active" : ""}`}
            onClick={onResearchToggle}
            disabled={streaming}
            title="Research mode: local model gathers data, strong model synthesizes"
          >
            ⚗ Research
          </button>
          {researchProvider !== null && (
            <select
              className="profile-select"
              value={researchProvider}
              onChange={(e) => onResearchProviderChange(e.target.value)}
              disabled={streaming}
              title="Model used to synthesize research findings"
            >
              <option value="gemini">Synthesize: Gemini</option>
              <option value="anthropic">Synthesize: Claude</option>
            </select>
          )}
        </div>
      </div>
    </div>
  );
}

// ── Main component ────────────────────────────────────────────────────────────

export default function ChatPanel({ onNotifCountChange, visible }: { onNotifCountChange?: (count: number) => void; visible?: boolean }) {
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [sessionId, setSessionId] = useState<string>(() => uid());
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loadingSessions, setLoadingSessions] = useState(false);
  const [profiles, setProfiles] = useState<string[]>([]);
  const [contextProfile, setContextProfile] = useState("general");
  const [researchProvider, setResearchProvider] = useState<string | null>(null);
  const [activeModelKey, setActiveModelKey] = useState("");
  const [activeProvider, setActiveProvider] = useState("");
  const [catalogModels, setCatalogModels] = useState<CatalogModel[]>([]);
  const [availableProviders, setAvailableProviders] = useState<AvailableProvider[]>([]);
  const [modelUsage, setModelUsage] = useState<ModelUsageStat[]>([]);
  const [switchingModel, setSwitchingModel] = useState(false);
  const [notifications, setNotifications] = useState<AgentNotification[]>([]);
  const [thread, setThread] = useState<ThreadState | null>(null);
  const threadAbortRef = useRef<AbortController | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  // Cache full Msg[] per session to preserve thinking/tool events when switching sessions
  const sessionCacheRef = useRef<Map<string, Msg[]>>(new Map());

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [msgs]);

  const loadSessions = useCallback(async () => {
    setLoadingSessions(true);
    try {
      const res = await fetch(`${getBrainUrl()}/api/sessions?limit=50`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const data = (await res.json()) as { sessions?: Session[] };
      setSessions(data.sessions ?? []);
    } catch {
      // ignore
    } finally {
      setLoadingSessions(false);
    }
  }, []);

  useEffect(() => {
    loadSessions();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!visible) return;
    const load = async () => {
      try {
        const res = await fetch(`${getBrainUrl()}/api/notifications?unread=true`, {
          headers: { Authorization: `Bearer ${getApiKey()}` },
        });
        if (res.ok) {
          const data = await res.json() as { notifications?: AgentNotification[] };
          const notifs = data.notifications ?? [];
          setNotifications(notifs);
          onNotifCountChange?.(notifs.length);
        }
      } catch {
        // ignore
      }
    };
    load();
  }, [visible]); // eslint-disable-line react-hooks/exhaustive-deps

  const dismissNotification = useCallback(async (id: string) => {
    setNotifications((prev) => {
      const next = prev.filter((n) => n.id !== id);
      onNotifCountChange?.(next.length);
      return next;
    });
    try {
      await fetch(`${getBrainUrl()}/api/notifications/${id}/read`, {
        method: "POST",
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
    } catch {
      // ignore
    }
  }, [onNotifCountChange]);

  const openThread = useCallback(async (n: AgentNotification) => {
    const sid = n.related_session_id;
    dismissNotification(n.id);

    if (!sid) return;

    const seedMsg: ThreadMsg = {
      id: uid(),
      role: "assistant",
      content: n.message,
    };
    setThread({
      sessionId: sid,
      title: n.context || sid,
      msgs: [seedMsg],
      input: "",
      streaming: false,
    });

    try {
      const res = await fetch(`${getBrainUrl()}/api/sessions/${sid}/entries?limit=200`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const parsed = (await res.json()) as {
        entries?: Array<{ role: string; content: string }>;
      };
      const entries = (parsed.entries ?? []).filter(
        (e) => e.role === "user" || e.role === "assistant"
      );
      if (entries.length > 0) {
        const restored: ThreadMsg[] = entries.map((e) => ({
          id: uid(),
          role: e.role as "user" | "assistant",
          content: e.content,
        }));
        setThread((prev) =>
          prev ? { ...prev, msgs: restored } : prev
        );
      }
    } catch {
      // keep seed message
    }
  }, [dismissNotification]);

  const sendThreadMessage = useCallback(async () => {
    if (!thread || !thread.input.trim() || thread.streaming) return;
    const text = thread.input.trim();
    const userMsg: ThreadMsg = { id: uid(), role: "user", content: text };
    const asstId = uid();
    const asstMsg: ThreadMsg = {
      id: asstId,
      role: "assistant",
      content: "",
      events: [],
      done: false,
    };

    setThread((prev) =>
      prev ? { ...prev, msgs: [...prev.msgs, userMsg, asstMsg], input: "", streaming: true } : prev
    );

    const history = (thread.msgs).map((m) => ({
      role: m.role as "user" | "assistant",
      content: m.content,
    }));

    const abort = new AbortController();
    threadAbortRef.current = abort;

    await streamChat({
      message: text,
      history,
      sessionId: thread.sessionId,
      contextProfile,
      signal: abort.signal,
      onEvent: (evt) => {
        setThread((prev) => {
          if (!prev) return prev;
          const updated = prev.msgs.map((m) => {
            if (m.id !== asstId) return m;
            const newEvents = [...(m.events ?? []), evt];
            const finalText = newEvents.find(e => e.type === "message")?.content ?? "";
            const tokenText = newEvents.filter(e => e.type === "token").map(e => e.content ?? "").join("");
            return {
              ...m,
              events: newEvents,
              content: finalText || tokenText || m.content,
              done: evt.type === "done",
            };
          });
          return { ...prev, msgs: updated };
        });
      },
    });

    setThread((prev) => prev ? { ...prev, streaming: false } : prev);
    threadAbortRef.current = null;
    loadSessions();
  }, [thread, contextProfile, loadSessions]);

  useEffect(() => {
    fetch(`${getBrainUrl()}/api/contexts`, {
      headers: { Authorization: `Bearer ${getApiKey()}` },
    })
      .then((r) => r.json())
      .then((data: { profiles?: Array<{ name: string }> }) => {
        const names = (data.profiles ?? []).map((p) => p.name).sort();
        if (names.length > 0) setProfiles(names);
      })
      .catch(() => {});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const loadModels = useCallback(async () => {
    try {
      const res = await fetch(`${getBrainUrl()}/api/models`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const data = (await res.json()) as {
        active_provider?: string;
        active_model?: string;
        catalog_models?: CatalogModel[];
        available_providers?: AvailableProvider[];
      };
      const provider = (data.active_provider ?? "").toLowerCase();
      const model = data.active_model ?? "";
      if (provider && model) {
        setActiveModelKey(`${provider}::${model}`);
        setActiveProvider(provider);
      }
      setCatalogModels(data.catalog_models ?? []);
      setAvailableProviders(data.available_providers ?? []);
    } catch {
      // ignore
    }
  }, []);

  const loadModelUsage = useCallback(async () => {
    try {
      const res = await fetch(`${getBrainUrl()}/api/models/usage`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      if (res.ok) {
        const data = (await res.json()) as { models?: ModelUsageStat[]; available?: boolean };
        setModelUsage(data.models ?? []);
      }
    } catch {
      // ignore — telemetry not configured
    }
  }, []);

  useEffect(() => {
    loadModels();
    loadModelUsage();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const reloadModels = useCallback(async () => {
    setSwitchingModel(true);
    try {
      await callTool("reload_models", {});
      await loadModels();
      await loadModelUsage();
    } catch {
      // ignore
    } finally {
      setSwitchingModel(false);
    }
  }, [loadModels, loadModelUsage]);

  const handleModelChange = useCallback(async (provider: string, model: string) => {
    const key = `${provider}::${model}`;
    setActiveModelKey(key);
    setActiveProvider(provider);
    setSwitchingModel(true);
    try {
      await callTool("use_model", { provider, model });
    } catch {
      // ignore
    } finally {
      setSwitchingModel(false);
    }
  }, []);

  const switchSession = useCallback(async (sid: string) => {
    if (sid === sessionId && msgs.length > 0) return;

    // Save current session's full message list (preserves thinking/tool events)
    if (msgs.length > 0) {
      sessionCacheRef.current.set(sessionId, msgs);
    }

    setSessionId(sid);
    setMsgs([]);

    // Restore from cache if available (full events preserved)
    const cached = sessionCacheRef.current.get(sid);
    if (cached) {
      setMsgs(cached);
      return;
    }

    // Otherwise fetch from server (text-only fallback)
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
          const ts = new Date().toISOString();
          if (e.role === "user") {
            return { kind: "user" as const, id: uid(), text: e.content, ts };
          }
          return {
            kind: "assistant" as const,
            id: uid(),
            events: [{ type: "message" as const, content: e.content }],
            done: true,
            ts,
          };
        });
      setMsgs(restored);
    } catch {
      setMsgs([]);
    }
  }, [sessionId, msgs]);

  const archiveSession = useCallback(async (sid: string, e: React.MouseEvent) => {
    e.stopPropagation();
    try {
      await fetch(`${getBrainUrl()}/api/sessions/${sid}/archive`, {
        method: "POST",
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      setSessions((prev) => prev.filter((s) => s.session_id !== sid));
      // If the archived session was active, start a new chat
      if (sid === sessionId) {
        setSessionId(uid());
        setMsgs([]);
      }
    } catch {
      // ignore
    }
  }, [sessionId]);

  const newChat = useCallback(() => {
    if (msgs.length > 0) {
      sessionCacheRef.current.set(sessionId, msgs);
    }
    setSessionId(uid());
    setMsgs([]);
    loadSessions();
  }, [msgs, sessionId, loadSessions]);

  const send = useCallback(async () => {
    const text = input.trim();
    if (!text || streaming) return;

    const now = new Date().toISOString();
    const userMsg: UserMsg = { kind: "user", id: uid(), text, ts: now };
    const asstId = uid();
    const asstMsg: AssistantMsg = {
      kind: "assistant",
      id: asstId,
      events: [],
      done: false,
      ts: now,
      model: activeModelKey,
    };

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
    loadSessions();
  }, [input, msgs, streaming, sessionId, loadSessions, contextProfile, researchProvider, activeModelKey]);

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
          <div className="chat-msg-bubble markdown-body user-markdown">
            <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
              {m.text}
            </ReactMarkdown>
          </div>
          <div className="chat-msg-ts">{formatTime(m.ts)}</div>
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
    const modelLabel = m.model ? m.model.split("::")[1] || m.model : "";

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
            <span className="chat-msg-ts">{formatTime(m.ts)}</span>
            {modelLabel && <span className="chat-msg-model">{modelLabel}</span>}
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
        </div>
      </div>

      <ChatSettingsBar
        activeModelKey={activeModelKey}
        activeProvider={activeProvider}
        catalogModels={catalogModels}
        availableProviders={availableProviders}
        modelUsage={modelUsage}
        switchingModel={switchingModel}
        streaming={streaming}
        contextProfile={contextProfile}
        profiles={profiles}
        researchProvider={researchProvider}
        onModelChange={handleModelChange}
        onReloadModels={reloadModels}
        onProfileChange={setContextProfile}
        onResearchToggle={() => setResearchProvider((v) => v === null ? "gemini" : null)}
        onResearchProviderChange={setResearchProvider}
      />

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
                  {truncate(s.title, 32)}
                </div>
                <div className="session-item-meta">
                  {formatDate(s.started_at)}
                  {s.msg_count > 0 && ` · ${s.msg_count} msgs`}
                </div>
                <button
                  className="session-archive-btn"
                  onClick={(e) => archiveSession(s.session_id, e)}
                  title="Archive this chat (hidden from list, kept for training data)"
                >
                  🗄
                </button>
              </div>
            ))}
          </div>
        </div>

        {/* ── Chat area ── */}
        <div className="chat-area" style={{ minWidth: 0 }}>
          <div className="chat-messages">
            <NotificationBanner
              notifications={notifications}
              onResume={(sid) => {
                const n = notifications.find((x) => x.related_session_id === sid);
                if (n) openThread(n);
              }}
              onDismiss={dismissNotification}
            />
            {msgs.length === 0 && notifications.length === 0 && (
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
              <button className="btn" onClick={send} disabled={streaming || !input.trim()}>
                Send
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
        </div>

        {/* ── Thread drawer ── */}
        {thread && (
          <ThreadDrawer
            thread={thread}
            onClose={() => {
              threadAbortRef.current?.abort();
              setThread(null);
            }}
            onInputChange={(v) => setThread((prev) => prev ? { ...prev, input: v } : prev)}
            onSend={sendThreadMessage}
          />
        )}
      </div>
    </div>
  );
}
