import ModelPicker from "../ui/ModelPicker";
import React, { useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import "highlight.js/styles/github-dark.css";
import type { ChatEvent, ChatHistoryMessage } from "../../api/chat";
import { streamChat } from "../../api/chat";
import { callTool } from "../../api/mcp";
import { apiRequest } from "../../api/client";
import { useDefaultProfile, useProfiles, type ProfileInfo } from "../../api/queries";
import { hrefTo } from "../../nav";
import ContextDock from "./ContextDock";

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
  /** Working-memory role when it is not "assistant" (e.g. "observation"). */
  role?: string;
}

type Msg = UserMsg | AssistantMsg;

interface Session {
  session_id: string;
  started_at: string;
  msg_count: number;
  /** turn-0 content — the user's opening message for a chat, boilerplate otherwise. */
  title: string;
  /** Goal of the Task this session was scratch space for, when it maps to one. */
  task_goal?: string | null;
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
  /** Already aggregated server-side; was simply never rendered. */
  total_cost: number | null;
}

/** 1234567 -> "1.2M", 34567 -> "34.6K". Spend is read at a glance. */
function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(n);
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

// ── Session labels ───────────────────────────────────────────────────────────
//
// Agent-created sessions all open with the same banner line — every claim sweep
// starts "## CLAIM VERIFICATION SWEEP", every news sweep "## METRO DETROIT —
// raw search results" — so labelling the list with turn-0 content rendered all
// eleven claim sweeps as one repeated string, distinguishable only by the date
// underneath. The session id already encodes what kind of run it was; that,
// plus the Task goal or the date, is what actually tells them apart.
//
// This list is exactly the set of session ids that workflows create.
// Keep it in sync with `grep -rhoE 'session_id: *"[^"]+"' plugins/*/workflows/`,
// and do NOT add speculative prefixes: a human session that happens to start
// with one gets its real identity — the user's opening message — replaced by a
// generic run label. `eval-*` and `verify-*` are hand-made test chats and were
// mislabelled exactly that way before this list was checked against the source.
//
// Longest prefix first: `news-raw-` must be tested before `news-`.
const SESSION_KINDS: ReadonlyArray<readonly [string, string]> = [
  ["news-raw-", "News sweep"],
  ["news-", "News brief"],
  ["todos-", "Todo review"],
  ["claims-", "Claim verification"],
  ["gap-", "Knowledge gap"],
  ["video-", "Video learning"],
  ["offgrid-", "Off-grid watch"],
  ["slmwatch-", "SLM benchmark watch"],
  ["tripwire-", "Hardware tripwire"],
  ["synthesis-", "Tech dependency synthesis"],
  ["conduit-", "Phone (Conduit)"],
];

