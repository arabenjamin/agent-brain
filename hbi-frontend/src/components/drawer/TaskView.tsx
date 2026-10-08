/**
 * A task's lineage, top to bottom: where it came from, the job chain it ran
 * (each step's status, attempts, time and output), and what came of it.
 */
import { useTaskLineage, type LineageJob, type Origin } from "../../api/work";
import { fmtDuration } from "../../lib/status";
import { formatLocal, relTime } from "../../lib/time";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity } from "./entities";
import { EntityLink, LoadState, ResultText, Section, StatusPill } from "./parts";

function OriginLine({ origin }: { origin: Origin }) {
  if (!origin) {
    return <div className="dw-note">Created directly: no schedule, todo or parent task is recorded.</div>;
  }
  switch (origin.kind) {
    case "schedule":
      return origin.exists ? (
        <div>Run of schedule <EntityLink kind="schedule" id={origin.id}>{origin.name}</EntityLink></div>
      ) : (
        <div className="dw-note">Run of a schedule that no longer exists (<code>{origin.id}</code>).</div>
      );
    case "todo":
      return <div>Made from todo <EntityLink kind="todo" id={origin.id}>{origin.title ?? origin.id}</EntityLink></div>;
    case "task":
      return (
        <div>
          Subtask of <EntityLink kind="task" id={origin.id}>{origin.goal ?? origin.id}</EntityLink>{" "}
          <StatusPill status={origin.status} />
        </div>
      );
  }
}

function ChainStep({ job, index }: { job: LineageJob; index: number }) {
  const failed = job.error || job.dead_letter_reason;
  return (
    <li className={`dw-step tone-${job.status === "completed" ? "green" : failed ? "red" : "muted"}`}>
      <div className="dw-step-head">
        <span className="dw-step-n">{index + 1}</span>
        <EntityLink kind="job" id={job.id}><strong>{job.tool_name}</strong></EntityLink>
        <StatusPill status={job.status} />
        <span className="dw-dim">
          {job.attempt_count}/{job.max_attempts} tries · {fmtDuration(job.duration_ms)}
          {job.provider_hint && ` · ${job.provider_hint}`}
        </span>
      </div>
      {failed && <div className="tone-text-red dw-step-err">{job.dead_letter_reason ?? job.error}</div>}
      {job.result && (
        <details>
          <summary className="dw-dim">
            output · {job.result_chars.toLocaleString()} chars{job.result_truncated && " (preview)"}
          </summary>
          <ResultText text={job.result} truncated={job.result_truncated} total={job.result_chars} />
        </details>
      )}
      {(job.notes.length > 0 || (job.tasks?.length ?? 0) > 0) && (
        <div className="dw-chips">
          {job.notes.map((n) => (
            <EntityLink key={n.id} kind="note" id={n.id} title={n.preview}>
              <Pill tone="accent">→ note {n.note_type ?? ""}</Pill>
            </EntityLink>
          ))}
          {job.tasks?.map((t) => (
            <EntityLink key={t.id} kind="task" id={t.id} title={t.goal ?? undefined}>
              <Pill tone="purple">→ task</Pill>
            </EntityLink>
          ))}
        </div>
      )}
    </li>
  );
}

export default function TaskView({ id }: { id: string }) {
  const q = useTaskLineage(id);
  const d = q.data;

  return (
    <Drawer title={<><span>Task</span>{d && <StatusPill status={d.task.status} />}</>} onClose={closeEntity} wide>
      {!d ? (
        <LoadState what="task" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{d.task.goal}</h3>
          <div className="dw-dim">
            created {relTime(d.task.created_at)} ({formatLocal(d.task.created_at)}) · updated {relTime(d.task.updated_at)} ·{" "}
            <code>{d.task.id}</code>
          </div>

          <Section title="Came from">
            <OriginLine origin={d.origin} />
          </Section>

          <Section title="Ran" aside={d.jobs.length > 0 ? `${d.jobs.length} steps` : undefined}>
            {d.jobs_error && <EmptyState tone="red" title="Couldn't read this task's jobs" checked={d.jobs_error} />}
            {d.jobs.length > 0 ? (
              <ol className="dw-chain">
                {d.jobs.map((j, i) => <ChainStep key={j.id} job={j} index={i} />)}
              </ol>
            ) : (
              d.jobs_note && <EmptyState title="No job records" checked={d.jobs_note} />
            )}
          </Section>

          <Section title="Produced">
            {d.notes.length === 0 && d.created_tasks.length === 0 && d.subtasks.length === 0 ? (
              <EmptyState
                title="Nothing recorded"
                checked="No step reported a stored note or a created task, no note reflects on this task, and it has no subtasks."
              />
            ) : (
              <ul className="dw-list">
                {d.notes.map((n) => (
                  <li key={n.id}>
                    <EntityLink kind="note" id={n.id}><Pill tone="accent">{n.note_type ?? "note"}</Pill></EntityLink>
                    <span className="dw-clip">{n.preview}</span>
                    {n.via === "reflects_on" && <span className="dw-dim">review</span>}
                  </li>
                ))}
                {[...d.created_tasks, ...d.subtasks].map((t) => (
                  <li key={t.id}>
                    <EntityLink kind="task" id={t.id}><Pill tone="purple">task</Pill></EntityLink>
                    <span className="dw-clip">{t.goal}</span>
                    <StatusPill status={t.status} />
                  </li>
                ))}
              </ul>
            )}
          </Section>

          {d.task.context && (
            <details className="dw-section">
              <summary className="dw-section-head">Context</summary>
              <pre className="dw-pre">{d.task.context}</pre>
            </details>
          )}
        </>
      )}
    </Drawer>
  );
}
