import { useCallback, useEffect, useState, type ReactElement } from "react";
import { callTool } from "../../api/mcp";
import { apiRequest, query } from "../../api/client";
import { useQueueStatus, type QueueStatus } from "../../api/queries";
import { formatLocal } from "../../lib/time";
import { goTo } from "../../nav";
import { DrillBanner } from "../ui";

// ── Types ─────────────────────────────────────────────────────────────────────

interface Task {
  id: string;
  goal: string;
  status: string;
  parent_id?: string;
  created_at?: string;
}

interface Job {
  id: string;
  tool_name: string;
  status: string;
  priority: number;
  attempt_count: number;
  max_attempts: number;
  provider_hint?: string;
  args?: Record<string, unknown>;
  error?: string;
  parent_job_id?: string;
  created_at: string;
  updated_at: string;
}

type Tab = "queue" | "tasks";
type JobFilter = "active" | "all" | "completed" | "failed";
type TaskFilter = "all" | "created" | "in_progress" | "completed" | "failed" | "blocked";

// ── Helpers ───────────────────────────────────────────────────────────────────

const STATUS_COLORS: Record<string, string> = {
  running: "var(--yellow)",
  queued: "var(--blue)",
  parked: "var(--text-dim)",
  completed: "var(--green)",
  failed: "var(--red)",
  dead: "var(--red)",
  cancelled: "var(--text-muted)",
  in_progress: "var(--yellow)",
  created: "var(--blue)",
  blocked: "var(--text-muted)",
};

const PRIORITY_LABELS: Record<number, string> = { 0: "low", 1: "normal", 2: "high", 3: "crit" };

function fmtTime(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  } catch {
    return iso;
  }
}