/** `2026-08-20` → `Aug 20`. Returns null for anything that is not a bare date. */
function dateSuffixLabel(rest: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(rest);
  if (!m) return null;
  const d = new Date(`${rest}T00:00:00`);
  if (Number.isNaN(d.getTime())) return null;
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/**
 * Strip the markdown banner and any JSON body from turn-0 content, leaving the
 * first line of readable prose. Used only as the last resort.
 */
function cleanSnippet(content: string): string {
  const line = content
    .split("\n")
    .map((l) => l.replace(/^#+\s*/, "").trim())
    .find((l) => l.length > 0 && !/^[[{]/.test(l));
  return (line ?? content).replace(/\s+/g, " ").trim();
}

/**
 * Human label for a session, plus whether the date is already part of it (the
 * meta row omits it when so, rather than printing "Aug 20" twice).
 */
function sessionLabel(s: Session): { label: string; datedInLabel: boolean } {
  const kind = SESSION_KINDS.find(([prefix]) => s.session_id.startsWith(prefix));

  if (kind) {
    const [prefix, name] = kind;
    const rest = s.session_id.slice(prefix.length);

    // A Task goal is the most specific thing available — it names the topic,
    // not just the run type.
    const goal = s.task_goal?.trim();
    if (goal) {
      // Chain-routing prefixes ("fill knowledge gap: X", "watch video: X") are
      // already conveyed by the kind name; the part after the colon is the
      // topic. Left intact when it would leave nothing behind.
      const topic = goal.includes(":") ? goal.slice(goal.indexOf(":") + 1).trim() : goal;
      return { label: `${name}: ${topic || goal}`, datedInLabel: false };
    }

    const dated = dateSuffixLabel(rest);
    if (dated) return { label: `${name} · ${dated}`, datedInLabel: true };

    // Unresolvable suffix (Task deleted, or an id-scoped run) — a short id at
    // least keeps sibling runs distinguishable.
    return { label: `${name} · ${rest.slice(0, 8)}`, datedInLabel: false };
  }

  // Plain chat: turn 0 is the user's own opening message, which is already the
  // best possible label.
  const snippet = cleanSnippet(s.title || s.session_id);
  return { label: snippet || s.session_id, datedInLabel: false };
}

// ── Event bubble ─────────────────────────────────────────────────────────────

/**
 * The model that actually answered this turn, from the server's own `⚙`
 * announcements: the opening `⚙ provider=… | model=…` line, then any fallback
 * (`… continuing on <model> (<provider>)`, `… retrying on <model> (<provider>)`).
 * The last one wins. A sub-agent's lines arrive prefixed `[agent]`, so they never
 * match — the worker's model is not the answerer's.
 *
 * The label used to be the panel's selected chat model, stamped when the message
 * was created. With a profile pinning another model (the front door on
 * claude-sonnet-5) every reply was labelled gpt-oss — the delegated sub-agent's
 * model — so a Sonnet answer read as the sub-agent's.
 */
function answeringModel(events: ChatEvent[]): string | null {
  let model: string | null = null;
  for (const e of events) {
    if (e.type !== "thinking" || !e.content?.startsWith("⚙")) continue;
    const opening = e.content.match(/^⚙ provider=[^ |]+ \| model=([^ |]+)/);
    const fallback = e.content.match(/^⚙ .*?(?:continuing|retrying) on (\S+) \(/);
    model = opening?.[1] ?? fallback?.[1] ?? model;
  }
  return model;
}

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
          <>
            {typeof evt.model_saw_chars === "number" && (
              <div className="event-note">
                Showing all {preview.length.toLocaleString()} characters. The model read the
                first {evt.model_saw_chars.toLocaleString()} — anything below that point
                informed this view, not its answer.
              </div>
            )}
            <pre className="event-pre">{preview}</pre>
          </>
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

// ── Header: agent · model · tokens ───────────────────────────────────────────

const PROFILE_KEY = "hbi.chat.profile";
const SIDEBAR_KEY = "hbi.chat.sidebar";

function storedProfile(): string | null {
  try {
    return localStorage.getItem(PROFILE_KEY) || null;
  } catch {
    return null;
  }
}

function storeProfile(name: string | null) {
  try {
    if (name) localStorage.setItem(PROFILE_KEY, name);
    else localStorage.removeItem(PROFILE_KEY);
  } catch {
    // private window: the choice lasts this page only
  }
}

function ChatHeader({
  activeModelKey,
  modelUsage,
  streaming,
  profile,
  defaultProfile,
  profiles,
  onProfile,
  onExport,
  canExport,
  sidebarOpen,
  onToggleSidebar,
  turnModel,
  onTurnModel,
}: {
  activeModelKey: string;
  modelUsage: ModelUsageStat[];
  streaming: boolean;
  /** null: the deployment's front door (no profile sent). */
  profile: string | null;
  defaultProfile: string | null;
  profiles: ProfileInfo[];
  onProfile: (name: string | null) => void;
  onExport: () => void;
  canExport: boolean;
  sidebarOpen: boolean;
  onToggleSidebar: () => void;
  /** A model chosen for the next turns; "" = the agent's own or the chat default. */
  turnModel: string;
  onTurnModel: (model: string) => void;
}) {
  const sep = activeModelKey.indexOf("::");
  const chatModel = sep >= 0 ? activeModelKey.slice(sep + 2) : activeModelKey;
  const effective = profile ?? defaultProfile;
  const effectiveProfile = profiles.find((p) => p.name === effective);
  const pinned = effectiveProfile?.model_preference;
  const model = pinned || chatModel;

  // TOKEN SPEND, last 24h: the total, with where it went in the tooltip. A
  // model reporting no tokens is a broken ledger, not a free model, so it is
  // named rather than read as zero spend.
  const tin = modelUsage.reduce((a, s) => a + (s.total_tokens_in ?? 0), 0);
  const tout = modelUsage.reduce((a, s) => a + (s.total_tokens_out ?? 0), 0);
  const cost = modelUsage.reduce((a, s) => a + (s.total_cost ?? 0), 0);
  const blind = modelUsage.filter((s) => s.total_calls > 0 && !s.total_tokens_in && !s.total_tokens_out);
  const breakdown = modelUsage
    .slice()
    .sort((a, b) => (b.total_tokens_in ?? 0) + (b.total_tokens_out ?? 0) - ((a.total_tokens_in ?? 0) + (a.total_tokens_out ?? 0)))
    .map((s) => `${s.model}: ${fmtTokens((s.total_tokens_in ?? 0) + (s.total_tokens_out ?? 0))} · ${s.total_calls} calls${s.total_cost ? ` · $${s.total_cost.toFixed(4)}` : ""}`)
    .join("\n");

  return (
    <div className="chat-header">
      <button type="button" className="btn-ghost chat-sidebar-toggle" onClick={onToggleSidebar}
        title={sidebarOpen ? "Hide history" : "Show history"} aria-pressed={sidebarOpen}>☰</button>
      <span className="chat-title">Chat</span>
      <label className="chat-agent" title="Which agent answers this conversation">
        <span className="wk-dim">agent</span>
        <select className="ui-input auto" value={profile ?? ""} disabled={streaming}
          onChange={(e) => onProfile(e.target.value || null)}>
          <option value="">{defaultProfile ? `front door (${defaultProfile})` : "auto (chosen per message)"}</option>
          {profiles.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
        </select>
      </label>
      <label className="chat-model" title="The model this conversation's turns run on; an agent may still delegate to others">
        <span className="wk-dim">model</span>
        <ModelPicker
          value={turnModel}
          onChange={onTurnModel}
          purpose="chat"
          dataClass={effectiveProfile?.data_class === "public" ? "public" : "private"}
          defaultLabel={pinned ? `${effective}'s own (${pinned})` : `chat default (${model || "—"})`}
          className="ui-input auto"
          style={{ maxWidth: 280 }}
        />
      </label>
      {modelUsage.length > 0 && (
        <span className="chat-tokens" title={`Last 24 h, all models:\n${breakdown}`}>
          <span className="wk-dim">24h</span> ↑{fmtTokens(tin)} ↓{fmtTokens(tout)}{cost > 0 && ` · $${cost.toFixed(3)}`}
          {blind.length > 0 && (
            <span className="tone-text-yellow" title={`No token counts recorded for: ${blind.map((b) => b.model).join(", ")}. Not in the totals.`}> · ⚠ {blind.length} unmetered</span>
          )}
        </span>
      )}
      <span className="chat-header-end">
        <a className="wk-dim" href={hrefTo("settings", { section: "agents" })} title="Create and edit agents">edit agents →</a>
        <button type="button" className="btn-ghost" onClick={onExport} disabled={!canExport} title="Export conversation as Markdown">Export</button>
      </span>
    </div>
  );
}

// ── Main component ────────────────────────────────────────────────────────────

export default function ChatPanel({
  visible,
  session,
}: {
  visible?: boolean;
  /** A conversation to open (`ask=<id>`): the Feed, an item's thread, Home. */
  session?: string;
}) {
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [sessionId, setSessionId] = useState<string>(() => uid());
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loadingSessions, setLoadingSessions] = useState(false);
  // null = the deployment's front door: no profile is sent and the server
  // applies CHAT_DEFAULT_PROFILE. (A hardcoded "general" here bypassed it.)
  const [contextProfile, setContextProfileState] = useState<string | null>(storedProfile);
  // Not remembered across reloads: a model chosen for one conversation should
  // not quietly carry into the next.
  const [turnModel, setTurnModel] = useState("");
  const setContextProfile = (name: string | null) => {
    setContextProfileState(name);
    storeProfile(name);
  };
  const defaultProfile = useDefaultProfile().data ?? null;
  const [sidebarOpen, setSidebarOpen] = useState<boolean>(() => {
    try {
      return localStorage.getItem(SIDEBAR_KEY) !== "closed";
    } catch {
      return true;
    }
  });
  const toggleSidebar = () => {
    setSidebarOpen((o) => {
      try {
        localStorage.setItem(SIDEBAR_KEY, o ? "closed" : "open");
      } catch {
        // private window
      }
      return !o;
    });
  };
  const [sessionFilter, setSessionFilter] = useState("");
  const [researchProvider] = useState<string | null>(null);
  const [activeModelKey, setActiveModelKey] = useState("");
  const [modelUsage, setModelUsage] = useState<ModelUsageStat[]>([]);
  // Shared with Settings → Agents, so an edit there shows up in this picker.
  const profilesQuery = useProfiles();
  const profileInfos = profilesQuery.data ?? [];
  // Why the transcript is empty after clicking a session, when it is not simply
  // a new chat. Without this, "loaded and it had nothing" and "the load failed"
  // both render as the generic new-chat empty state.
  const [sessionNote, setSessionNote] = useState<string | null>(null);
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
      const res = await apiRequest(`/api/sessions?limit=50`);
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


  const loadModels = useCallback(async () => {
    try {
      const res = await apiRequest(`/api/models`);
      const data = (await res.json()) as {
        active_provider?: string;
        active_model?: string;
        chat_provider?: string;
        chat_model?: string;
      };
      // Prefer the chat model — that is what this panel actually talks to. It
      // diverges from the brain's active_* whenever CHAT_LLM_* is set. Fall back
      // to active_* for older backends that don't return chat_*.
      const provider = (data.chat_provider || data.active_provider || "").toLowerCase();
      const model = data.chat_model || data.active_model || "";
      if (provider && model) setActiveModelKey(`${provider}::${model}`);
    } catch {
      // ignore
    }
  }, []);

  const loadModelUsage = useCallback(async () => {
    try {
      // 24h window: "spend" only means something against a period. All-time
      // totals never move visibly and cannot answer "what is this costing me".
      const res = await apiRequest(`/api/models/usage?hours=24`);
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


  const switchSession = useCallback(async (sid: string) => {
    if (sid === sessionId && msgs.length > 0) return;

    // Save current session's full message list (preserves thinking/tool events)
    if (msgs.length > 0) {
      sessionCacheRef.current.set(sessionId, msgs);
    }

    setSessionId(sid);
    setMsgs([]);
    setSessionNote(null);

    // Restore from cache if available (full events preserved)
    const cached = sessionCacheRef.current.get(sid);
    if (cached) {
      setMsgs(cached);
      return;
    }

    // Otherwise fetch from server (text-only fallback).
    //
    // Render EVERY role, not just user/assistant. Agent-written sessions bank
    // their work under other roles — the daily news chain pushes eight
    // `role: observation` entries into `news-raw-<date>` — and filtering those
    // out left History advertising "8 msgs" next to a session that opened
    // completely blank, with nothing on screen to say why.
    try {
      const res = await apiRequest(`/api/sessions/${sid}/entries?limit=200`);
      const parsed = (await res.json()) as {
        entries?: Array<{ role: string; content: string }>;
      };
      const entries = parsed.entries ?? [];
      const restored: Msg[] = entries.map((e) => {
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
          // Labelled in the message meta so an agent observation is not
          // mistaken for something the assistant said to the user.
          role: e.role === "assistant" ? undefined : e.role,
        };
      });
      setMsgs(restored);
      if (restored.length === 0) {
        setSessionNote(`Session "${sid}" has no stored messages.`);
      }
    } catch {
      setMsgs([]);
      setSessionNote(`Could not load session "${sid}".`);
    }
  }, [sessionId, msgs]);

  const openedFromUrl = useRef<string | null>(null);
  // The Ask modal opening: the input takes the keyboard. Closing forgets
  // which conversation the URL opened, so opening the same thread again
  // (after moving to another one) switches back to it.
  useEffect(() => {
    if (visible) inputRef.current?.focus();
    else openedFromUrl.current = null;
  }, [visible]);

  // A conversation named in the URL (`ask=<id>`, `lib/ask.ts`) opens once, and
  // History reloads so a session a schedule just created is listed too.
  useEffect(() => {
    if (!visible || !session || openedFromUrl.current === session) return;
    openedFromUrl.current = session;
    loadSessions();
    switchSession(session);
    // An item's thread (`todo-<id>`) starts by naming the item, so the agent
    // knows what it is about; the text stays editable before it is sent.
    const item = session.match(/^todo-(.+)$/)?.[1];
    if (item) {
      apiRequest(`/todos/${encodeURIComponent(item)}`)
        .then((r) => (r.ok ? r.json() : null))
        .then((t: { title?: string } | null) => {
          if (t?.title) setInput((cur) => cur || `About my item “${t.title}”: `);
        })
        .catch(() => {});
    }
  }, [visible, session]); // eslint-disable-line react-hooks/exhaustive-deps

  const archiveSession = useCallback(async (sid: string, e: React.MouseEvent) => {
    e.stopPropagation();
    try {
      await apiRequest(`/api/sessions/${sid}/archive`, { method: "POST" });
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
    setSessionNote(null);
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
      contextProfile: contextProfile ?? undefined,
      synthesisProvider: researchProvider ?? undefined,
      model: turnModel || undefined,
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

  const q = sessionFilter.trim().toLowerCase();
  const shownSessions = sessions
    .map((s) => ({ s, ...sessionLabel(s) }))
    .filter(({ s, label }) => !q || label.toLowerCase().includes(q) || s.session_id.toLowerCase().includes(q) || (s.task_goal ?? "").toLowerCase().includes(q));

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
    const modelLabel =
      answeringModel(m.events) ?? (m.model ? m.model.split("::")[1] || m.model : "");

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
            {m.role && <span className="chat-msg-model">{m.role}</span>}
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
        {m.done && displayText && <ContextDock events={m.events} answer={displayText} />}
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
      <ChatHeader
        activeModelKey={activeModelKey}
        modelUsage={modelUsage}
        streaming={streaming}
        profile={contextProfile}
        defaultProfile={defaultProfile}
        profiles={profileInfos}
        onProfile={setContextProfile}
        onExport={exportChat}
        canExport={msgs.length > 0}
        sidebarOpen={sidebarOpen}
        onToggleSidebar={toggleSidebar}
        turnModel={turnModel}
        onTurnModel={setTurnModel}
      />

      <div className="chat-body">
        {/* ── Session sidebar ── */}
        {sidebarOpen && <div className="session-sidebar">
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
          <input className="ui-input session-search" value={sessionFilter} onChange={(e) => setSessionFilter(e.target.value)}
            placeholder={`search ${sessions.length} titles…`} aria-label="Search session titles" />

          <div className="session-list scroll">
            {sessions.length === 0 && !loadingSessions && (
              <div className="session-empty">No history yet</div>
            )}
            {sessionFilter.trim() && shownSessions.length === 0 && (
              <div className="session-empty">None of the {sessions.length} loaded titles matches “{sessionFilter.trim()}”</div>
            )}
            {shownSessions.map(({ s, label, datedInLabel }) => {
              return (
              <div
                key={s.session_id}
                className={`session-item${s.session_id === sessionId ? " active" : ""}`}
                onClick={() => switchSession(s.session_id)}
                title={`${label}\n${s.session_id}`}
              >
                <div className="session-item-title">
                  {truncate(label, 38)}
                </div>
                <div className="session-item-meta">
                  {!datedInLabel && formatDate(s.started_at)}
                  {s.msg_count > 0 && `${datedInLabel ? "" : " · "}${s.msg_count} msgs`}
                </div>
                <button
                  className="session-archive-btn"
                  onClick={(e) => archiveSession(s.session_id, e)}
                  title="Archive this chat (hidden from list, kept for training data)"
                >
                  🗄
                </button>
              </div>
              );
            })}
          </div>
        </div>}

        {/* ── Chat area ── */}
        <div className="chat-area" style={{ minWidth: 0 }}>
          <div className="chat-messages">
            {msgs.length === 0 && (
              <div className="empty-state" style={{ marginTop: 60 }}>
                <span className="icon">🤖</span>
                {sessionNote ? (
                  <span>{sessionNote}</span>
                ) : (
                  <span>Send a message to start a conversation</span>
                )}
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

      </div>
    </div>
  );
}
