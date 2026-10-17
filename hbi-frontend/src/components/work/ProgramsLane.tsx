/**
 * Programs (PROGRAMS_PLAN.md Phase 2): the tree of departments, projects,
 * investigations and cases, each row with its scorecard over a window. A
 * parent's numbers include the programs under it. Click a row to open, edit,
 * pause or close it; "+ New program" creates one.
 */
import { useState } from "react";
import { programTone, usePrograms, useProgramScorecard, type ProgramRow, type ProgramScore } from "../../api/programs";
import { useWorkSchedules } from "../../api/work";
import { openEntity } from "../drawer/entities";
import { EmptyState, Pill, Tabs } from "../ui";

const WINDOWS = [7, 30, 90] as const;

/** Cost is recorded from 2026-10-17: null means no ledger rows, not $0. */
function money(v: number | null | undefined): string {
  if (v === null || v === undefined) return "—";
  return v === 0 ? "$0" : v < 0.01 ? "<$0.01" : `$${v.toFixed(2)}`;
}

export default function ProgramsLane({ openId }: { openId?: string }) {
  const [days, setDays] = useState<number>(30);
  const tree = usePrograms();
  const score = useProgramScorecard(days);
  const schedules = useWorkSchedules();
  const idOf = new Map((schedules.data?.workflows ?? []).map((w) => [w.name, w.id]));
  const byName = new Map((score.data?.rows ?? []).map((r) => [r.program, r]));

  return (
    <div className="wk-lane-frame">
      <div className="wk-lane-bar">
        <span className="wk-dim" style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
          last
          <Tabs tabs={WINDOWS.map((d) => ({ id: String(d), label: `${d} days` }))} active={String(days)} onChange={(id) => setDays(Number(id))} />
        </span>
        <button type="button" className="btn" onClick={() => openEntity("program", "new")}>+ New program</button>
      </div>
      <div className="wk-lane scroll">
        <div className="wk-hint">
          A program owns workflows; a parent's row counts the programs under it. Reads and cited are thin signs of value (notes read
          again, notes a claim points at). Cost is recorded from 2026-10-17, so — means no recorded calls yet, not free.
        </div>
        {!tree.data ? (
          tree.error ? <EmptyState tone="red" title="Couldn't load programs" checked={tree.error.message} /> : <div className="loading">Loading programs…</div>
        ) : tree.data.programs.length === 0 ? (
          <EmptyState title="No programs yet" checked="Programs are seeded from programs/*.yaml in the plugin bundles, or created here." />
        ) : (
          <table className="reg-table prog-table">
            <thead>
              <tr>
                <th>Program</th>
                <th>Kind</th>
                <th>Status</th>
                <th title="Workflows on / owned (with the programs under it)">Workflows</th>
                <th>Runs</th>
                <th>Notes</th>
                <th title="Times its notes were read again">Reads</th>
                <th title="Notes a claim points at">Cited</th>
                <th title="Model cost of its runs in the window">Cost</th>
                <th title="Monthly cap (shown, not enforced yet)">Budget</th>
              </tr>
            </thead>
            <tbody>
              {tree.data.programs.map((p) => (
                <ProgramLine key={p.name} p={p} s={byName.get(p.name)} active={openId === p.name} />
              ))}
            </tbody>
          </table>
        )}
        {score.data?.sql_error && <EmptyState tone="yellow" title="Cost could not be read" checked={score.data.sql_error} />}
        {score.error && <EmptyState tone="yellow" title="The scorecard could not be read" checked={score.error.message} />}
        {tree.data && tree.data.unassigned_enabled_workflows.length > 0 && (
          <EmptyState
            tone="yellow"
            title={`${tree.data.unassigned_enabled_workflows.length} enabled workflows are in no program`}
            checked={
              <>
                {tree.data.unassigned_enabled_workflows.map((w, i) => (
                  <span key={w}>
                    {i > 0 && ", "}
                    {idOf.get(w) ? <button type="button" className="dw-link" onClick={() => openEntity("schedule", idOf.get(w)!)}>{w}</button> : w}
                  </span>
                ))}
                . Add each to a program (or to Shared services) so someone is accountable for it.
              </>
            }
          />
        )}
      </div>
    </div>
  );
}

function ProgramLine({ p, s, active }: { p: ProgramRow; s?: ProgramScore; active: boolean }) {
  return (
    <tr className={`prog-row${active ? " active" : ""}`} onClick={() => openEntity("program", p.name)}>
      <td>
        <span style={{ paddingLeft: p.depth * 18 }}>
          {p.depth > 0 && <span className="wk-dim">└ </span>}
          <span className="wk-row-title">{p.label}</span>
          {p.issues.length > 0 && <span className="tone-text-yellow" title={p.issues.join("\n")}> ⚠</span>}
          {p.managed_by === "runtime" && <span className="wk-dim" title="Edited here; its file no longer applies"> · edited</span>}
        </span>
      </td>
      <td className="wk-dim">{p.kind}</td>
      <td><Pill tone={programTone(p.status)}>{p.status}</Pill></td>
      <td>{s ? `${s.enabled}/${s.workflows}` : `${p.enabled_workflows}/${p.workflows}`}</td>
      <td>
        {s ? s.runs.toLocaleString() : "…"}
        {s && s.failed > 0 && <span className="tone-text-red" title={`${s.failed} failed or dead`}> ({s.failed}✗)</span>}
      </td>
      <td>{s ? s.notes.toLocaleString() : "…"}</td>
      <td>{s ? s.reads.toLocaleString() : "…"}</td>
      <td>{s ? s.cited.toLocaleString() : "…"}</td>
      <td title={s?.model_calls ? `${s.model_calls} model calls · ${(s.tokens_in ?? 0).toLocaleString()} tokens in` : undefined}>{s ? money(s.cost_usd) : "…"}</td>
      <td className="wk-dim">{s?.budget_monthly_usd != null ? `${money(s.budget_monthly_usd)}/mo` : "—"}</td>
    </tr>
  );
}