function fmtAge(iso: string): string {
  const secs = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h`;
}

function argSummary(args?: Record<string, unknown>): string {
  if (!args) return "";
  const entries = Object.entries(args);
  if (entries.length === 0) return "{}";
  const parts = entries.slice(0, 2).map(([k, v]) => {
    const s = typeof v === "string" ? v : JSON.stringify(v);
    return `${k}: ${s.slice(0, 40)}${s.length > 40 ? "…" : ""}`;
  });
  return parts.join(", ") + (entries.length > 2 ? `, +${entries.length - 2}` : "");
}

// ── Component ─────────────────────────────────────────────────────────────────

/** A drill-down from Home: a server-side filtered subset, shown with a banner. */
export interface TaskDrill {
  tab?: string;
  /** Comma-separated task statuses. */
  taskStatus?: string;
  /** Comma-separated job statuses. */
  jobStatus?: string;
  /** UTC RFC 3339; tasks created / jobs updated since. */
  since?: string;
}

/** Server-side status lists for the job filter buttons. */
const JOB_FILTER_STATUSES: Record<JobFilter, string | undefined> = {
  active: "running,queued,parked",
  all: undefined,
  completed: "completed",
  // A job that will not run again is stored as failed, dead or dead_letter.
  failed: "failed,dead,dead_letter",
};

const DRILL_LIMIT = 500;

export default function TaskPanel({ drill }: { drill?: TaskDrill }) {
  const [tab, setTab] = useState<Tab>(drill?.tab === "tasks" || drill?.taskStatus ? "tasks" : "queue");
  // Primitives, so the poll callbacks stay stable across renders.
  const jobStatus = drill?.jobStatus;
  const taskStatus = drill?.taskStatus;
  const since = drill?.since;

  // Queue tab state
  const [jobs, setJobs] = useState<Job[]>([]);
  // Shared with every other view that shows queue counts (one request per tick).
  const queueQuery = useQueueStatus(tab === "queue" ? 2_000 : 5_000);
  const queue: QueueStatus | null = queueQuery.data ?? null;
  const [jobFilter, setJobFilter] = useState<JobFilter>("active");
  const [expandedJob, setExpandedJob] = useState<string | null>(null);

  // Tasks tab state
  const [tasks, setTasks] = useState<Task[]>([]);
  const [taskFilter, setTaskFilter] = useState<TaskFilter>("all");

  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);

  // ── Data fetching ──────────────────────────────────────────────────────────

  const refreshQueue = useCallback(async () => {
    try {
      // Server-side status lists: filtering a "latest N" page in the browser
      // hid older parked jobs, and "failed" alone never matched dead jobs.
      const path = jobStatus
        ? `/api/jobs${query({ status: jobStatus, since, limit: DRILL_LIMIT })}`
        : `/api/jobs${query({ status: JOB_FILTER_STATUSES[jobFilter], limit: jobFilter === "all" ? 100 : 50 })}`;
      const jobsRes = await apiRequest(path).then((r) => r.json());
      const allJobs: Job[] = jobsRes.jobs ?? [];

      setJobs(allJobs);
      setLastRefresh(new Date());
    } catch (e) {
      setError(String(e));
    }
  }, [jobFilter, jobStatus, since]);

  const refreshTasks = useCallback(async () => {
    try {
      const res = await apiRequest(taskStatus
        ? `/api/tasks${query({ status: taskStatus, since, limit: DRILL_LIMIT })}`
        : "/api/tasks?limit=100");
      const data = await res.json();
      setTasks(data.tasks ?? []);
      setLastRefresh(new Date());
    } catch (e) {
      setError(String(e));
    }
  }, [taskStatus, since]);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      if (tab === "queue") await refreshQueue();
      else await refreshTasks();
    } finally {
      setLoading(false);
    }
  }, [tab, refreshQueue, refreshTasks]);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, tab === "queue" ? 2000 : 5000);
    return () => clearInterval(id);
  }, [refresh, tab]);

  // ── Actions ────────────────────────────────────────────────────────────────

  const cancelJob = async (id: string) => {
    await callTool("manage_job", { action: "cancel", job_id: id });
    await Promise.all([refreshQueue(), queueQuery.refetch()]);
  };

  const retryJob = async (id: string) => {
    await callTool("manage_job", { action: "retry", job_id: id });
    await Promise.all([refreshQueue(), queueQuery.refetch()]);
  };

  // ── Queue tab rendering ────────────────────────────────────────────────────

  const jobFilterBtns: { key: JobFilter; label: string }[] = [
    { key: "active", label: "Active" },
    { key: "all", label: "All" },
    { key: "completed", label: "Completed" },
    { key: "failed", label: "Failed / Dead" },
  ];

  const renderJob = (job: Job) => {
    const isExpanded = expandedJob === job.id;
    const color = STATUS_COLORS[job.status] ?? "var(--text-muted)";
    const isActive = job.status === "running" || job.status === "queued";

    return (
      <div
        key={job.id}
        className="job-card"
        style={{ borderLeft: `3px solid ${color}` }}
        onClick={() => setExpandedJob(isExpanded ? null : job.id)}
      >
        <div className="job-card-header">
          <span className="job-tool">{job.tool_name}</span>
          <span className="job-badges">
            <span className="job-badge" style={{ color }}>
              {job.status === "running" && <span className="spinner-inline" />}
              {job.status}
            </span>
            {job.priority > 1 && (
              <span className="job-badge" style={{ color: "var(--yellow)" }}>
                {PRIORITY_LABELS[job.priority]}
              </span>
            )}
            {job.provider_hint && (
              <span className="job-badge" style={{ color: "var(--text-muted)" }}>
                {job.provider_hint}
              </span>
            )}
            <span className="job-age">{fmtAge(job.created_at)}</span>
          </span>
        </div>

        {job.args && !isExpanded && (
          <div className="job-args-preview">{argSummary(job.args)}</div>
        )}

        {isExpanded && (
          <div className="job-details">
            <div className="job-detail-row">
              <span className="detail-label">ID</span>
              <span className="detail-value mono">{job.id}</span>
            </div>
            {job.parent_job_id && (
              <div className="job-detail-row">
                <span className="detail-label">Parent</span>
                <span className="detail-value mono">{job.parent_job_id.slice(0, 8)}…</span>
              </div>
            )}
            <div className="job-detail-row">
              <span className="detail-label">Attempts</span>
              <span className="detail-value">{job.attempt_count} / {job.max_attempts}</span>
            </div>
            <div className="job-detail-row">
              <span className="detail-label">Created</span>
              <span className="detail-value">{fmtTime(job.created_at)}</span>
            </div>
            {job.args && (
              <div className="job-detail-row" style={{ alignItems: "flex-start" }}>
                <span className="detail-label">Args</span>
                <pre className="detail-pre">{JSON.stringify(job.args, null, 2)}</pre>
              </div>
            )}
            {job.error && (
              <div className="job-detail-row" style={{ alignItems: "flex-start" }}>
                <span className="detail-label" style={{ color: "var(--red)" }}>Error</span>
                <span className="detail-value" style={{ color: "var(--red)", whiteSpace: "pre-wrap" }}>
                  {job.error}
                </span>
              </div>
            )}
            <div className="job-actions">
              {isActive && (
                <button className="job-btn cancel" onClick={e => { e.stopPropagation(); cancelJob(job.id); }}>
                  Cancel
                </button>
              )}
              {(job.status === "failed" || job.status === "dead" || job.status === "cancelled") && (
                <button className="job-btn retry" onClick={e => { e.stopPropagation(); retryJob(job.id); }}>
                  Retry
                </button>
              )}
            </div>
          </div>
        )}
      </div>
    );
  };

  const renderQueueTab = () => (
    <>
      {/* Stats bar */}
      {queue && (
        <div className="queue-stats-bar">
          <div className="qstat">
            <span className="qstat-val" style={{ color: queue.running_now > 0 ? "var(--yellow)" : "inherit" }}>
              {queue.running_now > 0 && <span className="spinner-inline" />}
              {queue.running_now}
            </span>
            <span className="qstat-label">running</span>
          </div>
          <div className="qstat">
            <span className="qstat-val">{queue.in_memory_pending}</span>
            <span className="qstat-label">pending</span>
          </div>
          {queue.by_status && Object.entries(queue.by_status)
            .filter(([k]) => !["completed", "cancelled"].includes(k))
            .map(([k, v]) => (
              <div key={k} className="qstat">
                <span className="qstat-val" style={{ color: STATUS_COLORS[k] ?? "inherit" }}>{v}</span>
                <span className="qstat-label">{k}</span>
              </div>
            ))}
          <div className="qstat" style={{ marginLeft: "auto" }}>
            <span className={`dot ${queue.enabled ? "green" : "red"}`} />
            <span className="qstat-label">{queue.enabled ? "running" : "paused"}</span>
          </div>
        </div>
      )}

      {/* Filter (a drill-down replaces it until cleared) */}
      {!jobStatus && <div className="filter-row">
        {jobFilterBtns.map(({ key, label }) => (
          <button
            key={key}
            className={`filter-btn${jobFilter === key ? " active" : ""}`}
            onClick={() => setJobFilter(key)}
          >
            {label}
            {key === "active" && queue && queue.running_now > 0 && (
              <span className="filter-badge">{queue.running_now}</span>
            )}
          </button>
        ))}
      </div>}

      {/* Job list */}
      <div className="jobs-list">
        {jobs.length === 0 && !loading && (
          <div className="empty-state">
            <span className="icon">✓</span>
            <span>No {jobFilter} jobs</span>
          </div>
        )}
        {jobs.map(renderJob)}
      </div>
    </>
  );

  // ── Tasks tab rendering ────────────────────────────────────────────────────

  const taskFilterBtns: TaskFilter[] = ["all", "created", "in_progress", "completed", "failed", "blocked"];

  const buildTree = () => {
    const filtered = tasks.filter(t => taskFilter === "all" || t.status === taskFilter);
    const roots = filtered.filter(t => !t.parent_id || !tasks.find(pt => pt.id === t.parent_id));
    const childrenMap = new Map<string, Task[]>();
    tasks.forEach(t => {
      if (t.parent_id) {
        const list = childrenMap.get(t.parent_id) ?? [];
        list.push(t);
        childrenMap.set(t.parent_id, list);
      }
    });
    return { roots, childrenMap };
  };

  const { roots, childrenMap } = buildTree();

  const renderTask = (task: Task, depth = 0): ReactElement => {
    const children = childrenMap.get(task.id) ?? [];
    const color = STATUS_COLORS[task.status] ?? "var(--text-muted)";
    return (
      <div key={task.id} style={{ marginLeft: depth * 16 }}>
        <div className="task-card" style={{ borderLeft: `3px solid ${color}` }}>
          <div className="task-card-header">
            <span className="task-goal" title={task.goal}>
              {depth > 0 ? "↳ " : ""}{task.goal}
            </span>
            <span className="task-status-badge" style={{ color }}>
              {task.status.replace("_", " ")}
            </span>
          </div>
          <div className="task-meta">
            {task.id.slice(0, 8)}…
            {task.created_at && <> · {new Date(task.created_at).toLocaleString()}</>}
          </div>
        </div>
        {children.map(child => renderTask(child, depth + 1))}
      </div>
    );
  };

  const renderTasksTab = () => (
    <>
      <div className="filter-row">
        {taskFilterBtns.map(s => (
          <button
            key={s}
            className={`filter-btn${taskFilter === s ? " active" : ""}`}
            onClick={() => setTaskFilter(s)}
          >
            {s.replace("_", " ")}
            {" "}({s === "all" ? tasks.length : tasks.filter(t => t.status === s).length})
          </button>
        ))}
      </div>
      <div className="tasks-list">
        {roots.length === 0 && !loading && (
          <div className="empty-state">
            <span className="icon">✓</span>
            <span>No tasks matching "{taskFilter}"</span>
          </div>
        )}
        {roots.map(t => renderTask(t))}
      </div>
    </>
  );

  // ── Main render ────────────────────────────────────────────────────────────

  return (
    <div className="panel">
      <div className="panel-header">
        {tab === "queue" ? "⚙ Queue" : "📋 Tasks"}
        {loading && <span style={{ color: "var(--text-muted)", marginLeft: 8, fontSize: 11 }}>…</span>}
        {lastRefresh && (
          <span style={{ color: "var(--text-muted)", fontSize: 10, marginLeft: 4 }}>
            {lastRefresh.toLocaleTimeString()}
          </span>
        )}
        <button className="refresh-btn" onClick={refresh} title="Refresh">↻</button>
      </div>

      {(tab === "queue" ? jobStatus : taskStatus) && (
        <DrillBanner
          rows={tab === "queue" ? jobs.length : tasks.length}
          cap={DRILL_LIMIT}
          onClear={() => goTo("work", { view: "jobs" })}
        >
          {tab === "queue"
            ? <>jobs <strong>{jobStatus?.replaceAll(",", " / ")}</strong>{since && <> updated since {formatLocal(since)}</>}</>
            : <>tasks <strong>{taskStatus?.replaceAll(",", " / ")}</strong>{since && <> created since {formatLocal(since)}</>}</>}
        </DrillBanner>
      )}

      {/* Tab switcher */}
      <div className="tab-row">
        <button
          className={`tab-btn${tab === "queue" ? " active" : ""}`}
          onClick={() => setTab("queue")}
        >
          Queue
          {queue && queue.running_now > 0 && (
            <span className="tab-badge running">{queue.running_now}</span>
          )}
        </button>
        <button
          className={`tab-btn${tab === "tasks" ? " active" : ""}`}
          onClick={() => setTab("tasks")}
        >
          Tasks
          {tasks.filter(t => t.status === "in_progress").length > 0 && (
            <span className="tab-badge">{tasks.filter(t => t.status === "in_progress").length}</span>
          )}
        </button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      {tab === "queue" ? renderQueueTab() : renderTasksTab()}
    </div>
  );
}
