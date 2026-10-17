/**
 * Create or edit a program (PROGRAMS_PLAN.md Phase 2). The server checks the
 * whole program on save (services/programs.rs) and refuses with what would
 * fit; this form only keeps the obvious mistakes out. Status is changed from
 * the drawer's actions, not here. "Ask an agent" drafts the program from a
 * prompt (program-manager by default, read-only); the draft goes through the
 * same check as a save, once more to the agent if refused, then into the form.
 */
import { useMemo, useState, type FormEvent } from "react";
import {
  checkProgram, createProgram, PROGRAM_KINDS, PROGRAM_NAME, TEXT_MAX, updateProgram, usePrograms, useProgramSchema,
  type ProgramCheck, type ProgramDef, type ProgramSaved,
} from "../../api/programs";
import { answerBody, runAssist, type AssistRun } from "../../lib/assist";
import { buildProgramTask, readProgramDraft } from "../../lib/programDraft";
import AgentAssist from "../ui/AgentAssist";
import { useProfiles } from "../../api/queries";
import { useTopics } from "../../api/topics";
import { useWorkSchedules } from "../../api/work";

function Checks({ options, chosen, onChange, disabled }: {
  options: { value: string; label?: string; note?: string }[];
  chosen: string[];
  onChange: (next: string[]) => void;
  disabled?: (value: string) => string | undefined;
}) {
  return (
    <div className="prog-checks">
      {options.map((o) => {
        const why = disabled?.(o.value);
        const on = chosen.includes(o.value);
        return (
          <label key={o.value} className={`wk-check${why ? " prog-taken" : ""}`} title={why ?? o.note}>
            <input type="checkbox" checked={on} disabled={!!why && !on}
              onChange={(e) => onChange(e.target.checked ? [...chosen, o.value] : chosen.filter((c) => c !== o.value))} />
            {o.label ?? o.value}
            {why && <span className="wk-dim"> ({why})</span>}
          </label>
        );
      })}
    </div>
  );
}

const list = (s: string) => s.split(",").map((x) => x.trim()).filter(Boolean);

