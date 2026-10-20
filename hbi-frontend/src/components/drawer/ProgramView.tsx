/**
 * A program (PROGRAMS_PLAN.md Phase 2): its charter and success line, its
 * scorecard, the programs under it, its workflows and their state, its open
 * todos, and what is wrong with it. Edit, pause, resume, close (a cold case,
 * re-openable), reset to its file or delete it here; `name = "new"` creates one.
 */
import { useState } from "react";
import { ApiError } from "../../api/client";
import {
  deleteProgram, programTone, QUESTION_KINDS, resetProgram, savedNote, updateProgram, useProgram, useProgramAgents, useProgramScorecard,
  type ProgramDetail, type ProgramSaved,
} from "../../api/programs";
import { useWorkSchedules } from "../../api/work";
import { relTime } from "../../lib/time";
import { Drawer, EmptyState, Pill, Stat } from "../ui";
import { closeEntity, openEntity } from "./entities";
import ProgramForm from "./ProgramForm";
import { EntityLink, LoadState, Section } from "./parts";
import TodosFor from "./TodosFor";

function NewProgram() {
  return (
    <Drawer title="New program" onClose={closeEntity} wide>
      <div className="dw-note">
        A program owns workflows and says what they are for. Pausing or closing it later pauses its workflows and those of the
        programs under it.
      </div>
      <ProgramForm existing={null} onCancel={closeEntity} onSaved={(n) => openEntity("program", n)} />
    </Drawer>
  );
}

export default function ProgramView({ name }: { name: string }) {
  if (name === "new") return <NewProgram />;
  return <OneProgram name={name} />;
}

