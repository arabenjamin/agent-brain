/**
 * A schedule: when it runs, what it runs, how its recent runs went, and the
 * controls — run now, enable/disable, edit, delete. `id = "new"` creates one.
 */
import { useState } from "react";
import {
  deleteSchedule,
  invalidateWork,
  runScheduleNow,
  setScheduleEnabled,
  useScheduleDetail,
  type WorkSchedule,
} from "../../api/work";
import { fmtEvery } from "../../lib/status";
import { formatLocal, parseTime, relTime } from "../../lib/time";
import { useNow } from "../../lib/useNow";
import type { ScheduledTask } from "../schedules/model";
import ScheduledTaskForm from "../schedules/ScheduleForm";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity, openEntity } from "./entities";
import { EntityLink, LoadState, Section, StatusPill } from "./parts";

/** The editor's shape: `steps` as the JSON string it round-trips. */
function forEditor(s: WorkSchedule): ScheduledTask {
  return {
    id: s.id,
    name: s.name,
    description: s.description ?? undefined,
    enabled: s.enabled,
    interval_seconds: s.interval_seconds,
    steps: typeof s.steps === "string" ? s.steps : JSON.stringify(s.steps),
    last_run_at: s.last_run_at ?? undefined,
    next_run_at: s.next_run_at,
    created_at: s.created_at,
    updated_at: s.updated_at,
  };
}

type Step = { tool_name?: string; tool?: string; provider_hint?: string; arguments?: unknown; args?: unknown };

export default function ScheduleView({ id }: { id: string }) {
  const isNew = id === "new";
  const q = useScheduleDetail(id);
  const [editing, setEditing] = useState(isNew);
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const now = useNow();

  if (isNew || editing) {
    const existing = !isNew && q.data ? forEditor(q.data.schedule) : null;
    return (
      <Drawer title={isNew ? "New schedule" : "Edit schedule"} onClose={isNew ? closeEntity : () => setEditing(false)} wide>
        {!isNew && q.data?.schedule.managed_by === "yaml" && <YamlNotice />}
        <ScheduledTaskForm
          embedded
          existing={existing}
          onCancel={() => (isNew ? closeEntity() : setEditing(false))}
          onSaved={async (savedId) => {
            await invalidateWork();
            setEditing(false);
            if (isNew && savedId) openEntity("schedule", savedId);
            else if (isNew) closeEntity();
          }}
        />
      </Drawer>
    );
  }

  const d = q.data;
  const s = d?.schedule;

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

  const overdue = s && s.enabled && (parseTime(s.next_run_at)?.getTime() ?? Infinity) < now;
  const steps: Step[] = Array.isArray(s?.steps) ? (s.steps as Step[]) : [];

  const actions = s && (
    <>
      <button type="button" className="btn" disabled={!!busy}
        title={s.enabled ? "Dispatch one run now" : "Dispatch one run now, although it is turned off"}
        onClick={() => act("Run now", async () => {
          const r = await runScheduleNow(s.id);
          openEntity("task", r.task_id);
        })}>
        Run now
      </button>
      <button type="button" className="btn-ghost" disabled={!!busy}
        onClick={() => act(s.enabled ? "Turn off" : "Turn on", () => setScheduleEnabled(s.id, !s.enabled))}>
        {s.enabled ? "Turn off" : "Turn on"}
      </button>
      <button type="button" className="btn-ghost" disabled={!!busy} onClick={() => setEditing(true)}>Edit</button>
    </>
  );

  return (
    <Drawer title={<><span>Schedule</span>{s && <Pill tone={s.enabled ? "green" : "muted"}>{s.enabled ? "on" : "off"}</Pill>}</>}
      actions={actions} onClose={closeEntity} wide>
      {!d || !s ? (
        <LoadState what="schedule" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{s.name}</h3>
          {s.description && <div className="dw-note">{s.description}</div>}
          {result && <div className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</div>}
          {s.managed_by === "yaml" && <YamlNotice />}
          {s.legacy_steps > 0 && (
            <EmptyState
              tone="red"
              title={`${s.legacy_steps} step${s.legacy_steps === 1 ? " is" : "s are"} stored in the old format and cannot run`}
              checked="They use `tool`/`args`; the runner reads `tool_name`/`arguments`, so they run as an empty tool and fail with “Unknown tool”. Open Edit and save to convert them."
            />
          )}

          <Section title="When">
            <div className="dw-grid">
              <span>Cadence</span><span>{fmtEvery(s.interval_seconds)}</span>
              <span>Next run</span>
              <span>
                {s.enabled ? <>{formatLocal(s.next_run_at)} {overdue && <span className="tone-text-yellow">(due — runs at the next scheduler tick)</span>}</> : "— (turned off)"}
              </span>
              <span>Last run</span><span>{s.last_run_at ? `${formatLocal(s.last_run_at)} (${relTime(s.last_run_at)})` : "never"}</span>
            </div>
          </Section>

          <Section title="Runs" aside={d.runs_total > d.runs_shown ? `latest ${d.runs_shown} of ${d.runs_total}` : `${d.runs_total}`}>
            {d.runs.length === 0 ? (
              <EmptyState title="No runs recorded" checked="No run-record task names this schedule." />
            ) : (
              <ul className="dw-list">
                {d.runs.map((r) => (
                  <li key={r.task_id}>
                    <StatusPill status={r.status} />
                    <EntityLink kind="task" id={r.task_id}>{formatLocal(r.created_at)}</EntityLink>
                    <span className="dw-dim">{relTime(r.created_at)}</span>
                  </li>
                ))}
              </ul>
            )}
          </Section>

          <Section title="Steps" aside={`${steps.length}`}>
            <ol className="dw-steps-plain">
              {steps.map((st, i) => (
                <li key={i}>
                  {st.tool_name
                    ? <strong>{st.tool_name}</strong>
                    : <><strong className="tone-text-red">{st.tool ?? "(no tool)"}</strong> <span className="tone-text-red">old format — will not run</span></>}
                  {st.provider_hint && <span className="dw-dim"> · {st.provider_hint}</span>}
                  {(st.arguments ?? st.args) != null && (
                    <details>
                      <summary className="dw-dim">arguments</summary>
                      <pre className="dw-pre">{JSON.stringify(st.arguments ?? st.args, null, 2)}</pre>
                    </details>
                  )}
                </li>
              ))}
            </ol>
          </Section>

          <Section title="Danger">
            <button type="button" className="btn danger" disabled={!!busy}
              onClick={() => {
                const warn = s.managed_by === "yaml"
                  ? `\n\nIt is defined in a schedules/*.yaml file, so the next restart will create it again. Remove the file to delete it for good.`
                  : "";
                if (window.confirm(`Delete “${s.name}”?${warn}`)) {
                  act("Delete", async () => { await deleteSchedule(s.id); closeEntity(); });
                }
              }}>
              Delete schedule
            </button>
          </Section>
          <div className="dw-dim"><code>{s.id}</code> · managed by {s.managed_by ?? "unknown"}</div>
        </>
      )}
    </Drawer>
  );
}

function YamlNotice() {
  return (
    <EmptyState
      tone="yellow"
      title="Defined in a schedules/*.yaml file"
      checked="On every restart the file's steps, description and interval replace what is stored, so edits to those here last only until then. Turning it on or off, and Run now, are kept."
    />
  );
}
