/**
 * System → Health: is the machine all right? One call (`/api/system/health`)
 * drives the whole view. Issues come first, worst first; then each service the
 * brain depends on (from `architecture/components.yaml`), then the scheduler,
 * the queue, the GPU lease and the providers.
 */
import { useSystemHealth, type ComponentStatus, type HealthIssue, type SystemHealth } from "../../api/system";
import { formatLocal, relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { Card, EmptyState, Pill, Stat, type Tone } from "../ui";

const STATUS_TONE: Record<ComponentStatus, Tone> = { up: "green", degraded: "yellow", down: "red", not_configured: "muted" };
const STATUS_LABEL: Record<ComponentStatus, string> = { up: "up", degraded: "degraded", down: "down", not_configured: "not configured" };
const SEVERITY_TONE: Record<HealthIssue["severity"], Tone> = { critical: "red", warn: "yellow", info: "accent" };
const OVERALL: Record<SystemHealth["overall"], { tone: Tone; text: string }> = {
  ok: { tone: "green", text: "Everything the brain depends on is answering." },
  degraded: { tone: "yellow", text: "The brain is running, with something to look at." },
  down: { tone: "red", text: "Something the brain needs is not working." },
};

export default function HealthView() {
  const h = useSystemHealth();
  if (!h.data) {
    return (
      <div className="sys scroll">
        {h.error
          ? <EmptyState tone="red" title="Couldn't reach the brain" checked={`GET /api/system/health failed: ${h.error.message}`} action={<button type="button" className="btn" onClick={() => h.refetch()}>Retry</button>} />
          : <div className="loading">Checking every component…</div>}
      </div>
    );
  }
  const d = h.data;
  const o = OVERALL[d.overall];
  return (
    <div className="sys scroll">
      <div className={`sys-banner ${d.overall}`}>
        <Pill tone={o.tone}>{d.overall}</Pill>
        <span>{o.text}</span>
        <span className="wk-dim sys-checked" title={formatLocal(d.checked_at)}>checked {relTime(d.checked_at)} · every 15 s</span>
      </div>

      {d.issues.length > 0 && (
        <section className="sys-issues">
          {d.issues.map((i, k) => (
            <div key={k} className="sys-issue">
              <Pill tone={SEVERITY_TONE[i.severity]}>{i.severity}</Pill>
              <span className="wk-dim">{i.area}</span>
              <span>{i.message}</span>
            </div>
          ))}
        </section>
      )}

      <div className="sys-grid">
        <Card title="Services" actions={<span className="wk-dim">{d.components.length} declared in architecture/components.yaml</span>}>
          {d.components.length === 0 ? (
            <EmptyState tone="yellow" title="No components declared" checked="architecture/components.yaml was not found or is empty (ARCHITECTURE_DIR), so nothing was probed." />
          ) : (
            <ul className="sys-comps">
              {d.components.map((c) => (
                <li key={c.name} title={c.purpose}>
                  <Pill tone={STATUS_TONE[c.status]}>{STATUS_LABEL[c.status]}</Pill>
                  <span className="sys-comp-name">{c.label}{c.required && <span className="wk-dim" title="The brain is broken without it"> · required</span>}</span>
                  <span className="wk-dim">{c.latency_ms !== null ? `${c.latency_ms} ms` : ""}</span>
                  <span className="wk-dim sys-comp-detail">{c.detail || c.purpose}{c.endpoint ? ` · ${c.endpoint}` : ""}</span>
                </li>
              ))}
            </ul>
          )}
        </Card>

        <SchedulerCard s={d.scheduler} />
        <QueueCard q={d.queue} lease={d.gpu_lease} />
        <ProvidersCard budget={d.budget} />

        <Card title="Running version">
          {d.version?.sha ? (
            <div className="sys-version">
              <code>{d.version.sha.slice(0, 10)}</code>{d.version.dirty && <Pill tone="yellow">uncommitted changes</Pill>}
              <span>{d.version.subject}</span>
              <span className="wk-dim">{d.version.branch} · deployed {relTime(d.version.deployed_at)} ({formatLocal(d.version.deployed_at)})</span>
            </div>
          ) : (
            <EmptyState title="No version recorded" checked="No (:BrainVersion {id:'current'}) node: the binary records it at startup." />
          )}
        </Card>
      </div>
    </div>
  );
}

/* eslint-disable @typescript-eslint/no-explicit-any -- the services' own JSON */
function SchedulerCard({ s }: { s: any }) {
  if (s?.error) return <Card title="Scheduler"><EmptyState tone="yellow" title="Scheduler unavailable" checked={s.error} /></Card>;
  const on = s?.config?.enabled;
  return (
    <Card title="Scheduler" actions={<a className="wk-dim" href={hrefTo("settings", { section: "scheduler" })}>settings →</a>}>
      <div className="ui-stats">
        <Stat label="State" value={on ? (s.state?.is_sleeping ? "sleeping" : s.state?.is_running ? "ticking now" : "on") : "off"} />
        <Stat label="Last tick" value={relTime(s?.state?.last_run_at) || "never"} title={formatLocal(s?.state?.last_run_at)} />
        <Stat label="Errors in a row" value={`${s?.state?.consecutive_errors ?? 0} / ${s?.config?.error_budget ?? "?"}`}
          title="It pauses itself when this reaches the budget" />
        <Stat label="Tick every" value={`${Math.round((s?.config?.interval_secs ?? 0) / 60)} min`} />
      </div>
      {s?.state?.last_error && <div className="tone-text-red sys-detail">Last error: {s.state.last_error}</div>}
    </Card>
  );
}

function QueueCard({ q, lease }: { q: any; lease: any }) {
  if (q?.error) return <Card title="Job queue"><EmptyState tone="yellow" title="Queue unavailable" checked={q.error} /></Card>;
  const providers = Object.entries(q?.per_provider ?? {}) as [string, { running: number; max: number }][];
  return (
    <Card title="Job queue" actions={<a className="wk-dim" href={hrefTo("work", { view: "active" })}>jobs →</a>}>
      <div className="ui-stats">
        <Stat label="Coordinator" value={q?.coordinator?.alive ? "alive" : "stopped"} title={`last heartbeat ${formatLocal(q?.coordinator?.last_heartbeat)}`} />
        <Stat label="Waiting" value={q?.in_memory_pending ?? 0} />
        <Stat label="Deferred (GPU)" value={q?.deferred_for_gpu_lease ?? 0} />
      </div>
      <div className="sys-slots">
        {providers.map(([name, p]) => (
          <div key={name} className="sys-slot" title={`${p.running} of ${p.max} ${name} slots in use`}>
            <span>{name}</span>
            <span className="sys-slot-bar"><span style={{ width: `${p.max ? (p.running / p.max) * 100 : 0}%` }} /></span>
            <span className="wk-dim">{p.running}/{p.max}</span>
          </div>
        ))}
      </div>
      <div className="wk-dim sys-detail">
        GPU: {lease?.held ? <span className="tone-text-yellow">leased to {lease.holder} until {formatLocal(lease.expires_at)}</span> : "free (local models available)"}
      </div>
    </Card>
  );
}

function ProvidersCard({ budget }: { budget: any }) {
  const list: any[] = budget?.providers ?? [];
  return (
    <Card title="Providers" actions={<a className="wk-dim" href={hrefTo("settings", { section: "models" })}>models →</a>}>
      {list.length === 0 ? (
        <EmptyState title="No providers in the budget ledger" checked="services/budget.rs reported none." />
      ) : (
        <ul className="sys-comps">
          {list.map((p) => {
            const cooling = (p.models ?? []).filter((m: any) => m.cooling_down);
            const today = (p.models ?? []).reduce((a: number, m: any) => a + (m.today?.requests ?? 0), 0);
            const tone: Tone = !p.enabled ? "muted" : p.key_configured === false ? "yellow" : cooling.length ? "yellow" : "green";
            return (
              <li key={p.name ?? p.kind}>
                <Pill tone={tone}>{!p.enabled ? "off" : p.key_configured === false ? "no key" : cooling.length ? "cooling" : "ready"}</Pill>
                <span className="sys-comp-name">{p.name ?? p.kind}</span>
                <span className="wk-dim">{today} today</span>
                <span className="wk-dim sys-comp-detail">
                  {p.billing}{p.data_policy ? ` · ${p.data_policy}` : ""}
                  {cooling.length > 0 && ` · ${cooling.map((m: any) => m.model).join(", ")} cooling down`}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </Card>
  );
}
/* eslint-enable @typescript-eslint/no-explicit-any */
