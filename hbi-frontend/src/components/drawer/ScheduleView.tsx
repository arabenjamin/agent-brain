/**
 * A workflow: when it runs (its schedule, or by name), its inputs, what it
 * runs, how its recent runs went, and the controls — run now (with inputs),
 * enable/disable, edit, delete. `id = "new"` creates one.
 */
import { Fragment, useState } from "react";
import {
  deleteSchedule,
  invalidateWork,
  runScheduleNow,
  setScheduleEnabled,
  useScheduleDetail,
  useScheduleTopicMix,
  type WorkSchedule,
} from "../../api/work";
import { useTopics } from "../../api/topics";
import { moveWorkflow, usePrograms } from "../../api/programs";
import { KIND_LABELS, mixFlag, type TopicMix } from "../../lib/scheduleGroups";
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
    topic: s.topic ?? null,
    kind: s.kind ?? null,
  };
}

type Step = { tool_name?: string; tool?: string; provider_hint?: string; arguments?: unknown; args?: unknown };

export default function ScheduleView({ id }: { id: string }) {
  const isNew = id === "new";
  const q = useScheduleDetail(id);
  const [editing, setEditing] = useState(isNew);
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [inputs, setInputs] = useState<Record<string, string>>({});
  const now = useNow();

  if (isNew || editing) {
    const existing = !isNew && q.data ? forEditor(q.data.workflow) : null;
    return (
      <Drawer title={isNew ? "New workflow" : "Edit workflow"} onClose={isNew ? closeEntity : () => setEditing(false)} wide>
        {!isNew && q.data?.workflow.managed_by === "yaml" && <YamlNotice name={q.data.workflow.name} />}
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
  const s = d?.workflow;

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

  const scheduled = !!s && s.interval_seconds != null && s.next_run_at != null;
  const overdue = s && s.enabled && scheduled && (parseTime(s.next_run_at ?? "")?.getTime() ?? Infinity) < now;
  const missing = s ? s.inputs.filter((i) => i.required && !(inputs[i.name] ?? "").trim()).map((i) => i.name) : [];
  const steps: Step[] = Array.isArray(s?.steps) ? (s.steps as Step[]) : [];

  const actions = s && (
    <>
      <button type="button" className="btn" disabled={!!busy || missing.length > 0}
        title={missing.length > 0 ? `Fill in ${missing.join(", ")} under Inputs first`
          : s.enabled ? "Dispatch one run now" : "Dispatch one run now, although it is turned off"}
        onClick={() => act("Run now", async () => {
          const r = await runScheduleNow(s.id, inputs);
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
    <Drawer title={<><span>Workflow</span>{s && <Pill tone={s.enabled ? "green" : "muted"}>{s.enabled ? "on" : "off"}</Pill>}</>}
      actions={actions} onClose={closeEntity} wide>
      {!d || !s ? (
        <LoadState what="workflow" id={id} error={q.error} onRetry={() => q.refetch()} />
      ) : (
        <>
          <h3 className="dw-title">{s.name}</h3>
          {s.description && <div className="dw-note">{s.description}</div>}
          {result && <div className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</div>}
          {s.unavailable && (
            <EmptyState
              tone="red"
              title="This workflow cannot run"
              checked={`${s.unavailable}. Checked at every start against the registered tools, MCP servers and enabled plugins; runs are refused until it clears.`}
            />
          )}
          {s.managed_by === "yaml" && <YamlNotice name={s.name} plugin={s.plugin} />}
          {s.legacy_steps > 0 && (
            <EmptyState
              tone="red"
              title={`${s.legacy_steps} step${s.legacy_steps === 1 ? " is" : "s are"} stored in the old format and cannot run`}
              checked="They use `tool`/`args`; the runner reads `tool_name`/`arguments`, so they run as an empty tool and fail with “Unknown tool”. Open Edit and save to convert them."
            />
          )}

          <Section title="When">
            <div className="dw-grid">
              <span>Cadence</span><span>{scheduled ? fmtEvery(s.interval_seconds) : "no schedule: runs when started by name"}</span>
              {scheduled && <>
                <span>Next run</span>
                <span>
                  {s.enabled ? <>{formatLocal(s.next_run_at ?? "")} {overdue && <span className="tone-text-yellow">(due — runs at the next scheduler tick)</span>}</> : "— (turned off)"}
                </span>
              </>}
              {s.checks.length > 0 && <><span>Checks</span><span>{s.checks.join(", ")}</span></>}
              {s.is_default && <><span>Default</span><span>free-text tasks run this workflow</span></>}
              <span>Last run</span><span>{s.last_run_at ? `${formatLocal(s.last_run_at)} (${relTime(s.last_run_at)})` : "never"}</span>
            </div>
          </Section>

          {s.inputs.length > 0 && (
            <Section title="Inputs" aside="for Run now">
              <div className="dw-grid">
                {s.inputs.map((i) => (
                  <Fragment key={i.name}>
                    <span title={i.description ?? undefined}>{i.name}{i.required ? " *" : ""}</span>
                    <input className="ui-input" value={inputs[i.name] ?? ""} placeholder={i.description ?? ""}
                      onChange={(e) => setInputs({ ...inputs, [i.name]: e.target.value })} />
                  </Fragment>
                ))}
              </div>
            </Section>
          )}

          <ProgramSection name={s.name} />

          <TopicSection s={s} />

          <Section title="Runs" aside={d.runs_total > d.runs_shown ? `latest ${d.runs_shown} of ${d.runs_total}` : `${d.runs_total}`}>
            {d.runs.length === 0 ? (
              <EmptyState title="No runs recorded" checked="No task names this workflow as the one it ran." />
            ) : (
              <ul className="dw-list">
                {d.runs.map((r) => (
                  <li key={r.task_id}>
                    <StatusPill status={r.status} />
                    <EntityLink kind="task" id={r.task_id}>{formatLocal(r.created_at)}</EntityLink>
                    <span className="dw-dim">{relTime(r.created_at)}</span>
                    {r.goal && r.goal !== s.name && <span className="dw-dim">{r.goal}</span>}
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
                  ? `\n\nIt is defined in workflows/${s.name}.yaml, so the next restart will create it again. Remove the file to delete it for good.`
                  : "";
                if (window.confirm(`Delete “${s.name}”?${warn}`)) {
                  act("Delete", async () => { await deleteSchedule(s.id); closeEntity(); });
                }
              }}>
              Delete workflow
            </button>
          </Section>
          <div className="dw-dim"><code>{s.id}</code> · managed by {s.managed_by ?? "unknown"}</div>
        </>
      )}
    </Drawer>
  );
}

/**
 * The program that owns this workflow, and a picker to assign or move it.
 * Membership lives on the program (its `workflows` list), so this edits the
 * programs involved; a seeded program edited here becomes runtime-owned.
 */
function ProgramSection({ name }: { name: string }) {
  const tree = usePrograms();
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<{ ok: boolean; text: string } | null>(null);
  const rows = tree.data?.programs ?? [];
  const owner = rows.find((r) => r.workflow_names.includes(name)) ?? null;
  const choices = rows.filter((r) => r.name !== owner?.name && r.status !== "closed");

  const move = async (to: string | null) => {
    setBusy(true);
    setSaid(null);
    try {
      const lines = await moveWorkflow(name, owner?.name ?? null, to);
      setSaid({ ok: true, text: lines.join(" ") });
      setTarget("");
    } catch (e) {
      setSaid({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Section title="Program">
      {tree.error ? (
        <EmptyState tone="red" title="Couldn't load programs" checked={tree.error.message} />
      ) : !tree.data ? (
        <div className="dw-dim">loading…</div>
      ) : (
        <>
          <div className="dw-grid">
            <span>Owned by</span>
            <span>
              {owner
                ? <><EntityLink kind="program" id={owner.name}>{owner.label}</EntityLink> <span className="dw-dim">{owner.kind}{owner.status !== "active" ? ` · ${owner.status}` : ""}</span></>
                : <span className="tone-text-yellow">no program: no one is accountable for it</span>}
            </span>
          </div>
          <div className="dw-row" style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 6, flexWrap: "wrap" }}>
            <select className="ui-input" value={target} disabled={busy} onChange={(e) => setTarget(e.target.value)} style={{ flex: "1 1 200px" }}>
              <option value="">{owner ? "Move to…" : "Assign to…"}</option>
              {choices.map((r) => (
                <option key={r.name} value={r.name}>{"\u00a0\u00a0".repeat(r.depth)}{r.label} ({r.kind}{r.status !== "active" ? `, ${r.status}` : ""})</option>
              ))}
            </select>
            <button type="button" className="btn" disabled={busy || !target} onClick={() => move(target)}>
              {owner ? "Move" : "Assign"}
            </button>
            {owner && (
              <button type="button" className="btn-ghost" disabled={busy}
                onClick={() => window.confirm(`Take “${name}” out of ${owner.label}? It will belong to no program.`) && move(null)}>
                Remove
              </button>
            )}
          </div>
          {target && rows.find((r) => r.name === target)?.status === "paused" && (
            <div className="tone-text-yellow dw-dim">That program is paused: assigning it there turns this workflow off until the program resumes.</div>
          )}
          {said && <div className={said.ok ? "tone-text-green" : "tone-text-red"}>{said.text}</div>}
        </>
      )}
    </Section>
  );
}

/** Declared topic and kind beside what the last 30 days of runs wrote. */
function TopicSection({ s }: { s: WorkSchedule }) {
  const mix = useScheduleTopicMix();
  const topics = useTopics().data?.topics ?? [];
  const labels = Object.fromEntries(topics.map((t) => [t.name, t.label]));
  const m: TopicMix | undefined = mix.data?.workflows.find((x) => x.id === s.id);
  const flag = mixFlag(m, labels);
  const assessed = m ? m.topics.reduce((a, t) => a + t.notes, 0) + m.fits_none : 0;
  return (
    <Section title="Topic" aside={mix.data ? `last ${mix.data.days} days` : undefined}>
      <div className="dw-grid">
        <span>Declared</span>
        <span>
          {s.topic ? <EntityLink kind="topic" id={s.topic}>{labels[s.topic] ?? s.topic}</EntityLink> : <span className="dw-dim">none: spans topics</span>}
          {" · "}
          {s.kind ? KIND_LABELS[s.kind] ?? s.kind : <span className="dw-dim">no kind</span>}
        </span>
        <span>Its notes</span>
        <span>
          {mix.error ? <span className="tone-text-red">could not be read: {mix.error.message}</span>
            : !m ? <span className="dw-dim">loading…</span>
            : m.notes === 0
              ? <span className="dw-dim">
                  no notes traced from {m.runs} run{m.runs === 1 ? "" : "s"}.{" "}
                  {m.source_contexts.length === 0 && "Its steps write no fixed source_context, so notes from before 2026-10-09 can't be traced to it; notes its runs write from now on are."}
                </span>
              : <>
                  {m.notes} traced from {m.runs} run{m.runs === 1 ? "" : "s"}
                  {flag && <span className={`tone-text-${flag.tone}`}> · {flag.text}</span>}
                  {m.verdict === "matches" && <span className="tone-text-green"> · matches</span>}
                  {m.verdict === "too_few" && <span className="dw-dim"> · too few classified to judge</span>}
                </>}
        </span>
      </div>
      {m && assessed > 0 && (
        <ul className="dw-list">
          {m.topics.map((t) => (
            <li key={t.topic}>
              <EntityLink kind="topic" id={t.topic}>{labels[t.topic] ?? t.topic}</EntityLink>
              <span className="dw-dim">{t.notes} · {Math.round((t.notes / assessed) * 100)}%</span>
              {t.topic === s.topic && <span className="dw-dim">declared</span>}
            </li>
          ))}
          {m.fits_none > 0 && <li><span className="dw-dim">fits no topic</span><span className="dw-dim">{m.fits_none}</span></li>}
        </ul>
      )}
      {m && m.notes > 0 && (
        <div className="dw-dim" title="Each note is counted once, by the first of these that reached it">
          traced by: {[
            m.routes.run_stamp && `${m.routes.run_stamp} run stamp`,
            m.routes.job_result && `${m.routes.job_result} job result`,
            m.routes.source_context && `${m.routes.source_context} source_context (${m.source_contexts.join(", ")})`,
            m.routes.claim_of && `${m.routes.claim_of} claims extracted from those`,
          ].filter(Boolean).join(" · ")}
        </div>
      )}
      {m && (m.waiting > 0 || m.skipped > 0) && (
        <div className="dw-dim">
          {m.waiting > 0 && `${m.waiting} not classified yet`}
          {m.waiting > 0 && m.skipped > 0 && " · "}
          {m.skipped > 0 && `${m.skipped} activity records, not classified by design`}
        </div>
      )}
    </Section>
  );
}

function YamlNotice({ name, plugin }: { name: string; plugin?: string | null }) {
  return (
    <EmptyState
      tone="yellow"
      title={`Defined in plugins/${plugin ?? "core"}/workflows/${name}.yaml`}
      checked="On every restart the file's definition (steps, description, schedule, inputs) replaces what is stored, so edits to those here last only until then. Turning it on or off, and Run now, are kept."
    />
  );
}
