/**
 * Settings: every configurable thing in one place (`#/settings?section=…`).
 *
 * Before Phase 1 these were scattered: connection in a modal, agents in a
 * Chat modal, scheduler on/off in Architecture and its config inside the
 * schedule editor. Ctrl-K jumps to any section.
 */
import { useEffect, useMemo, useState } from "react";
import { apiJson } from "../../api/client";
import { getApiKey, getBrainUrl } from "../../api/config";
import { callToolStrict, resetMcpClient } from "../../api/mcp";
import {
  keys,
  queryClient,
  useLlmBudget,
  useProfiles,
  useSchedulerStatus,
  type SchedulerStatus,
} from "../../api/queries";
import { summariseScheduler } from "../../lib/scheduler";
import { formatLocal, relTime } from "../../lib/time";
import { goTo } from "../../nav";
import AgentsEditor from "./AgentsEditor";
import { EmptyState, Pill } from "../ui";
import { SETTINGS_SECTIONS } from "./sections";

export default function SettingsPage({ section }: { section?: string }) {
  const [filter, setFilter] = useState("");
  const active = SETTINGS_SECTIONS.find((s) => s.id === section) ?? SETTINGS_SECTIONS[0];
  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return q
      ? SETTINGS_SECTIONS.filter((s) => `${s.label} ${s.keywords}`.toLowerCase().includes(q))
      : SETTINGS_SECTIONS;
  }, [filter]);

  return (
    <div className="settings">
      <nav className="settings-nav" aria-label="Settings sections">
        <input className="ui-input" placeholder="Filter settings…" value={filter} onChange={(e) => setFilter(e.target.value)} />
        {visible.map((s) => (
          <button
            key={s.id}
            type="button"
            className={s.id === active.id ? "active" : ""}
            onClick={() => goTo("settings", { section: s.id })}
          >
            {s.label}
          </button>
        ))}
        {visible.length === 0 && <div className="home-dim">No section matches “{filter}”.</div>}
      </nav>
      <div className="settings-body">
        {active.id === "connection" && <ConnectionSection />}
        {active.id === "agents" && <AgentsSection />}
        {active.id === "models" && <ModelsSection />}
        {active.id === "scheduler" && <SchedulerSection />}
        {active.id === "about" && <AboutSection />}
      </div>
    </div>
  );
}

// ── Connection ───────────────────────────────────────────────────────────────

