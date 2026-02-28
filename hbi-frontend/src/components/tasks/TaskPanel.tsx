import { useCallback, useEffect, useState } from "react";
import { callTool } from "../../api/mcp";

interface Task {
  id: string;
  goal: string;
  status: string;
  parent_id?: string;
  created_at?: string;
}

interface QueueStatus {
  in_memory_pending: number;
  running_now: number;
  max_concurrent: number;
  enabled: boolean;
  by_status: Record<string, number>;
}

type StatusFilter = "all" | "created" | "in_progress" | "completed" | "failed" | "blocked";

export default function TaskPanel() {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [queue, setQueue] = useState<QueueStatus | null>(null);
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [taskJson, queueJson] = await Promise.all([
        callTool("list_tasks", { limit: 100 }),
        callTool("queue_status", {}),
      ]);

      const taskData = JSON.parse(taskJson);
      setTasks(taskData.tasks ?? []);

      const queueData = JSON.parse(queueJson);
      setQueue(queueData);
      setLastRefresh(new Date());
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  // Initial load + auto-refresh every 8 seconds.
  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 8000);
    return () => clearInterval(id);
  }, [refresh]);

  const filtered = tasks.filter(
    (t) => filter === "all" || t.status === filter
  );

  const statusFilters: StatusFilter[] = [
    "all", "created", "in_progress", "completed", "failed", "blocked",
  ];

  const statusCount = (s: StatusFilter) =>
    s === "all" ? tasks.length : tasks.filter((t) => t.status === s).length;

  return (
    <div className="panel">
      <div className="panel-header">
        📋 Tasks &amp; Queue
        {loading && <span style={{ color: "var(--text-muted)", marginLeft: 8, fontSize: 11 }}>refreshing…</span>}
        {lastRefresh && (
          <span style={{ color: "var(--text-muted)", fontSize: 10, marginLeft: 4 }}>
            {lastRefresh.toLocaleTimeString()}
          </span>
        )}
        <button className="refresh-btn" onClick={refresh} title="Refresh">↻</button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div className="filter-row">
        {statusFilters.map((s) => (
          <button
            key={s}
            className={`filter-btn${filter === s ? " active" : ""}`}
            onClick={() => setFilter(s)}
          >
            {s} ({statusCount(s)})
          </button>
        ))}
      </div>

      <div className="tasks-grid">
        {/* Task list */}
        <div className="tasks-list">
          {filtered.length === 0 && !loading && (
            <div className="empty-state" style={{ flex: 1, paddingTop: 40 }}>
              <span className="icon">✓</span>
              <span>No tasks matching "{filter}"</span>
            </div>
          )}
          {filtered.map((task) => (
            <div key={task.id} className="task-card">
              <div className="task-card-header">
                <span className="task-goal" title={task.goal}>
                  {task.parent_id ? "↳ " : ""}{task.goal}
                </span>
                <span className={`task-status-badge ${task.status}`}>
                  {task.status.replace("_", " ")}
                </span>
              </div>
              <div className="task-meta">
                ID: {task.id.slice(0, 8)}…
                {task.created_at && (
                  <> · {new Date(task.created_at).toLocaleString()}</>
                )}
              </div>
            </div>
          ))}
        </div>

        {/* Queue sidebar */}
        <div className="queue-sidebar">
          {queue ? (
            <>
              <div className="queue-stat">
                <div className="queue-stat-label">Pending</div>
                <div className="queue-stat-value">{queue.in_memory_pending}</div>
              </div>
              <div className="queue-stat">
                <div className="queue-stat-label">Running</div>
                <div className="queue-stat-value" style={{ color: queue.running_now > 0 ? "var(--yellow)" : "var(--text-muted)" }}>
                  {queue.running_now}
                </div>
              </div>
              <div className="queue-stat">
                <div className="queue-stat-label">Worker</div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, marginTop: 4 }}>
                  <span className={`dot ${queue.enabled ? "green" : "red"}`} />
                  <span style={{ fontSize: 12, color: "var(--text-dim)" }}>
                    {queue.enabled ? "enabled" : "paused"}
                  </span>
                </div>
              </div>
              {queue.by_status && Object.keys(queue.by_status).length > 0 && (
                <div className="queue-breakdown">
                  <div className="queue-breakdown-title">By status</div>
                  {Object.entries(queue.by_status).map(([k, v]) => (
                    <div key={k} className="queue-row">
                      <span>{k}</span>
                      <span>{v}</span>
                    </div>
                  ))}
                </div>
              )}
            </>
          ) : (
            <div className="loading">Loading queue…</div>
          )}
        </div>
      </div>
    </div>
  );
}
