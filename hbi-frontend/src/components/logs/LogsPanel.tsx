import { useCallback, useEffect, useRef, useState } from "react";
import { getBrainUrl, getApiKey } from "../../api/config";

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
    is_sleeping: boolean;
  };
}

interface Task {
  id: string;
  goal: string;
  status: string;
  created_at: string;
}

interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function authHeaders(): Record<string, string> {
  const key = getApiKey();
  return key ? { Authorization: `Bearer ${key}` } : {};
}

async function apiFetch(path: string) {
  return fetch(`${getBrainUrl()}${path}`, { headers: authHeaders() });
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

const LOG_LEVEL_COLORS: Record<string, string> = {
  ERROR: "#f87171",
  WARN:  "#fbbf24",
  INFO:  "#89b4fa",
  DEBUG: "#a6adc8",
  TRACE: "#585b70",
};

// ── Main component ────────────────────────────────────────────────────────────

export default function LogsPanel() {
  const [queueStatus,     setQueueStatus]     = useState<QueueStatus | null>(null);
  const [schedulerStatus, setSchedulerStatus] = useState<SchedulerStatus | null>(null);
  const [recentTasks,     setRecentTasks]     = useState<Task[]>([]);
  const [logEntries,      setLogEntries]      = useState<LogEntry[]>([]);
  const [logLevel,        setLogLevel]        = useState<string>("info");
  const [lastRefresh,     setLastRefresh]     = useState<Date>(new Date());
  const [loading,         setLoading]         = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [queueRes, schedulerRes, tasksRes, logsRes] = await Promise.allSettled([
        apiFetch("/api/queue/status"),
        apiFetch("/api/scheduler/status"),
        apiFetch("/api/tasks?limit=20"),
        apiFetch(`/api/logs?limit=200&level=${logLevel}`),
      ]);

      if (queueRes.status === "fulfilled" && queueRes.value.ok) {
        const data = await queueRes.value.json();
        setQueueStatus(data);
      }
      if (schedulerRes.status === "fulfilled" && schedulerRes.value.ok) {
        const data = await schedulerRes.value.json();
        setSchedulerStatus(data);
      }
      if (tasksRes.status === "fulfilled" && tasksRes.value.ok) {
        const data = await tasksRes.value.json();
        setRecentTasks(data.tasks ?? []);
      }
      if (logsRes.status === "fulfilled" && logsRes.value.ok) {
        const data = await logsRes.value.json();
        setLogEntries(data.entries ?? []);
      }
    } catch (e) {
      console.error("LogsPanel refresh error:", e);
    } finally {
      setLoading(false);
      setLastRefresh(new Date());
    }
  }, [logLevel]);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

  // Scroll log pane to top when entries refresh
  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = 0;
  }, [logEntries]);

  return (
    <div className="panel">
      <div className="panel-header">
        System Logs
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
                  {schedulerStatus.state.is_sleeping && " (sleeping)"}
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

        {/* ── Process logs ───────────────────────────── */}
        <section className="logs-section">
          <div className="logs-section-title" style={{ display: "flex", alignItems: "center", gap: "10px" }}>
            <span>Process Logs</span>
            <select
              value={logLevel}
              onChange={(e) => setLogLevel(e.target.value)}
              style={{
                marginLeft: "auto",
                background: "var(--surface2, #313244)",
                border: "1px solid var(--border, #45475a)",
                borderRadius: "4px",
                padding: "2px 6px",
                color: "inherit",
                fontSize: "0.78rem",
              }}
            >
              <option value="debug">DEBUG+</option>
              <option value="info">INFO+</option>
              <option value="warn">WARN+</option>
              <option value="error">ERROR</option>
            </select>
          </div>

          {logEntries.length === 0 ? (
            <div className="text-muted">
              No log entries yet — they appear here once the server generates them after startup.
            </div>
          ) : (
            <div
              ref={logRef}
              style={{
                fontFamily: "monospace",
                fontSize: "0.75rem",
                maxHeight: "400px",
                overflowY: "auto",
                display: "flex",
                flexDirection: "column",
                gap: "1px",
              }}
            >
              {logEntries.map((entry, i) => (
                <div
                  key={i}
                  style={{
                    display: "grid",
                    gridTemplateColumns: "90px 48px 1fr",
                    gap: "6px",
                    padding: "2px 4px",
                    borderRadius: "3px",
                    background: entry.level === "ERROR" ? "rgba(248,113,113,0.08)"
                      : entry.level === "WARN" ? "rgba(251,191,36,0.06)"
                      : "transparent",
                  }}
                >
                  <span style={{ opacity: 0.45, whiteSpace: "nowrap", overflow: "hidden" }}>
                    {entry.timestamp.slice(11, 23)}
                  </span>
                  <span
                    style={{
                      color: LOG_LEVEL_COLORS[entry.level] ?? "#a6adc8",
                      fontWeight: 600,
                    }}
                  >
                    {entry.level}
                  </span>
                  <span style={{ wordBreak: "break-word", opacity: 0.85 }}>
                    <span style={{ opacity: 0.45 }}>[{entry.target}]</span>{" "}
                    {entry.message}
                  </span>
                </div>
              ))}
            </div>
          )}
        </section>

      </div>
    </div>
  );
}
