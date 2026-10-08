import { lazy, Suspense, useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import "./styles/main.css";
import "./styles/ui.css";
import type { AgentNotification } from "./api/notifications";
import { fetchUnreadNotifications, markNotificationRead } from "./api/notifications";
import { getMcpClient, onNotification } from "./api/mcp";
import { invalidateOnJobPush } from "./api/queries";
import { navigate, useRoute } from "./router";
import { goTo } from "./nav";
import { SETTINGS, WORKSPACES, resolveRoute, type Workspace } from "./workspaces";
import EntityDrawer from "./components/drawer/EntityDrawer";
import { Tabs } from "./components/ui";

const HomePanel          = lazy(() => import("./components/home/HomePanel"));
const InboxLane          = lazy(() => import("./components/work/InboxLane"));
const ActiveLane         = lazy(() => import("./components/work/ActiveLane"));
const ScheduledLane      = lazy(() => import("./components/work/ScheduledLane"));
const TodosLane          = lazy(() => import("./components/work/TodosLane"));
const HistoryLane        = lazy(() => import("./components/work/HistoryLane"));
const ChatPanel          = lazy(() => import("./components/chat/ChatPanel"));
const KnowledgePanel     = lazy(() => import("./components/knowledge/KnowledgePanel"));
const GraphPanel         = lazy(() => import("./components/graph/GraphPanel"));
const ToolPanel          = lazy(() => import("./components/tools/ToolPanel"));
const LogsPanel          = lazy(() => import("./components/logs/LogsPanel"));
const ArchitecturePanel  = lazy(() => import("./components/architecture/ArchitecturePanel"));
const SettingsPage       = lazy(() => import("./components/settings/SettingsPage"));
const CommandPalette     = lazy(() => import("./components/palette/CommandPalette"));

function Fallback() {
  return <div className="loading" style={{ padding: 24 }}>Loading…</div>;
}

/** A workspace with views: its name, the view tabs, then the view itself. */
function WorkspaceFrame({ workspace, view, children }: { workspace: Workspace; view: string; children: ReactNode }) {
  return (
    <div className="workspace">
      <div className="workspace-bar">
        <span className="workspace-title">{workspace.icon} {workspace.label}</span>
        <Tabs
          tabs={(workspace.views ?? []).map((v) => ({ id: v.id, label: v.label }))}
          active={view}
          onChange={(id) => goTo(workspace.id, { view: id })}
        />
      </div>
      <div className="workspace-body">{children}</div>
    </div>
  );
}

export default function App() {
  // The URL owns navigation: `#/<workspace>?view=<view>&<filters>&note=<id>`.
  // Old one-tab URLs (#/tasks, #/graph, …) redirect to their new home.
  const route = useRoute();
  const resolved = resolveRoute(route);
  useEffect(() => {
    if (resolved.kind === "redirect") navigate({ path: resolved.path, params: resolved.params }, { replace: true });
  }, [resolved]);
  const workspace = resolved.kind === "show" ? resolved.workspace : null;
  const view = resolved.kind === "show" ? resolved.view?.id ?? "" : "";
  const p = route.params;

  const [paletteOpen, setPaletteOpen] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

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

  // One subscription keeps every shared status query fresh on job pushes.
  useEffect(() => invalidateOnJobPush(), []);

  const ws = workspace?.id;
  // Drill-down filters key their panel, so a new drill remounts it cleanly.
  const historyDrill = { mode: p.mode, taskStatus: p.task_status, jobStatus: p.job_status, since: p.since };
  // The entity the drawer has open, so a lane can highlight its row.
  const openId = p.task ?? p.job ?? p.schedule ?? p.todo ?? p.proposal;
  const knowledgeDrill = { since: p.since, type: p.type, verifiedSince: p.verified_since, q: p.q };

  return (
    <div className="app">
      <nav className="sidebar">
        <div className="sidebar-title">Agent Brain</div>
        {WORKSPACES.map((w) => (
          <button
            key={w.id}
            className={`sidebar-btn${ws === w.id ? " active" : ""}`}
            onClick={() => goTo(w.id)}
          >
            <span className="icon">{w.icon}</span>
            {w.label}
            {w.id === "chat" && notifications.length > 0 && (
              <span className="notif-badge">{notifications.length}</span>
            )}
          </button>
        ))}
        <div style={{ marginTop: "auto" }}>
          <div className="sidebar-hint">Ctrl-K to jump anywhere</div>
          <button
            className={`sidebar-btn${ws === SETTINGS.id ? " active" : ""}`}
            onClick={() => goTo(SETTINGS.id)}
          >
            <span className="icon">{SETTINGS.icon}</span>
            {SETTINGS.label}
          </button>
        </div>
      </nav>

      <main className="main-content">
        {/* ChatPanel stays mounted so conversation history survives navigation. */}
        <div style={ws === "chat"
          ? { flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }
          : { display: "none" }}>
          <Suspense fallback={<Fallback />}>
            <ChatPanel
              notifications={notifications}
              onDismissNotification={dismissNotification}
              visible={ws === "chat"}
            />
          </Suspense>
        </div>
        <Suspense fallback={<Fallback />}>
          {ws === "home" && <HomePanel notifications={notifications} />}
          {ws === "work" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              {view === "inbox"     && <InboxLane openId={openId} />}
              {view === "active"    && <ActiveLane openId={openId} />}
              {view === "scheduled" && <ScheduledLane mode={p.mode} openId={openId} />}
              {view === "todos"     && <TodosLane openId={openId} />}
              {view === "history"   && <HistoryLane key={JSON.stringify(historyDrill)} drill={historyDrill} openId={openId} />}
            </WorkspaceFrame>
          )}
          {ws === "knowledge" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              {view === "notes" && <KnowledgePanel key={JSON.stringify(knowledgeDrill)} drill={knowledgeDrill} />}
              {view === "graph" && <GraphPanel />}
            </WorkspaceFrame>
          )}
          {ws === "system" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              {view === "logs"         && <LogsPanel />}
              {view === "architecture" && <ArchitecturePanel />}
              {view === "tools"        && <ToolPanel />}
            </WorkspaceFrame>
          )}
          {ws === "settings" && <SettingsPage section={p.section} />}
        </Suspense>
      </main>

      <EntityDrawer />

      {paletteOpen && (
        <Suspense fallback={null}>
          <CommandPalette onClose={() => setPaletteOpen(false)} />
        </Suspense>
      )}
    </div>
  );
}
