/**
 * The five workspaces and the views inside them (HBI_REDESIGN_PLAN.md §2).
 *
 * Until each workspace is rebuilt, its views are the old panels, so nothing
 * becomes unreachable mid-migration. The old one-tab-per-panel URLs
 * (`#/tasks`, `#/graph`, …) redirect to their new home, keeping their params,
 * so links in notifications and notes still land.
 */
import type { Route } from "./router";

export type WorkspaceId = "home" | "chat" | "work" | "knowledge" | "system" | "settings";

export interface View {
  id: string;
  label: string;
}

export interface Workspace {
  id: WorkspaceId;
  icon: string;
  label: string;
  /** Words Ctrl-K matches besides the label. */
  keywords: string;
  views?: View[];
  /**
   * Params shared by all of this workspace's views, kept when switching
   * between them (Knowledge's search drives every view).
   */
  shared?: string[];
}

export const WORKSPACES: Workspace[] = [
  { id: "home", icon: "⌂", label: "Home", keywords: "dashboard overview status" },
  { id: "chat", icon: "🧠", label: "Chat", keywords: "conversation talk sessions" },
  {
    id: "work",
    icon: "📋",
    label: "Work",
    keywords: "tasks jobs queue todos scheduled schedules proposals inbox history running",
    views: [
      { id: "inbox", label: "Inbox" },
      { id: "active", label: "Active" },
      { id: "scheduled", label: "Scheduled" },
      { id: "todos", label: "Todos" },
      { id: "history", label: "History" },
    ],
  },
  {
    id: "knowledge",
    icon: "🔍",
    label: "Knowledge",
    keywords: "notes search graph memory entities semantic keyword timeline map topics domains",
    shared: ["q", "mode", "type", "hops", "since", "verified_since", "within"],
    views: [
      { id: "list", label: "Results" },
      { id: "map", label: "Map" },
      { id: "graph", label: "Neighbourhood" },
      { id: "timeline", label: "Timeline" },
    ],
  },
  {
    id: "system",
    icon: "🖥",
    label: "System",
    keywords: "logs architecture tools skills registry health",
    views: [
      { id: "logs", label: "Logs" },
      { id: "architecture", label: "Architecture" },
      { id: "tools", label: "Tools" },
    ],
  },
];

export const SETTINGS: Workspace = {
  id: "settings",
  icon: "⚙",
  label: "Settings",
  keywords: "preferences configuration",
};

/** Old tab ids → the workspace and view that replaced them. */
export const LEGACY_PATHS: Record<string, { path: WorkspaceId; view?: string }> = {
  tasks: { path: "work", view: "active" },
  todos: { path: "work", view: "todos" },
  "scheduled-tasks": { path: "work", view: "scheduled" },
  graph: { path: "knowledge", view: "graph" },
  tools: { path: "system", view: "tools" },
  logs: { path: "system", view: "logs" },
  architecture: { path: "system", view: "architecture" },
};

export const DEFAULT_WORKSPACE: WorkspaceId = "home";

/**
 * Views that were replaced inside a workspace (Phase 2 turned Work's four
 * panels into lanes; Phase 3 renamed Knowledge's "notes" to "list"). A filtered `jobs` link lands in History, an unfiltered
 * one in Active, so Phase-1 links keep working.
 */
function legacyView(path: string, params: Record<string, string>): string | null {
  if (path === "knowledge") return params.view === "notes" ? "list" : null;
  if (path !== "work") return null;
  switch (params.view) {
    case "jobs":
      if (params.job_status) return "history";
      if (params.task_status || params.tab === "tasks") return "history";
      return "active";
    case "proposals":
      return "inbox";
    default:
      return null;
  }
}

export function workspaceById(id: string): Workspace | undefined {
  return id === SETTINGS.id ? SETTINGS : WORKSPACES.find((w) => w.id === id);
}

export type Resolved =
  | { kind: "show"; workspace: Workspace; view: View | null }
  | { kind: "redirect"; path: string; params: Record<string, string | null> };

/**
 * What a route shows, or where it should go instead. A missing or unknown
 * view inside a workspace shows its first view without a redirect.
 */
export function resolveRoute(route: Route): Resolved {
  const legacy = LEGACY_PATHS[route.path];
  if (legacy) {
    return { kind: "redirect", path: legacy.path, params: legacy.view ? { view: legacy.view } : {} };
  }
  const workspace = workspaceById(route.path);
  if (!workspace) return { kind: "redirect", path: DEFAULT_WORKSPACE, params: {} };
  const renamed = legacyView(route.path, route.params);
  if (renamed) {
    const mode = route.params.job_status ? "jobs" : route.params.task_status || route.params.tab === "tasks" ? "tasks" : null;
    return { kind: "redirect", path: route.path, params: { view: renamed, tab: null, ...(mode ? { mode } : {}) } };
  }
  const views = workspace.views ?? [];
  const view = views.find((v) => v.id === route.params.view) ?? views[0] ?? null;
  return { kind: "show", workspace, view };
}
