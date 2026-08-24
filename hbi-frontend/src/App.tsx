import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import "./styles/main.css";
import type { AgentNotification } from "./api/notifications";
import { fetchUnreadNotifications, markNotificationRead } from "./api/notifications";
import { getMcpClient, onNotification } from "./api/mcp";

const ChatPanel          = lazy(() => import("./components/chat/ChatPanel"));
const TaskPanel          = lazy(() => import("./components/tasks/TaskPanel"));
const KnowledgePanel     = lazy(() => import("./components/knowledge/KnowledgePanel"));
const GraphPanel         = lazy(() => import("./components/graph/GraphPanel"));
const ToolPanel          = lazy(() => import("./components/tools/ToolPanel"));
const LogsPanel          = lazy(() => import("./components/logs/LogsPanel"));
const ArchitecturePanel  = lazy(() => import("./components/architecture/ArchitecturePanel"));
const TodoPanel               = lazy(() => import("./components/todo/TodoPanel"));
const ScheduledTasksPanel     = lazy(() => import("./components/scheduled-tasks/ScheduledTasksPanel"));
const SettingsModal           = lazy(() => import("./components/settings/SettingsModal"));

type Tab = "chat" | "tasks" | "todos" | "scheduled-tasks" | "knowledge" | "graph" | "tools" | "logs" | "architecture";

const TABS: { id: Tab; icon: string; label: string }[] = [
  { id: "chat",            icon: "🧠", label: "Chat" },
  { id: "tasks",           icon: "📋", label: "Tasks" },
  { id: "todos",           icon: "✅", label: "Todos" },
  { id: "scheduled-tasks", icon: "📅", label: "Scheduled" },
  { id: "knowledge",       icon: "🔍", label: "Knowledge" },
  { id: "graph",           icon: "🕸", label: "Graph" },
  { id: "tools",           icon: "🔧", label: "Tools" },
  { id: "logs",            icon: "📊", label: "Logs" },
  { id: "architecture",    icon: "🏗", label: "Architecture" },
];

function Fallback() {
  return <div className="loading" style={{ padding: 24 }}>Loading…</div>;
}

export default function App() {
  const [tab, setTab] = useState<Tab>("chat");
  const [showSettings, setShowSettings] = useState(false);
  // Single source of truth for agent notifications: the nav badge counts this
  // list and ChatPanel renders it, so the badge can never advertise a message
  // the chat panel does not display.
  const [notifications, setNotifications] = useState<AgentNotification[]>([]);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  // Ids dismissed locally but whose POST /read may not have landed yet — the
  // poll must not resurrect them.
  const dismissedRef = useRef<Set<string>>(new Set());

  const refreshNotifications = useCallback(async () => {
    try {
      const items = await fetchUnreadNotifications();
      setNotifications(items.filter((n) => !dismissedRef.current.has(n.id)));
    } catch {
      // brain not reachable — keep whatever we last saw
    }
  }, []);

  // Push: the brain broadcasts `notifications/agent_chat` to every MCP session
  // when a chain calls `notify_user`. Subscribing makes delivery instant.
  // `getMcpClient()` is what actually opens the stream — `onNotification` only
  // registers a handler, and nothing else here would connect the client.
  useEffect(() => {
    const unsub = onNotification((n) => {
      if (n.method === "notifications/agent_chat") refreshNotifications();
    });
    getMcpClient().catch(() => {
      // brain unreachable — the poll below still covers us
    });
    return unsub;
  }, [refreshNotifications]);

  // Poll every 30 s as the fallback. The push stream does not reconnect on its
  // own, so this is what keeps notifications arriving after it drops — and it
  // is the only path when the client failed to connect at all.
  useEffect(() => {
    refreshNotifications();
    pollRef.current = setInterval(refreshNotifications, 30_000);
    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
    };
  }, [refreshNotifications]);

  const dismissNotification = useCallback(async (id: string) => {
    dismissedRef.current.add(id);
    setNotifications((prev) => prev.filter((n) => n.id !== id));
    try {
      await markNotificationRead(id);
    } catch {
      // ignore — it stays unread server-side and returns on a later poll
    } finally {
      dismissedRef.current.delete(id);
    }
  }, []);

  const handleTabClick = useCallback((id: Tab) => {
    setTab(id);
  }, []);

  return (
    <div className="app">
      <nav className="sidebar">
        <div className="sidebar-title">Agent Brain</div>
        {TABS.map((t) => (
          <button
            key={t.id}
            className={`sidebar-btn${tab === t.id ? " active" : ""}`}
            onClick={() => handleTabClick(t.id)}
          >
            <span className="icon">{t.icon}</span>
            {t.label}
            {t.id === "chat" && notifications.length > 0 && (
              <span className="notif-badge">{notifications.length}</span>
            )}
          </button>
        ))}
        <div style={{ marginTop: "auto" }}>
          <button
            className="sidebar-btn"
            onClick={() => setShowSettings(true)}
            title="Settings"
          >
            <span className="icon">⚙</span>
            Settings
          </button>
        </div>
      </nav>

      <main className="main-content">
        {/* ChatPanel stays mounted so conversation history survives tab switches. */}
        <div style={tab === "chat"
          ? { flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }
          : { display: "none" }}>
          <Suspense fallback={<Fallback />}>
            <ChatPanel
              notifications={notifications}
              onDismissNotification={dismissNotification}
              visible={tab === "chat"}
            />
          </Suspense>
        </div>
        <Suspense fallback={<Fallback />}>
          {tab === "tasks"           && <TaskPanel />}
          {tab === "todos"           && <TodoPanel />}
          {tab === "scheduled-tasks" && <ScheduledTasksPanel />}
          {tab === "knowledge"       && <KnowledgePanel />}
          {tab === "graph"           && <GraphPanel />}
          {tab === "tools"           && <ToolPanel />}
          {tab === "logs"            && <LogsPanel />}
          {tab === "architecture"    && <ArchitecturePanel />}
        </Suspense>
      </main>

      {showSettings && (
        <Suspense fallback={null}>
          <SettingsModal onClose={() => setShowSettings(false)} />
        </Suspense>
      )}
    </div>
  );
}