export default function ProgramForm({ existing, onSaved, onCancel }: {
  existing: ProgramDef | null;
  onSaved: (name: string, result: ProgramSaved) => void;
  onCancel: () => void;
}) {
  const tree = usePrograms();
  const topics = useTopics();
  const profiles = useProfiles();
  const schedules = useWorkSchedules();

  const [name, setName] = useState(existing?.name ?? "");
  const [label, setLabel] = useState(existing?.label ?? "");
  const [kind, setKind] = useState<string>(existing?.kind ?? "department");
  const [parent, setParent] = useState(existing?.parent ?? "");
  const [charter, setCharter] = useState(existing?.charter ?? "");
  const [success, setSuccess] = useState(existing?.success ?? "");
  const [topicSel, setTopicSel] = useState<string[]>(existing?.topics ?? []);
  const [workflows, setWorkflows] = useState<string[]>(existing?.workflows ?? []);
  const [keep, setKeep] = useState<string[]>(existing?.keep_running ?? []);
  const [agents, setAgents] = useState<string[]>(existing?.agents ?? []);
  const [apis, setApis] = useState((existing?.apis ?? []).join(", "));
  const [review, setReview] = useState(existing?.review?.every ?? "");
  const [budget, setBudget] = useState(existing?.budget ? String(existing.budget.monthly_usd) : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const rows = useMemo(() => tree.data?.programs ?? [], [tree.data]);
  // A program can't sit under itself or anything below it.
  const below = useMemo(() => {
    const out = new Set<string>();
    if (!existing) return out;
    const walk = (n: string) => {
      out.add(n);
      rows.filter((r) => r.parent === n).forEach((r) => walk(r.name));
    };
    walk(existing.name);
    return out;
  }, [rows, existing]);
  const owner = useMemo(() => {
    const m = new Map<string, { name: string; label: string }>();
    rows.forEach((r) => r.workflow_names.forEach((w) => m.set(w, { name: r.name, label: r.label })));
    return m;
  }, [rows]);
  // Its own and the free ones first; another program's last (shown, disabled).
  const allWorkflows = useMemo(() => {
    const free = (n: string) => !owner.has(n) || owner.get(n)?.name === existing?.name;
    return [...(schedules.data?.workflows ?? [])].sort((a, b) =>
      Number(free(b.name)) - Number(free(a.name)) || Number(b.enabled) - Number(a.enabled) || a.name.localeCompare(b.name));
  }, [schedules.data, owner, existing]);

  const schema = useProgramSchema();

  /** The form as a save body: what Save sends, what a draft is checked as. */
  const formBody = (): Partial<ProgramDef> => ({
    ...(existing ? { name: existing.name } : name ? { name } : {}),
    label: label.trim(),
    kind,
    parent: parent || null,
    charter: charter.trim(),
    success: success.trim(),
    topics: topicSel,
    workflows,
    keep_running: keep.filter((k) => workflows.includes(k)),
    agents,
    apis: list(apis),
    review: review.trim() ? { every: review.trim() } : null,
    budget: budget.trim() ? { monthly_usd: Number(budget) } : null,
  });

  const assistTask = (prompt: string, fix?: { previous: string; errors: string }) => {
    const own = new Set(existing?.workflows ?? []);
    return buildProgramTask({
      request: prompt,
      current: formBody(),
      schema: schema.data ?? "(the schema could not be loaded: follow the fields of the form as it stands)",
      programs: rows.filter((r) => r.name !== existing?.name).map((r) => ({ name: r.name, label: r.label, kind: r.kind, parent: r.parent })),
      freeWorkflows: allWorkflows.filter((w) => !owner.has(w.name) || own.has(w.name)),
      topics: (topics.data?.topics ?? []).map((t) => ({ name: t.name, label: t.label })),
      agents: (profiles.data ?? []).map((p) => p.name),
      fix,
    });
  };

  const applyDraft = (d: Partial<ProgramDef>) => {
    if (!existing && typeof d.name === "string") setName(d.name.toLowerCase());
    if (typeof d.label === "string") setLabel(d.label);
    if (typeof d.kind === "string") setKind(d.kind);
    if (d.parent !== undefined) setParent(d.parent ?? "");
    if (typeof d.charter === "string") setCharter(d.charter);
    if (typeof d.success === "string") setSuccess(d.success);
    if (Array.isArray(d.topics)) setTopicSel(d.topics);
    if (Array.isArray(d.workflows)) setWorkflows(d.workflows);
    if (Array.isArray(d.keep_running)) setKeep(d.keep_running);
    if (Array.isArray(d.agents)) setAgents(d.agents);
    if (Array.isArray(d.apis)) setApis(d.apis.join(", "));
    if (d.review !== undefined) setReview(d.review?.every ?? "");
    if (d.budget !== undefined) setBudget(d.budget ? String(d.budget.monthly_usd) : "");
  };

  const applyAssist = async (body: string, run: AssistRun, _action: string | undefined, prompt: string): Promise<string | null> => {
    const create = !existing;
    const checkDraft = (d: Partial<ProgramDef>) =>
      checkProgram({ ...d, ...(existing ? { name: existing.name } : {}) }, create);
    const askAgain = async (previous: string, errors: string) =>
      readProgramDraft(answerBody((await runAssist(run.agent || "program-manager", assistTask(prompt, { previous, errors }), run.requestedModel ?? "")).answer));
    // No usable draft, or one the save check refuses, goes back to the agent
    // once with what was wrong, as the workflow form does.
    let read = readProgramDraft(body);
    let retried = false;
    if (typeof read === "string") {
      read = await askAgain(body.slice(0, 2000), read);
      retried = true;
      if (typeof read === "string") return read;
    }
    let check: ProgramCheck = await checkDraft(read.draft);
    if (!check.ok && check.refused && !retried) {
      const again = await askAgain(read.text, check.error);
      if (typeof again !== "string") {
        read = again;
        check = await checkDraft(read.draft);
      }
    }
    applyDraft(read.draft);
    if (!check.ok) {
      return check.refused
        ? `The draft is in the form, but Save will refuse it until these are fixed:\n${check.error}`
        : `The draft is in the form, unchecked: ${check.error}`;
    }
    return check.warnings.length ? `The draft is in the form and will save. Worth knowing: ${check.warnings.join(" ")}` : null;
  };

  const nameOk = PROGRAM_NAME.test(name);
  const budgetOk = budget.trim() === "" || (Number.isFinite(Number(budget)) && Number(budget) >= 0);
  const tooLong = [label, charter, success].some((t) => t.length > TEXT_MAX);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const body = formBody();
    try {
      const r = existing ? await updateProgram(existing.name, body) : await createProgram({ ...body, name, kind } as ProgramDef);
      onSaved(r.program.name, r);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="item-hand" onSubmit={submit}>
      <AgentAssist
        title={existing ? "Ask an agent to revise this program" : "Ask an agent to draft the program"}
        defaultAgent="program-manager"
        placeholder={existing
          ? "What should change, e.g. 'take over the grid-outage workflow and add a budget of $2 a month'."
          : "Describe the program, e.g. 'An investigation into who supplies HBM memory to Chinese labs, under the NIA'."}
        buildTask={(prompt) => assistTask(prompt)}
        onResult={applyAssist}
        runLabel="Draft"
        note="Fills the form; nothing is saved until you press Save"
      />
      <label>
        <span className="wk-dim">Name (its identity: lowercase, digits and hyphens; can't change later)</span>
        <input className="ui-input" value={name} disabled={!!existing} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="e.g. supply-chain-shocks" />
        {!existing && name && !nameOk && <span className="tone-text-red" style={{ fontSize: 11 }}>lowercase letters, digits and hyphens, starting with a letter or digit</span>}
      </label>
      <label>
        <span className="wk-dim">Label (what the HBI shows)</span>
        <input className="ui-input" value={label} maxLength={80} onChange={(e) => setLabel(e.target.value)} />
      </label>
      <div className="prog-pair">
        <label>
          <span className="wk-dim">Kind</span>
          <select className="ui-input auto" value={kind} onChange={(e) => setKind(e.target.value)}>
            {PROGRAM_KINDS.map((k) => <option key={k} value={k}>{k}</option>)}
          </select>
        </label>
        <label>
          <span className="wk-dim">Under</span>
          <select className="ui-input auto" value={parent} onChange={(e) => setParent(e.target.value)}>
            <option value="">— top level —</option>
            {rows.filter((r) => !below.has(r.name)).map((r) => (
              <option key={r.name} value={r.name}>{"  ".repeat(r.depth)}{r.label}</option>
            ))}
          </select>
        </label>
      </div>
      <span className="wk-dim" style={{ fontSize: 11 }}>
        department: standing work · project: a deliverable · investigation / case: a question that closes with a finding · services: on-demand workflows every program uses
      </span>
      <label>
        <span className="wk-dim">Charter: what it is for, in a few sentences (its manager reads this)</span>
        <textarea className="ui-input" rows={3} value={charter} onChange={(e) => setCharter(e.target.value)} />
      </label>
      <label>
        <span className="wk-dim">Success: what good output looks like (the manager judges against it)</span>
        <textarea className="ui-input" rows={2} value={success} onChange={(e) => setSuccess(e.target.value)} />
        {tooLong && <span className="tone-text-red" style={{ fontSize: 11 }}>Keep each under {TEXT_MAX} characters</span>}
      </label>
      <div>
        <span className="wk-dim">Workflows it owns (a workflow has one program)</span>
        <Checks
          options={allWorkflows.map((w) => ({ value: w.name, label: `${w.name}${w.enabled ? "" : " (off)"}`, note: w.description ?? undefined }))}
          chosen={workflows}
          onChange={setWorkflows}
          disabled={(w) => {
            const o = owner.get(w);
            return o && o.name !== existing?.name ? `in ${o.label}` : undefined;
          }}
        />
      </div>
      {workflows.length > 0 && (
        <div>
          <span className="wk-dim">Keep running when the program is paused or closed</span>
          <Checks options={workflows.map((w) => ({ value: w }))} chosen={keep} onChange={setKeep} />
        </div>
      )}
      <div>
        <span className="wk-dim">Topics it covers</span>
        <Checks options={(topics.data?.topics ?? []).map((t) => ({ value: t.name, label: t.label }))} chosen={topicSel} onChange={setTopicSel} />
      </div>
      <div>
        <span className="wk-dim">Agents its work relies on</span>
        <Checks options={(profiles.data ?? []).map((p) => ({ value: p.name }))} chosen={agents} onChange={setAgents} />
      </div>
      <label>
        <span className="wk-dim">APIs and MCP servers it relies on (comma-separated names)</span>
        <input className="ui-input" value={apis} onChange={(e) => setApis(e.target.value)} placeholder="e.g. aidevboard, jobs-board-remoteok" />
      </label>
      <div className="prog-pair">
        <label>
          <span className="wk-dim">Review every</span>
          <input className="ui-input" value={review} onChange={(e) => setReview(e.target.value)} placeholder="1w" />
        </label>
        <label>
          <span className="wk-dim">Monthly budget, USD (shown, not enforced yet)</span>
          <input className="ui-input" inputMode="decimal" value={budget} onChange={(e) => setBudget(e.target.value)} placeholder="none" />
          {!budgetOk && <span className="tone-text-red" style={{ fontSize: 11 }}>a number of dollars, 0 or more</span>}
        </label>
      </div>
      {error && <div className="tone-text-red" style={{ fontSize: 12, whiteSpace: "pre-wrap" }}>{error}</div>}
      <div className="item-actions">
        <button type="submit" className="btn" disabled={busy || !budgetOk || tooLong || (!existing && !nameOk)}>
          {busy ? "Saving…" : existing ? "Save" : "Create program"}
        </button>
        <button type="button" className="btn-ghost" disabled={busy} onClick={onCancel}>Cancel</button>
      </div>
    </form>
  );
}
