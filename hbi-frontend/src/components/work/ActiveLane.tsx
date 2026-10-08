/**
 * Active: what is running or about to. Live jobs (running, queued, parked)
 * grouped under the task that owns them; tasks still waiting to be planned;
 * and tasks marked in progress with no live job, which may be stuck.
 */
import { useJobList, useTaskList, type JobRow, type TaskRow } from "../../api/work";
import { statusTone } from "../../lib/status";
import { relTime } from "../../lib/time";
import { EmptyState, Pill } from "../ui";
import { EntityLink, StatusPill } from "../drawer/parts";
import Row from "./Row";

const LIVE = "running,queued,parked";
const JOB_CAP = 500;
const TASK_CAP = 200;

function owner(j: JobRow): string | null {
  const v = j.args?.["__owner_task_id"];
  return typeof v === "string" ? v : null;
}

const ORDER: Record<string, number> = { running: 0, queued: 1, parked: 2 };

export default function ActiveLane({ openId }: { openId?: string }) {
  const jobs = useJobList(LIVE, "", JOB_CAP, 3_000);
  const tasks = useTaskList("created,in_progress", "", TASK_CAP, 5_000);

  if (!jobs.data || !tasks.data) {
    const err = jobs.error ?? tasks.error;
    return (
      <div className="wk-lane">
        {err ? <EmptyState tone="red" title="Couldn't load active work" checked={err.message} /> : <div className="loading">Loading active work…</div>}
      </div>
    );
  }

  const byTask = new Map<string, JobRow[]>();
  const orphans: JobRow[] = [];
  for (const j of jobs.data) {
    const t = owner(j);
    if (t) byTask.set(t, [...(byTask.get(t) ?? []), j]);
    else orphans.push(j);
  }
  for (const list of byTask.values()) {
    list.sort((a, b) => (ORDER[a.status] ?? 9) - (ORDER[b.status] ?? 9) || a.created_at.localeCompare(b.created_at));
  }
  const taskById = new Map(tasks.data.map((t) => [t.id, t]));
  const running = [...byTask.entries()].sort(([, a], [, b]) => (ORDER[a[0].status] ?? 9) - (ORDER[b[0].status] ?? 9));
  const waiting = tasks.data.filter((t) => t.status === "created");
  const noLiveJob = tasks.data.filter((t) => t.status === "in_progress" && !byTask.has(t.id));

  const nothing = running.length === 0 && waiting.length === 0 && noLiveJob.length === 0 && orphans.length === 0;

  return (
    <div className="wk-lane scroll">
      {jobs.data.length >= JOB_CAP && (
        <EmptyState tone="yellow" title={`Showing the first ${JOB_CAP} live jobs`} checked="The queue holds more; History › Jobs lists them all by status." />
      )}
      {tasks.data.length >= TASK_CAP && (
        <EmptyState tone="yellow" title={`Showing the newest ${TASK_CAP} created or in-progress tasks`} checked="There are more; older ones are not listed here." />
      )}

      {running.length > 0 && (
        <section className="wk-group">
          <header className="wk-group-head"><span>Running and queued</span><span className="wk-count">{running.length}</span></header>
          {running.map(([taskId, list]) => (
            <ActiveChain key={taskId} taskId={taskId} task={taskById.get(taskId)} jobs={list} active={openId === taskId} />
          ))}
        </section>
      )}

      {orphans.length > 0 && (
        <section className="wk-group">
          <header className="wk-group-head"><span>Jobs with no task</span><span className="wk-count">{orphans.length}</span></header>
          {orphans.map((j) => (
            <Row key={j.id} kind="job" id={j.id} active={openId === j.id} lead={<StatusPill status={j.status} />} meta={relTime(j.created_at)}>
              <strong>{j.tool_name}</strong> <span className="wk-dim">chat, bedtime or agent-enqueued</span>
            </Row>
          ))}
        </section>
      )}

      {waiting.length > 0 && (
        <section className="wk-group">
          <header className="wk-group-head"><span>Waiting to be planned</span><span className="wk-count">{waiting.length}</span></header>
          <div className="wk-hint">The scheduler turns each into a job chain on its next tick.</div>
          {waiting.map((t: TaskRow) => (
            <Row key={t.id} kind="task" id={t.id} active={openId === t.id} lead={<StatusPill status={t.status} />} meta={relTime(t.created_at)}>
              {t.goal}
            </Row>
          ))}
        </section>
      )}

      {noLiveJob.length > 0 && (
        <section className="wk-group">
          <header className="wk-group-head"><span>In progress with no live job</span><span className="wk-count">{noLiveJob.length}</span></header>
          <div className="wk-hint">Marked in progress, but none of their jobs is running, queued or parked. A chain that ended without updating its task stays like this until the stale-task reaper fails it.</div>
          {noLiveJob.map((t) => (
            <Row key={t.id} kind="task" id={t.id} active={openId === t.id} lead={<Pill tone="yellow">no live job</Pill>} meta={relTime(t.created_at)}>
              {t.goal}
            </Row>
          ))}
        </section>
      )}

      {nothing && (
        <EmptyState title="Nothing is running"
          checked="0 running, queued or parked jobs · 0 tasks waiting to be planned · 0 tasks in progress." />
      )}
    </div>
  );
}

function ActiveChain({ taskId, task, jobs, active }: { taskId: string; task?: TaskRow; jobs: JobRow[]; active: boolean }) {
  const now = jobs.find((j) => j.status === "running") ?? jobs[0];
  return (
    <div className={`wk-chain${active ? " active" : ""}`}>
      <div className="wk-chain-head">
        <EntityLink kind="task" id={taskId}>{task?.goal ?? <span className="wk-dim">task {taskId.slice(0, 8)}</span>}</EntityLink>
        <span className="wk-dim">{relTime(now.created_at)}</span>
      </div>
      <div className="dw-chips">
        {jobs.map((j) => (
          <EntityLink key={j.id} kind="job" id={j.id} title={`${j.tool_name} · ${j.status} · try ${j.attempt_count}/${j.max_attempts}`}>
            <Pill tone={statusTone(j.status)}>{j.tool_name}</Pill>
          </EntityLink>
        ))}
      </div>
    </div>
  );
}
