import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import "./styles/main.css";
import { getBrainUrl, getApiKey } from "./api/config";

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
  const [notifCount, setNotifCount] = useState(0);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchNotifCount = useCallback(async () => {
    try {
      const res = await fetch(`${getBrainUrl()}/api/notifications?unread=true`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      if (res.ok) {
        const data = await res.json() as { notifications?: unknown[] };
        setNotifCount(data.notifications?.length ?? 0);
      }
    } catch {
      // brain not reachable — ignore
    }
  }, []);

  // Poll for unread notifications every 30 s.
  useEffect(() => {
    fetchNotifCount();
    pollRef.current = setInterval(fetchNotifCount, 30_000);
    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
    };
  }, [fetchNotifCount]);

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
            {t.id === "chat" && notifCount > 0 && (
              <span className="notif-badge">{notifCount}</span>
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
            <ChatPanel onNotifCountChange={setNotifCount} visible={tab === "chat"} />
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
