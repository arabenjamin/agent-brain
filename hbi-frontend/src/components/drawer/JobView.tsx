/** One job in full: arguments, output, error, its task and its place in the chain. */
import { useState } from "react";
import { cancelJob, retryJob, useJobDetail } from "../../api/work";
import { LIVE_JOB, TERMINAL_FAILED, fmtDuration } from "../../lib/status";
import { formatLocal } from "../../lib/time";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity } from "./entities";
import { EntityLink, LoadState, ResultText, Section, StatusPill } from "./parts";

export default function JobView({ id }: { id: string }) {
  const q = useJobDetail(id);
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const d = q.data;

  const act = async (label: string, fn: () => Promise<unknown>) => {
    setBusy(label);
    setResult(null);
    try {
      await fn();
      setResult({ ok: true, text: `${label}: done` });
    } catch (e) {
      setResult({ ok: false, text: `${label} failed: ${e instanceof Error ? e.message : String(e)}` });
    } finally {
      setBusy(null);
      q.refetch();
    }
  };

  const job = d?.job;
  const actions = job && (
    <>
      {LIVE_JOB.includes(job.status) && (
        <button type="button" className="btn danger" disabled={!!busy}
          onClick={() => window.confirm(`Cancel ${job.tool_name}? Its parked successors are cancelled too.`) && act("Cancel", () => cancelJob(job.id))}>
          Cancel
        </button>
      )}
      {(TERMINAL_FAILED.includes(job.status) || job.status === "cancelled") && (
        <button type="button" className="btn" disabled={!!busy} onClick={() => act("Retry", () => retryJob(job.id, job.status))}>
          Retry
        </button>
      )}
    </>
  );

  return (
    <Drawer title={<><span>Job</span>{job && <StatusPill status={job.status} />}</>} actions={actions} onClose={closeEntity} wide>
      {!d || !job ? (
        <LoadState what="job" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{job.tool_name}</h3>
          <div className="dw-dim">
            {job.attempt_count}/{job.max_attempts} tries · {fmtDuration(job.duration_ms)}
            {job.provider_hint && ` · ${job.provider_hint}`} · <code>{job.id}</code>
          </div>
          {result && <div className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</div>}

          <Section title="Part of">
            {d.owner_task ? (
              d.owner_task.exists ? (
                <div>
                  <EntityLink kind="task" id={d.owner_task.id}>{d.owner_task.goal ?? d.owner_task.id}</EntityLink>{" "}
                  <StatusPill status={d.owner_task.status} />
                </div>
              ) : (
                <div className="dw-note">Its task (<code>{d.owner_task.id}</code>) no longer exists.</div>
              )
            ) : (
              <div className="dw-note">No owning task is recorded (a chat, bedtime or agent-enqueued chain).</div>
            )}
            {d.chain.length > 1 && (
              <ol className="dw-mini-chain">
                {d.chain.map((c) => (
                  <li key={c.id} className={c.id === job.id ? "current" : ""}>
                    {c.id === job.id ? <strong>{c.tool_name}</strong> : <EntityLink kind="job" id={c.id}>{c.tool_name}</EntityLink>}{" "}
                    <StatusPill status={c.status} />
                  </li>
                ))}
              </ol>
            )}
          </Section>

          {(job.error || job.dead_letter_reason) && (
            <Section title="Error">
              <pre className="dw-pre tone-text-red">{job.dead_letter_reason ?? job.error}</pre>
            </Section>
          )}

          <Section title="Timing">
            <div className="dw-grid">
              <span>Created</span><span>{formatLocal(job.created_at)}</span>
              <span>Started</span><span>{formatLocal(job.started_at)}</span>
              <span>Finished</span><span>{formatLocal(job.completed_at)}</span>
            </div>
          </Section>

          <Section title="Arguments">
            {job.args && Object.keys(job.args).length > 0
              ? <pre className="dw-pre">{JSON.stringify(job.args, null, 2)}</pre>
              : <div className="dw-note">No arguments.</div>}
          </Section>

          <Section title="Output" aside={job.result ? `${job.result_chars.toLocaleString()} chars` : undefined}>
            {job.result
              ? <ResultText text={job.result} truncated={job.result_truncated} total={job.result_chars} />
              : <EmptyState title="No output stored" checked={LIVE_JOB.includes(job.status) ? "The job has not finished." : "The job record has no result."} />}
            {job.notes.length > 0 && (
              <div className="dw-chips">
                {job.notes.map((n) => (
                  <EntityLink key={n.id} kind="note" id={n.id} title={n.preview}><Pill tone="accent">→ note {n.note_type ?? ""}</Pill></EntityLink>
                ))}
              </div>
            )}
          </Section>
        </>
      )}
    </Drawer>
  );
}
