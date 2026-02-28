import { useCallback, useEffect, useState } from "react";
import { callTool } from "../../api/mcp";

// ── Types ─────────────────────────────────────────────────────────────────────

interface QueueStatus {
  in_memory_pending: number;
  running_now: number;
  max_concurrent: number;
  enabled: boolean;
  by_status: Record<string, number>;
}

interface SchedulerStatus {
  config: {
    interval_secs: number;
    enabled: boolean;
    max_tasks_per_run: number;
    error_budget: number;
  };
  state: {
    tasks_dispatched: number;
    consecutive_errors: number;
    last_run_at: string | null;
    last_error: string | null;
    is_running: boolean;
  };
}

interface Task {
  id: string;
  goal: string;
  status: string;
  created_at: string;
}

// ── Colour helpers ────────────────────────────────────────────────────────────

const TASK_STATUS_COLORS: Record<string, string> = {
  created:     "#4f8ef7",
  in_progress: "#fbbf24",
  completed:   "#4ade80",
  failed:      "#f87171",
  blocked:     "#a78bfa",
};

const JOB_STATUS_COLORS: Record<string, string> = {
  queued:    "#4f8ef7",
  running:   "#22d3ee",
  completed: "#4ade80",
  failed:    "#f87171",
  dead:      "#f87171",
  parked:    "#a78bfa",
  cancelled: "#7a8099",
};

// ── Main component ────────────────────────────────────────────────────────────

export default function LogsPanel() {
  const [queueStatus,     setQueueStatus]     = useState<QueueStatus | null>(null);
  const [schedulerStatus, setSchedulerStatus] = useState<SchedulerStatus | null>(null);
  const [recentTasks,     setRecentTasks]     = useState<Task[]>([]);
  const [lastRefresh,     setLastRefresh]     = useState<Date>(new Date());
  const [loading,         setLoading]         = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [queueRes, schedulerRes, tasksRes] = await Promise.allSettled([
        callTool("queue_status",        {}),
        callTool("get_scheduler_status", {}),
        callTool("list_tasks",          { limit: 20 }),
      ]);

      if (queueRes.status === "fulfilled") {
        setQueueStatus(JSON.parse(queueRes.value));
      }
      if (schedulerRes.status === "fulfilled") {
        setSchedulerStatus(JSON.parse(schedulerRes.value));
      }
      if (tasksRes.status === "fulfilled") {
        const data = JSON.parse(tasksRes.value);
        setRecentTasks(data.tasks ?? []);
      }
    } catch (e) {
      console.error("LogsPanel refresh error:", e);
    } finally {
      setLoading(false);
      setLastRefresh(new Date());
    }
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

  return (
    <div className="panel">
      <div className="panel-header">
        📊 System Logs
        {loading && (
          <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 8 }}>updating…</span>
        )}
        <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: "auto" }}>
          {lastRefresh.toLocaleTimeString()}
        </span>
        <button className="refresh-btn" onClick={refresh} title="Refresh now">↻</button>
      </div>

      <div className="logs-body scroll">

        {/* ── Queue status ───────────────────────────── */}
        <section className="logs-section">
          <div className="logs-section-title">Job Queue</div>
          {queueStatus ? (
            <>
              <div className="logs-stats-grid">
                <div className="stat-card">
                  <div className="stat-val">{queueStatus.running_now}</div>
                  <div className="stat-label">Running</div>
                </div>
                <div className="stat-card">
                  <div className="stat-val">{queueStatus.in_memory_pending}</div>
                  <div className="stat-label">Pending</div>
                </div>
                <div className="stat-card">
                  <div className="stat-val">{queueStatus.max_concurrent}</div>
                  <div className="stat-label">Workers</div>
                </div>
                <div className="stat-card">
                  <div className={`stat-val ${queueStatus.enabled ? "ok" : "off"}`}>
                    {queueStatus.enabled ? "Active" : "Paused"}
                  </div>
                  <div className="stat-label">Status</div>
                </div>
              </div>

              {Object.keys(queueStatus.by_status).length > 0 && (
                <div className="logs-by-status">
                  {Object.entries(queueStatus.by_status).map(([status, count]) => (
                    <span
                      key={status}
                      className="status-pill"
                      style={{ borderColor: JOB_STATUS_COLORS[status] ?? "#7a8099" }}
                    >
                      {status}: {count}
                    </span>
                  ))}
                </div>
              )}
            </>
          ) : (
            <div className="text-muted">Loading…</div>
          )}
        </section>

        {/* ── Scheduler status ───────────────────────── */}
        <section className="logs-section">
          <div className="logs-section-title">Scheduler</div>
          {schedulerStatus ? (
            <div className="scheduler-info">
              <div className="sched-row">
                <span className="sched-label">Status</span>
                <span className={`sched-val ${schedulerStatus.config.enabled ? "ok" : "off"}`}>
                  {schedulerStatus.config.enabled ? "Enabled" : "Disabled"}
                  {schedulerStatus.state.is_running && " (running)"}
                </span>
              </div>
              <div className="sched-row">
                <span className="sched-label">Interval</span>
                <span className="sched-val">{schedulerStatus.config.interval_secs}s</span>
              </div>
              <div className="sched-row">
                <span className="sched-label">Tasks dispatched</span>
                <span className="sched-val">{schedulerStatus.state.tasks_dispatched}</span>
              </div>
              <div className="sched-row">
                <span className="sched-label">Last run</span>
                <span className="sched-val">
                  {schedulerStatus.state.last_run_at
                    ? new Date(schedulerStatus.state.last_run_at).toLocaleString()
                    : "Never"}
                </span>
              </div>
              {schedulerStatus.state.consecutive_errors > 0 && (
                <div className="sched-row warn">
                  <span className="sched-label">Consecutive errors</span>
                  <span className="sched-val">
                    {schedulerStatus.state.consecutive_errors} / {schedulerStatus.config.error_budget}
                  </span>
                </div>
              )}
              {schedulerStatus.state.last_error && (
                <div className="sched-row error">
                  <span className="sched-label">Last error</span>
                  <span className="sched-val">{schedulerStatus.state.last_error}</span>
                </div>
              )}
            </div>
          ) : (
            <div className="text-muted">Loading…</div>
          )}
        </section>

        {/* ── Recent tasks ───────────────────────────── */}
        <section className="logs-section">
          <div className="logs-section-title">Recent Tasks ({recentTasks.length})</div>
          {recentTasks.length === 0 ? (
            <div className="text-muted">No tasks found</div>
          ) : (
            <div className="task-log-list">
              {recentTasks.map((t) => (
                <div key={t.id} className="task-log-item">
                  <span
                    className="task-log-status-dot"
                    style={{ background: TASK_STATUS_COLORS[t.status] ?? "#7a8099" }}
                    title={t.status}
                  />
                  <span className="task-log-goal">{t.goal}</span>
                  <span className="task-log-status">{t.status}</span>
                </div>
              ))}
            </div>
          )}
        </section>

      </div>
    </div>
  );
}
