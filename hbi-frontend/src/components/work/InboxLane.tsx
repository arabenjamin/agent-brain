/** Inbox: what needs the user — proposals, overdue todos, and recent failures. */
import type { ReactNode } from "react";
import { useProposals, useTodos } from "../../api/queries";
import { useJobList, useTaskList } from "../../api/work";
import { relTime } from "../../lib/time";
import { useNow } from "../../lib/useNow";
import { hrefTo } from "../../nav";
import { isTodoOverdue } from "../todos/model";
import { EmptyState, Pill } from "../ui";
import { StatusPill } from "../drawer/parts";
import Row from "./Row";

const WEEK_MS = 7 * 86_400_000;
const FAILED_JOB = "failed,dead,dead_letter";

const CAP = 100;

function Group({ title, count, cap, more, children }: { title: string; count: number; cap?: number; more?: ReactNode; children: ReactNode }) {
  if (count === 0) return null;
  const capped = cap !== undefined && count >= cap;
  return (
    <section className="wk-group">
      <header className="wk-group-head">
        <span>{title}</span>
        <span className="wk-count" title={capped ? `The first ${cap}; there may be more` : undefined}>{capped ? `${cap}+` : count}</span>
        {more}
      </header>
      {children}
    </section>
  );
}

export default function InboxLane({ openId }: { openId?: string }) {
  const now = useNow();
  // A stable window start: rounded to the hour, so the query key doesn't change every render.
  const since = new Date(Math.floor((now - WEEK_MS) / 3_600_000) * 3_600_000).toISOString();
  const proposals = useProposals();
  const todos = useTodos();
  const failedTasks = useTaskList("failed", since, CAP, 30_000);
  const deadJobs = useJobList(FAILED_JOB, since, CAP, 30_000);

  const overdue = (todos.data ?? []).filter((t) => isTodoOverdue(t, now));
  const loading = !proposals.data || !todos.data || !failedTasks.data || !deadJobs.data;
  const errors = [proposals.error, todos.error, failedTasks.error, deadJobs.error].filter(Boolean) as Error[];
  const total = (proposals.data?.length ?? 0) + overdue.length + (failedTasks.data?.length ?? 0) + (deadJobs.data?.length ?? 0);

  return (
    <div className="wk-lane scroll">
      {errors.map((e, i) => <EmptyState key={i} tone="red" title="Part of the inbox could not be loaded" checked={e.message} />)}

      <Group title="Proposals awaiting review" count={proposals.data?.length ?? 0}>
        {proposals.data?.map((p) => (
          <Row key={p.filename} kind="proposal" id={p.filename} active={openId === p.filename}
            lead={<Pill tone={p.severity === "high" ? "red" : p.severity === "medium" ? "yellow" : "green"}>{p.severity}</Pill>}
            meta={relTime(p.created)}>
            {p.title}
          </Row>
        ))}
      </Group>

      <Group title="Overdue todos" count={overdue.length}>
        {overdue.map((t) => (
          <Row key={t.id} kind="todo" id={t.id} active={openId === t.id}
            lead={<Pill tone="yellow">due {t.due_at?.slice(0, 10)}</Pill>}>
            {t.title}
          </Row>
        ))}
      </Group>

      <Group title="Tasks that failed this week" count={failedTasks.data?.length ?? 0} cap={CAP}
        more={<a className="wk-more" href={hrefTo("work", { view: "history", mode: "tasks", task_status: "failed" })}>all failed →</a>}>
        {failedTasks.data?.map((t) => (
          <Row key={t.id} kind="task" id={t.id} active={openId === t.id} lead={<StatusPill status={t.status} />} meta={relTime(t.created_at)}>
            {t.goal}
          </Row>
        ))}
      </Group>

      <Group title="Jobs that died this week" count={deadJobs.data?.length ?? 0} cap={CAP}
        more={<a className="wk-more" href={hrefTo("work", { view: "history", mode: "jobs", job_status: FAILED_JOB })}>all →</a>}>
        {deadJobs.data?.map((j) => (
          <Row key={j.id} kind="job" id={j.id} active={openId === j.id} lead={<StatusPill status={j.status} />} meta={relTime(j.updated_at)}>
            <strong>{j.tool_name}</strong> <span className="wk-dim">{(j.error ?? "").slice(0, 140)}</span>
          </Row>
        ))}
      </Group>

      {!loading && total === 0 && errors.length === 0 && (
        <EmptyState
          title="Nothing needs you"
          checked={`0 proposals awaiting review · 0 overdue of ${todos.data?.filter((t) => t.status !== "done").length ?? 0} open todos · 0 tasks failed and 0 jobs died since ${new Date(since).toLocaleDateString()}`}
        />
      )}
      {loading && errors.length === 0 && <div className="loading">Loading inbox…</div>}
    </div>
  );
}
