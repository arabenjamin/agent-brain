import { lazy, Suspense, useEffect, useState, type ReactNode } from "react";
import "./styles/main.css";
import "./styles/ui.css";
import { refreshFeed, useFeed } from "./api/feed";
import { getMcpClient, onNotification } from "./api/mcp";
import { invalidateOnJobPush } from "./api/queries";
import { currentRoute, navigate, useRoute } from "./router";
import { goTo } from "./nav";
import { SETTINGS, WORKSPACES, resolveRoute, type Workspace } from "./workspaces";
import EntityDrawer from "./components/drawer/EntityDrawer";
import { Tabs } from "./components/ui";

const HomePanel          = lazy(() => import("./components/home/HomePanel"));
const FeedLane           = lazy(() => import("./components/work/FeedLane"));
const ActiveLane         = lazy(() => import("./components/work/ActiveLane"));
const ScheduledLane      = lazy(() => import("./components/work/ScheduledLane"));
const TodosLane          = lazy(() => import("./components/work/TodosLane"));
const HistoryLane        = lazy(() => import("./components/work/HistoryLane"));
const ChatPanel          = lazy(() => import("./components/chat/ChatPanel"));
const KnowledgeWorkspace = lazy(() => import("./components/knowledge/KnowledgeWorkspace"));
const HealthView         = lazy(() => import("./components/system/HealthView"));
const LogsView           = lazy(() => import("./components/system/LogsView"));
const RegistryView       = lazy(() => import("./components/system/RegistryView"));
const ArchitectureView   = lazy(() => import("./components/system/ArchitectureView"));
const SettingsPage       = lazy(() => import("./components/settings/SettingsPage"));
const CommandPalette     = lazy(() => import("./components/palette/CommandPalette"));

const NAV_KEY = "hbi.nav.collapsed";

/** The nav rail's collapsed state, remembered per browser (storage may be blocked). */
function useNavCollapsed(): [boolean, () => void] {
  const [collapsed, setCollapsed] = useState(() => {
    try { return localStorage.getItem(NAV_KEY) === "1"; } catch { return false; }
  });
  const toggle = () => setCollapsed((c) => {
    try { localStorage.setItem(NAV_KEY, c ? "0" : "1"); } catch { /* not remembered */ }
    return !c;
  });
  return [collapsed, toggle];
}

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
          onChange={(id) => {
            const keep = Object.fromEntries((workspace.shared ?? []).map((k) => [k, currentRoute().params[k]]));
            goTo(workspace.id, { ...keep, view: id });
          }}
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
  const [navCollapsed, toggleNav] = useNavCollapsed();
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

  // What wants the user is one list, the Feed (`/api/feed`). The nav badge,
  // Home's Needs-you tile and the Feed header all show its `total`, so they
  // cannot disagree. The chat banner that used to show notifications is gone:
  // a notification is a Feed row, and opening it opens its conversation.
  const feed = useFeed();
  const feedTotal = feed.data?.total ?? 0;

  // Push: the brain broadcasts `notifications/agent_chat` when a chain calls
  // `notify_user`, so the Feed refreshes at once rather than on its next poll.
  // `getMcpClient()` is what actually opens the stream.
  useEffect(() => {
    const unsub = onNotification((n) => {
      if (n.method === "notifications/agent_chat") refreshFeed();
    });
    getMcpClient().catch(() => {
      // brain unreachable — the Feed's poll still covers us
    });
    return unsub;
  }, []);

  // One subscription keeps every shared status query fresh on job pushes.
  useEffect(() => invalidateOnJobPush(), []);

  const ws = workspace?.id;
  // Drill-down filters key their panel, so a new drill remounts it cleanly.
  const historyDrill = { mode: p.mode, taskStatus: p.task_status, jobStatus: p.job_status, since: p.since };
  // The entity the drawer has open, so a lane can highlight its row.
  const openId = p.task ?? p.job ?? p.schedule ?? p.todo ?? p.proposal;
  const knowledgeParams = {
    q: p.q, mode: p.mode, type: p.type, hops: p.hops,
    since: p.since, verified_since: p.verified_since, within: p.within,
    note: p.note, entity: p.entity, topic: p.topic,
    focus: p.focus, scope: p.scope,
  };

  return (
    <div className="app">
      <nav className={`sidebar${navCollapsed ? " collapsed" : ""}`}>
        <div className="sidebar-title">
          <span className="sidebar-label">Agent Brain</span>
          <button type="button" className="sidebar-toggle" onClick={toggleNav}
            title={navCollapsed ? "Expand the sidebar" : "Collapse the sidebar"} aria-expanded={!navCollapsed}>
            {navCollapsed ? "»" : "«"}
          </button>
        </div>
        {WORKSPACES.map((w) => (
          <button
            key={w.id}
            className={`sidebar-btn${ws === w.id ? " active" : ""}`}
            onClick={() => goTo(w.id)}
            title={navCollapsed ? w.label : undefined}
          >
            <span className="icon">{w.icon}</span>
            <span className="sidebar-label">{w.label}</span>
            {w.id === "work" && feedTotal > 0 && (
              <span className="notif-badge" title={`${feedTotal} in the Feed`}>{feedTotal}</span>
            )}
          </button>
        ))}
        <div style={{ marginTop: "auto" }}>
          <div className="sidebar-hint sidebar-label">Ctrl-K to jump anywhere</div>
          <button
            className={`sidebar-btn${ws === SETTINGS.id ? " active" : ""}`}
            onClick={() => goTo(SETTINGS.id)}
            title={navCollapsed ? SETTINGS.label : undefined}
          >
            <span className="icon">{SETTINGS.icon}</span>
            <span className="sidebar-label">{SETTINGS.label}</span>
          </button>
        </div>
      </nav>

      <main className="main-content">
        {/* ChatPanel stays mounted so conversation history survives navigation. */}
        <div style={ws === "chat"
          ? { flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }
          : { display: "none" }}>
          <Suspense fallback={<Fallback />}>
            <ChatPanel visible={ws === "chat"} session={ws === "chat" ? p.session : undefined} />
          </Suspense>
        </div>
        <Suspense fallback={<Fallback />}>
          {ws === "home" && <HomePanel />}
          {ws === "work" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              {view === "feed"      && <FeedLane openId={openId} mode={p.mode} />}
              {view === "active"    && <ActiveLane openId={openId} />}
              {view === "scheduled" && <ScheduledLane mode={p.mode} openId={openId} />}
              {view === "todos"     && <TodosLane openId={openId} />}
              {view === "history"   && <HistoryLane key={JSON.stringify(historyDrill)} drill={historyDrill} openId={openId} />}
            </WorkspaceFrame>
          )}
          {ws === "knowledge" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              <KnowledgeWorkspace view={view} params={knowledgeParams} />
            </WorkspaceFrame>
          )}
          {ws === "system" && workspace && (
            <WorkspaceFrame workspace={workspace} view={view}>
              {view === "health"       && <HealthView />}
              {view === "logs"         && <LogsView />}
              {view === "registry"     && <RegistryView />}
              {view === "architecture" && <ArchitectureView />}
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
