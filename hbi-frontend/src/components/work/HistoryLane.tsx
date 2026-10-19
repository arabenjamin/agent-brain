/**
 * History: the work, live and done. Active shows what is running or about to
 * (it was its own Work view until 2026-10-19); Tasks and Jobs show what came
 * of it, filtered by status on the server. Home's drill-downs land here with
 * `task_status` / `job_status` / `since`.
 */
import { lazy, Suspense, useState } from "react";
import { useJobList, useTaskList } from "../../api/work";
import { formatLocal, relTime } from "../../lib/time";
import { goTo } from "../../nav";
import { StatusPill } from "../drawer/parts";
import { DrillBanner, EmptyState, Tabs } from "../ui";
import Row from "./Row";

const ActiveLane = lazy(() => import("./ActiveLane"));

const LIMIT = 300;

const TASK_FILTERS: { id: string; label: string; status: string }[] = [
  { id: "finished", label: "Finished", status: "completed,failed" },
  { id: "completed", label: "Completed", status: "completed" },
  { id: "failed", label: "Failed", status: "failed" },
  { id: "blocked", label: "Blocked", status: "blocked" },
  { id: "all", label: "All", status: "" },
];

const JOB_FILTERS: { id: string; label: string; status: string }[] = [
  { id: "all", label: "All", status: "" },
  { id: "completed", label: "Completed", status: "completed" },
  { id: "dead", label: "Failed / dead", status: "failed,dead,dead_letter" },
  { id: "cancelled", label: "Cancelled", status: "cancelled" },
];

export interface HistoryDrill {
  mode?: string;
  taskStatus?: string;
  jobStatus?: string;
  since?: string;
}

export default function HistoryLane({ drill, openId }: { drill: HistoryDrill; openId?: string }) {
  // No mode: a drill-down's filter picks its tab, otherwise Active.
  const mode = drill.mode === "active" || drill.mode === "jobs" || drill.mode === "tasks" ? drill.mode
    : drill.jobStatus ? "jobs"
    : drill.taskStatus || drill.since ? "tasks"
    : "active";
  return (
    <div className="wk-lane-frame">
      <div className="wk-lane-bar">
        <Tabs
          tabs={[{ id: "active", label: "Active" }, { id: "tasks", label: "Tasks" }, { id: "jobs", label: "Jobs" }]}
          active={mode}
          onChange={(id) => goTo("work", { view: "history", mode: id })}
        />
      </div>
      {mode === "active"
        ? <Suspense fallback={<div className="loading">Loading active work…</div>}><ActiveLane openId={openId} /></Suspense>
        : mode === "tasks"
        ? <TaskHistory drillStatus={drill.taskStatus} since={drill.since} openId={openId} />
        : <JobHistory drillStatus={drill.jobStatus} since={drill.since} openId={openId} />}
    </div>
  );
}

function TaskHistory({ drillStatus, since, openId }: { drillStatus?: string; since?: string; openId?: string }) {
  const [filter, setFilter] = useState(() => (drillStatus ? (TASK_FILTERS.find((f) => f.status === drillStatus)?.id ?? "custom") : "finished"));
  // "custom": a drilled status list that no filter button names.
  const status = filter === "custom" ? drillStatus ?? "" : TASK_FILTERS.find((f) => f.id === filter)?.status ?? "";
  const q = useTaskList(status, since ?? "", LIMIT, 15_000);

  return (
    <div className="wk-lane scroll">
      {since && (
        <DrillBanner rows={q.data?.length} cap={LIMIT} onClear={() => goTo("work", { view: "history", mode: "tasks" })}>
          tasks {status ? <strong>{status.replaceAll(",", " / ")}</strong> : "of any status"} created since {formatLocal(since)}
        </DrillBanner>
      )}
      <div className="wk-toolbar">
        <Tabs tabs={TASK_FILTERS.map((f) => ({ id: f.id, label: f.label }))} active={filter} onChange={setFilter} />
      </div>
      {!q.data ? (
        q.error ? <EmptyState tone="red" title="Couldn't load tasks" checked={q.error.message} /> : <div className="loading">Loading tasks…</div>
      ) : q.data.length === 0 ? (
        <EmptyState title="No tasks" checked={`No task with status ${status || "any"}${since ? ` created since ${formatLocal(since)}` : ""}.`} />
      ) : (
        <>
          {!since && q.data.length >= LIMIT && <div className="wk-hint">The newest {LIMIT}.</div>}
          {q.data.map((t) => (
            <Row key={t.id} kind="task" id={t.id} active={openId === t.id} lead={<StatusPill status={t.status} />}
              meta={<span title={formatLocal(t.created_at)}>{relTime(t.created_at)}</span>}>
              {t.goal}
            </Row>
          ))}
        </>
      )}
    </div>
  );
}

function JobHistory({ drillStatus, since, openId }: { drillStatus?: string; since?: string; openId?: string }) {
  const [filter, setFilter] = useState(() => (drillStatus ? (JOB_FILTERS.find((f) => f.status === drillStatus)?.id ?? "custom") : "all"));
  const status = filter === "custom" ? drillStatus ?? "" : JOB_FILTERS.find((f) => f.id === filter)?.status ?? "";
  const q = useJobList(status, since ?? "", LIMIT, 10_000);

  return (
    <div className="wk-lane scroll">
      {(since || drillStatus) && (
        <DrillBanner rows={q.data?.length} cap={LIMIT} onClear={() => goTo("work", { view: "history", mode: "jobs" })}>
          jobs {status ? <strong>{status.replaceAll(",", " / ")}</strong> : "of any status"}{since && <> updated since {formatLocal(since)}</>}
        </DrillBanner>
      )}
      <div className="wk-toolbar">
        <Tabs tabs={JOB_FILTERS.map((f) => ({ id: f.id, label: f.label }))} active={filter} onChange={setFilter} />
        <span className="wk-dim">Job records are pruned after their retention window.</span>
      </div>
      {!q.data ? (
        q.error ? <EmptyState tone="red" title="Couldn't load jobs" checked={q.error.message} /> : <div className="loading">Loading jobs…</div>
      ) : q.data.length === 0 ? (
        <EmptyState title="No jobs" checked={`No job with status ${status || "any"}${since ? ` updated since ${formatLocal(since)}` : ""}.`} />
      ) : (
        <>
          {!since && q.data.length >= LIMIT && <div className="wk-hint">The newest {LIMIT}.</div>}
          {q.data.map((j) => (
            <Row key={j.id} kind="job" id={j.id} active={openId === j.id} lead={<StatusPill status={j.status} />}
              meta={<span title={formatLocal(j.updated_at)}>{relTime(j.updated_at)}</span>}>
              <strong>{j.tool_name}</strong>
              {j.error && <span className="wk-dim"> · {j.error.slice(0, 140)}</span>}
            </Row>
          ))}
        </>
      )}
    </div>
  );
}