function ConnectionSection() {
  const [brainUrl, setBrainUrl] = useState(getBrainUrl);
  const [apiKey, setApiKey] = useState(getApiKey);
  const [state, setState] = useState<{ ok: boolean; text: string } | null>(null);
  const [checking, setChecking] = useState(false);

  const check = async () => {
    setChecking(true);
    try {
      const h = await apiJson<{ queue?: { coordinator?: { alive: boolean } } }>("/api/health");
      setState({ ok: true, text: `Connected. Queue coordinator ${h.queue?.coordinator?.alive ? "alive" : "NOT alive"}.` });
    } catch (e) {
      setState({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setChecking(false);
    }
  };

  useEffect(() => { check(); }, []);

  const save = async () => {
    try {
      localStorage.setItem("brain_url", brainUrl);
      localStorage.setItem("api_key", apiKey);
    } catch {
      setState({ ok: false, text: "This browser blocked localStorage; the settings could not be saved." });
      return;
    }
    resetMcpClient(); // the next tool call reconnects with the new values
    queryClient.invalidateQueries();
    await check();
  };

  return (
    <>
      <h2>Connection</h2>
      <p className="settings-lede">
        Where this browser finds the brain. Stored in this browser's localStorage; leave the URL blank to use the
        page's own origin (the nginx / Vite proxy).
      </p>
      <div className="settings-form">
        <label htmlFor="brain-url">Brain URL</label>
        <input id="brain-url" className="ui-input" value={brainUrl} onChange={(e) => setBrainUrl(e.target.value)}
          placeholder="(same origin)" spellCheck={false} />
        <span className="hint">e.g. http://localhost:3001</span>
        <label htmlFor="api-key">API key</label>
        <input id="api-key" className="ui-input" type="password" value={apiKey} onChange={(e) => setApiKey(e.target.value)}
          spellCheck={false} />
        <span className="hint">MCP_API_KEY on the brain</span>
      </div>
      <div className="settings-actions">
        <button type="button" className="btn" onClick={save}>Save &amp; reconnect</button>
        <button type="button" className="btn-ghost" onClick={check} disabled={checking}>{checking ? "Checking…" : "Test"}</button>
        {state && <span className={state.ok ? "tone-text-green" : "tone-text-red"}>{state.text}</span>}
      </div>
    </>
  );
}

// ── Agents ───────────────────────────────────────────────────────────────────

function AgentsSection() {
  const profiles = useProfiles();
  return (
    <>
      <h2>Agents &amp; profiles</h2>
      <p className="settings-lede">
        Each agent is a context profile: a tool allowlist, a system prompt and an optional model preference. Chat's
        agent picker uses this same list; choose which agent a conversation uses from Chat.
      </p>
      {profiles.isError ? (
        <EmptyState tone="red" title="Couldn't load agents" checked={profiles.error.message} />
      ) : (
        <AgentsEditor
          inline
          isOpen
          onClose={() => {}}
          activeProfile=""
          profiles={profiles.data ?? []}
          onProfileChange={() => {}}
          onProfilesChanged={() => queryClient.invalidateQueries({ queryKey: keys.profiles })}
        />
      )}
    </>
  );
}

// ── Models & routing ─────────────────────────────────────────────────────────

const BILLING_TONE = { local: "green", free: "accent", paid: "yellow" } as const;

function ModelsSection() {
  const budget = useLlmBudget(30_000);
  if (!budget.data) {
    return (
      <>
        <h2>Models &amp; routing</h2>
        {budget.error
          ? <EmptyState tone="red" title="Couldn't load the provider budget" checked={budget.error.message} />
          : <div className="loading">Loading providers…</div>}
      </>
    );
  }
  const b = budget.data;
  return (
    <>
      <h2>Models &amp; routing</h2>
      <p className="settings-lede">
        Read-only. Providers, limits and spend caps come from <code>providers:</code> in <code>models.yaml</code>; keys
        come from the brain's environment. Private work (the default) only reaches <code>no_log</code> providers, or a
        provider whose <code>private_ok_for</code> names the purpose. Day counters reset at {b.day_resets_at}.
        {b.gpu_lease.held && <strong className="tone-text-yellow"> A GPU lease is held: local providers are refused until it is released.</strong>}
      </p>
      <table className="settings-table">
        <thead>
          <tr>
            <th>Provider</th><th>Billing</th><th>Data policy</th><th>Key</th><th>Today</th><th>Spend</th><th>Limits</th>
          </tr>
        </thead>
        <tbody>
          {b.providers.map((p) => {
            const cooling = (p.models ?? []).filter((m) => m.cooling_down);
            const conc = p.concurrency as { in_use?: number; capacity?: number } | null | undefined;
            return (
              <tr key={p.name}>
                <td>
                  <strong>{p.name}</strong> <span className="home-dim">{p.kind}</span>
                  {!p.enabled && <> <Pill tone="muted">disabled</Pill></>}
                  {cooling.map((m) => (
                    <div key={m.model} className="tone-text-yellow">
                      {m.model} cooling down until {formatLocal(m.cooldown_until)}{m.cooldown_reason ? ` (${m.cooldown_reason})` : ""}
                    </div>
                  ))}
                </td>
                <td><Pill tone={BILLING_TONE[p.billing as keyof typeof BILLING_TONE] ?? "muted"}>{p.billing}</Pill></td>
                <td>
                  <span className={p.data_policy === "no_log" ? "tone-text-green" : "tone-text-yellow"}>{p.data_policy}</span>
                  {p.private_ok_for.length > 0 && <div className="home-dim">private OK for: {p.private_ok_for.join(", ")}</div>}
                </td>
                <td>
                  {p.api_key_env
                    ? <span className={p.key_configured ? "tone-text-green" : "tone-text-red"} title={p.api_key_env}>{p.key_configured ? "set" : "missing"}</span>
                    : <span className="home-dim">none needed</span>}
                </td>
                <td>{p.today ? <>{p.today.requests} req · {Math.round(p.today.tokens / 1000)}k tok</> : "—"}</td>
                <td>
                  {p.billing === "paid"
                    ? <>${(p.today?.spend_usd ?? 0).toFixed(2)} / ${p.spend?.daily_usd ?? "∞"} day<br />${(p.month_spend_usd ?? 0).toFixed(2)} / ${p.spend?.monthly_usd ?? "∞"} month</>
                    : <span className="home-dim">—</span>}
                </td>
                <td className="home-dim">
                  {Object.entries(p.limits).map(([k, v]) => `${k} ${v}`).join(" · ") || "—"}
                  {conc && conc.capacity !== undefined && <div>slots {conc.in_use ?? 0}/{conc.capacity}</div>}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <div className="home-dim">As of {formatLocal(b.generated_at)}.</div>
    </>
  );
}

// ── Scheduler ────────────────────────────────────────────────────────────────

type SchedField = "interval_secs" | "sleep_interval_secs" | "idle_sleep_after_ticks" | "max_tasks_per_run" | "error_budget";

const SCHED_FIELDS: { key: SchedField; label: string; hint: string; min: number }[] = [
  { key: "interval_secs", label: "Tick interval (s)", hint: "How often the scheduler looks for work while awake", min: 10 },
  { key: "sleep_interval_secs", label: "Sleep interval (s)", hint: "Tick interval once it has gone to sleep", min: 10 },
  { key: "idle_sleep_after_ticks", label: "Sleep after idle ticks", hint: "Consecutive ticks with nothing to do before sleeping", min: 1 },
  { key: "max_tasks_per_run", label: "Tasks per tick", hint: "Most tasks dispatched by one tick", min: 1 },
  { key: "error_budget", label: "Error budget", hint: "Consecutive failed ticks before it pauses itself", min: 1 },
];

function SchedulerSection() {
  const sched = useSchedulerStatus(10_000);
  const [draft, setDraft] = useState<Partial<Record<SchedField, string>>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);

  if (!sched.data) {
    return (
      <>
        <h2>Scheduler</h2>
        {sched.error
          ? <EmptyState tone="red" title="Couldn't read the scheduler" checked={sched.error.message} />
          : <div className="loading">Loading scheduler…</div>}
      </>
    );
  }
  const data: SchedulerStatus = sched.data;
  const s = summariseScheduler(data);

  const run = async (label: string, tool: string, args: Record<string, unknown>) => {
    setBusy(label);
    setResult(null);
    try {
      await callToolStrict(tool, args);
      setResult({ ok: true, text: `${label}: done` });
      await queryClient.invalidateQueries({ queryKey: keys.schedulerStatus });
    } catch (e) {
      setResult({ ok: false, text: `${label} failed: ${e instanceof Error ? e.message : String(e)}` });
    } finally {
      setBusy(null);
    }
  };

  // Only fields the user changed are sent, so saving one value never
  // rewrites another with a stale copy.
  const changes: Partial<Record<SchedField, number>> = {};
  const invalid: string[] = [];
  for (const f of SCHED_FIELDS) {
    const raw = draft[f.key];
    if (raw === undefined) continue;
    const n = Number(raw);
    if (!Number.isInteger(n) || n < f.min) invalid.push(`${f.label} must be a whole number ≥ ${f.min}`);
    else if (n !== data.config[f.key]) changes[f.key] = n;
  }
  const dirty = Object.keys(changes).length > 0;

  return (
    <>
      <h2>Scheduler</h2>
      <p className="settings-lede">
        The scheduler runs each pending task as its workflow, and fires scheduled workflows when they come due. Pausing it
        stops new dispatch; running jobs finish.
      </p>
      <div className="settings-actions">
        <Pill tone={s.tone}>{s.label}</Pill>
        <span className="home-dim">{s.detail}</span>
      </div>
      <div className="settings-actions home-dim">
        Last tick {relTime(data.state.last_run_at)} ({formatLocal(data.state.last_run_at)}) · {data.state.tasks_dispatched} tasks dispatched since start
      </div>
      <div className="settings-actions">
        {s.enabled ? (
          <button type="button" className="btn danger" disabled={!!busy}
            onClick={() => run("Pause", "scheduler_control", { action: "stop" })}>Pause scheduler</button>
        ) : (
          <button type="button" className="btn" disabled={!!busy}
            onClick={() => run("Start", "scheduler_control", { action: "start" })}>Start scheduler</button>
        )}
        <button type="button" className="btn-ghost" disabled={!!busy || !s.enabled}
          title={s.enabled ? "Run one tick now instead of waiting" : "Start the scheduler first"}
          onClick={() => run("Tick", "scheduler_control", { action: "tick" })}>Run a tick now</button>
        {busy && <span className="home-dim">{busy}…</span>}
      </div>

      <div className="settings-form">
        {SCHED_FIELDS.map((f) => (
          <FieldRow key={f.key} id={`sched-${f.key}`} label={f.label} hint={f.hint}
            value={draft[f.key] ?? String(data.config[f.key])}
            changed={changes[f.key] !== undefined}
            onChange={(v) => setDraft((d) => ({ ...d, [f.key]: v }))} />
        ))}
      </div>
      {invalid.length > 0 && <div className="tone-text-red">{invalid.join(" · ")}</div>}
      <div className="settings-actions">
        <button type="button" className="btn" disabled={!dirty || invalid.length > 0 || !!busy}
          onClick={async () => {
            await run("Save", "scheduler_control", { action: "configure", ...changes });
            setDraft({});
          }}>
          Save {dirty ? `(${Object.keys(changes).length} changed)` : ""}
        </button>
        {dirty && <button type="button" className="btn-ghost" onClick={() => setDraft({})}>Discard</button>}
        {result && <span className={result.ok ? "tone-text-green" : "tone-text-red"}>{result.text}</span>}
      </div>
    </>
  );
}

function FieldRow({ id, label, hint, value, changed, onChange }: {
  id: string; label: string; hint: string; value: string; changed: boolean; onChange: (v: string) => void;
}) {
  return (
    <>
      <label htmlFor={id}>{label}{changed && <span className="tone-text-yellow"> •</span>}</label>
      <input id={id} className="ui-input" inputMode="numeric" value={value} onChange={(e) => onChange(e.target.value)} />
      <span className="hint">{hint}</span>
    </>
  );
}

// ── About ────────────────────────────────────────────────────────────────────

interface HealthVersion {
  version?: { sha?: string; branch?: string; dirty?: boolean; subject?: string; deployed_at?: string } | null;
}

function AboutSection() {
  const [health, setHealth] = useState<HealthVersion | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    apiJson<HealthVersion>("/api/health").then(setHealth, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  const v = health?.version;
  return (
    <>
      <h2>About</h2>
      {error && <EmptyState tone="red" title="Couldn't reach the brain" checked={error} />}
      {health && !v && (
        <EmptyState title="The brain did not report a version"
          checked="/api/health returned no `version` (no BrainVersion node yet, or a brain older than this frontend)." />
      )}
      {v && (
        <div className="settings-form">
          <label>Brain commit</label>
          <span><code>{v.sha}</code> on <code>{v.branch}</code>{v.dirty && <span className="tone-text-yellow"> (built from a dirty tree)</span>}</span>
          <span className="hint">{v.subject}</span>
          <label>Deployed</label>
          <span>{formatLocal(v.deployed_at)}</span>
          <span className="hint">{relTime(v.deployed_at)}</span>
        </div>
      )}
      <div className="settings-form">
        <label>Interface</label>
        <span>HBI, built {formatLocal(__BUILD_TIME__)}</span>
        <span className="hint">{import.meta.env.MODE}</span>
      </div>
    </>
  );
}