function OneProgram({ name }: { name: string }) {
  const q = useProgram(name);
  const score = useProgramScorecard(30);
  const agents = useProgramAgents(name, 30);
  const schedules = useWorkSchedules();
  const [editing, setEditing] = useState(false);
  const [closing, setClosing] = useState(false);
  const [finding, setFinding] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const act = async (f: () => Promise<string>) => {
    setBusy(true);
    setMsg(null);
    try {
      setMsg({ ok: true, text: await f() });
    } catch (e) {
      setMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };
  const p = q.data;
  if (!p) {
    const missing = q.error instanceof ApiError && q.error.status === 404;
    return (
      <Drawer title="Program" onClose={closeEntity}>
        {missing
          ? <EmptyState tone="yellow" title="No program with this name" checked={<>Looked for <code>{name}</code>. It may have been deleted or renamed.</>} />
          : <LoadState what="program" id={name} error={q.error} onRetry={() => q.refetch()} />}
      </Drawer>
    );
  }
  const s = score.data?.rows.find((r) => r.program === name);
  const idOf = new Map((schedules.data?.workflows ?? []).map((w) => [w.name, w.id]));
  const question = QUESTION_KINDS.includes(p.kind);
  const setStatus = (status: "active" | "paused" | "closed", extra: Partial<ProgramDetail> = {}) =>
    act(async () => savedNote(await updateProgram(p.name, { status, ...extra })));

  return (
    <Drawer title={<><span>Program</span><Pill tone="accent">{p.name}</Pill><Pill tone={programTone(p.status)}>{p.status}</Pill></>} onClose={closeEntity} wide>
      <h3 className="dw-title">{p.label}</h3>
      <div className="dw-dim">
        {p.kind}
        {p.parent && <> · under <EntityLink kind="program" id={p.parent}>{p.parent}</EntityLink></>}
        {p.status_changed_at && <> · {p.status} {relTime(p.status_changed_at)}</>}
        {p.review?.every && <> · reviewed every {p.review.every}</>}
      </div>

      {editing ? (
        <ProgramForm
          existing={p}
          onCancel={() => setEditing(false)}
          onSaved={(_, r: ProgramSaved) => { setEditing(false); setMsg({ ok: true, text: savedNote(r) }); }}
        />
      ) : (
        <>
          {p.charter ? <div className="dw-note">{p.charter}</div> : <div className="dw-note wk-dim">No charter yet: say what it is for.</div>}
          {p.success && <div className="dw-note"><b>Success:</b> {p.success}</div>}
          {p.status === "closed" && p.finding && <div className="dw-note"><b>Finding:</b> {p.finding}</div>}
        </>
      )}

      <div className="dw-dim">
        {p.managed_by === "runtime"
          ? <>Edited here{p.updated_at ? ` ${relTime(p.updated_at)}` : ""}{p.source ? <>; it shadows <code>{p.source}</code>, which no longer applies</> : "; no file declares it"}.</>
          : <>Defined in <code>{p.source ?? "programs/*.yaml"}</code>; an edit here takes over from the file (a status change does not).</>}
      </div>

      {!editing && (
        <div className="item-actions">
          <button type="button" className="btn-ghost" disabled={busy} onClick={() => setEditing(true)}>Edit</button>
          {p.status === "active" && (
            <button type="button" className="btn-ghost" disabled={busy}
              title="Pauses its workflows and those of the programs under it, except those kept running"
              onClick={() => setStatus("paused")}>Pause</button>
          )}
          {p.status === "paused" && (
            <button type="button" className="btn-ghost" disabled={busy} onClick={() => setStatus("active")}>Resume</button>
          )}
          {p.status !== "closed" && (
            <button type="button" className="btn-ghost" disabled={busy}
              title="A closed program keeps its history, notes and workflows (off): a cold case you can reopen"
              onClick={() => setClosing((c) => !c)}>Close…</button>
          )}
          {p.status === "closed" && (
            <button type="button" className="btn-ghost" disabled={busy} title="Its workflows come back on"
              onClick={() => setStatus("active")}>Reopen</button>
          )}
          {p.managed_by === "runtime" && p.source && (
            <button type="button" className="btn-ghost" disabled={busy}
              onClick={() => act(async () => savedNote(await resetProgram(p.name)))}>Reset to file</button>
          )}
          <button type="button" className="btn-ghost" disabled={busy || p.children.length > 0}
            title={p.children.length ? "Programs sit under it: move or delete them first" : "Its workflows are released (and resumed, if its pause held them)"}
            onClick={() => {
              if (!window.confirm(`Delete the program "${p.label}"? Its workflows stay; they just belong to no program.`)) return;
              act(async () => { await deleteProgram(p.name); closeEntity(); return "Deleted."; });
            }}>Delete…</button>
          {msg && <span className={msg.ok ? "tone-text-green" : "tone-text-red"} style={{ fontSize: 11, whiteSpace: "pre-wrap" }}>{msg.text}</span>}
        </div>
      )}
      {closing && p.status !== "closed" && (
        <div className="item-hand">
          <label>
            <span className="wk-dim">
              {question ? "What it found (required to close an investigation or case)" : "Why it closes (optional)"}. Its workflows go off; reopening brings them back.
            </span>
            <textarea className="ui-input" rows={3} value={finding} onChange={(e) => setFinding(e.target.value)} />
          </label>
          <div className="item-actions">
            <button type="button" className="btn" disabled={busy || (question && !finding.trim())}
              onClick={() => { setStatus("closed", finding.trim() ? { finding: finding.trim() } : {}); setClosing(false); }}>Close it</button>
            <button type="button" className="btn-ghost" onClick={() => setClosing(false)}>Cancel</button>
          </div>
        </div>
      )}

      {p.issues.length > 0 && (
        <EmptyState tone="yellow" title="Worth a look" checked={<ul className="dw-list">{p.issues.map((i) => <li key={i}>{i}</li>)}</ul>} />
      )}

      <Section title="Last 30 days" aside={p.children.length ? "with the programs under it" : undefined}>
        {s ? (
          <div className="ui-stats">
            <Stat label="Runs" value={s.runs.toLocaleString()} tone={s.failed ? "yellow" : undefined} title={`${s.ok} succeeded · ${s.failed} failed`} />
            <Stat label="Notes" value={s.notes.toLocaleString()} />
            <Stat label="Read again" value={s.reads.toLocaleString()} />
            <Stat label="Cited" value={s.cited.toLocaleString()} />
            <Stat label="Cost" value={s.cost_usd == null ? "—" : `$${s.cost_usd.toFixed(2)}`} title="Recorded from 2026-10-17" />
            {s.budget_monthly_usd != null && <Stat label="Budget / month" value={`$${s.budget_monthly_usd.toFixed(2)}`} title="Shown, not enforced yet" />}
          </div>
        ) : <div className="loading">{score.error ? score.error.message : "Loading the scorecard…"}</div>}
      </Section>

      {p.children.length > 0 && (
        <Section title="Under it" aside={`${p.children.length}`}>
          <ul className="dw-list">
            {p.children.map((c) => <li key={c}><EntityLink kind="program" id={c}>{c}</EntityLink></li>)}
          </ul>
        </Section>
      )}

      <Section title="Workflows" aside={`${p.workflow_state.filter((w) => w.enabled).length} of ${p.workflow_state.length} on`}>
        {p.workflow_state.length === 0 ? (
          <div className="dw-note">It owns no workflows{p.children.length ? "; the programs under it do" : ""}.</div>
        ) : (
          <ul className="dw-list">
            {p.workflow_state.map((w) => (
              <li key={w.name}>
                {idOf.get(w.name) ? <EntityLink kind="schedule" id={idOf.get(w.name)!}>{w.name}</EntityLink> : <span>{w.name}</span>}
                {w.missing ? <Pill tone="red">missing</Pill> : <Pill tone={w.enabled ? "green" : "muted"}>{w.enabled ? "on" : "off"}</Pill>}
                {(p.keep_running ?? []).includes(w.name) && <span className="dw-dim">kept running</span>}
                {w.paused_reason && <span className="dw-dim dw-clip" title={w.paused_reason}>{w.paused_reason}</span>}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Agents its work uses" aside="last 30 days">
        {!agents.data ? (
          <div className="loading">{agents.error ? agents.error.message : "Loading the agents…"}</div>
        ) : agents.data.rows.length === 0 ? (
          <div className="dw-note">No workflow of this program calls an agent, and it names none it relies on.</div>
        ) : (
          <ul className="dw-list">
            {agents.data.rows.map((a) => (
              <li key={a.agent} title={a.called_by_workflows.length ? `Called by ${a.called_by_workflows.join(", ")}` : undefined}>
                <span>{a.agent}</span>
                <span className="dw-dim">
                  {a.runs != null ? `${a.answered ?? 0}/${a.runs} answered` : "no runs recorded"}
                  {a.tools_failed ? ` · ${a.tools_failed}/${a.tool_calls} tool calls failed` : ""}
                  {a.model_errors ? ` · ${a.model_errors} model errors` : ""}
                  {a.arg_misfits ? ` · ${a.arg_misfits} misfit arguments` : ""}
                  {a.model_calls != null ? ` · ${a.model_calls} model calls` : ""}
                  {a.cost_usd != null ? ` · $${a.cost_usd.toFixed(2)}` : ""}
                </span>
              </li>
            ))}
          </ul>
        )}
        {agents.data?.sql_error && <div className="dw-note tone-text-yellow">Agent numbers could not be read: {agents.data.sql_error}</div>}
      </Section>

      {((p.topics ?? []).length > 0 || (p.agents ?? []).length > 0 || (p.apis ?? []).length > 0) && (
        <Section title="Covers and relies on">
          {(p.topics ?? []).length > 0 && (
            <div className="dw-chips">
              {(p.topics ?? []).map((t) => <EntityLink key={t} kind="topic" id={t}><Pill tone="muted">{t}</Pill></EntityLink>)}
            </div>
          )}
          {(p.agents ?? []).length > 0 && <div className="dw-dim">Agents: {(p.agents ?? []).join(", ")}</div>}
          {(p.apis ?? []).length > 0 && <div className="dw-dim">APIs: {(p.apis ?? []).join(", ")}</div>}
        </Section>
      )}

      <Section title="Open todos" aside={p.open_todos.length ? `${p.open_todos.length}` : undefined}>
        {p.open_todos.length === 0 ? (
          <div className="dw-note">None. An item a workflow of this program makes lands here on its own.</div>
        ) : (
          <ul className="dw-list">
            {p.open_todos.map((t) => (
              <li key={t.id}>
                <EntityLink kind="todo" id={t.id}>{t.title}</EntityLink>
                <span className="dw-dim">{t.status.replaceAll("_", " ")}</span>
              </li>
            ))}
          </ul>
        )}
      </Section>
      <TodosFor tag={`program:${name}`} what="program" />
    </Drawer>
  );
}
