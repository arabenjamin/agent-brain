/**
 * Home: what the brain is doing, and whether anything needs the user.
 *
 * Rule for every tile: each number links to the rows behind it, and an
 * empty tile says what it checked. Domains are the topics notes are
 * classified into (HBI_REDESIGN_PLAN.md §3, `services/topics.rs`).
 */
import type { ReactNode } from "react";
import { openSession, useFeed } from "../../api/feed";
import {
  useActivity,
  useLlmBudget,
  useModelUsage,
  useQueueStatus,
  useSchedulerStatus,
  useSchedules,
  type Breakdown,
  type ScheduledTask,
} from "../../api/queries";
import { diffCounts, useArchitecture } from "../../api/system";
import { assessedShare, useTopics } from "../../api/topics";
import { summariseScheduler } from "../../lib/scheduler";
import { formatLocal, parseTime, relTime } from "../../lib/time";
import { useNow } from "../../lib/useNow";
import { hrefTo } from "../../nav";
import { Card, EmptyState, Pill, Stat, type Tone } from "../ui";

const DEAD_JOB_STATUSES = "failed,dead,dead_letter";

export default function HomePanel() {
  return (
    <div className="panel home">
      <div className="panel-header">
        <span>⌂ Home</span>
        <span className="home-now">{new Date().toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" })}</span>
      </div>
      <div className="home-grid scroll">
        <BrainTile />
        <NeedsYouTile />
        <NextUpTile />
        <DomainsTile />
        <LastDayTile />
        <LlmTile />
      </div>
    </div>
  );
}

/** Loading / error placeholder shared by tiles. */
function Pending({ what, error, onRetry }: { what: string; error?: Error | null; onRetry?: () => void }) {
  if (!error) return <div className="loading">Loading {what}…</div>;
  return (
    <EmptyState
      tone="red"
      title={`Couldn't load ${what}`}
      checked={error.message}
      action={onRetry && <button type="button" className="btn" onClick={onRetry}>Retry</button>}
    />
  );
}

// ── Brain ────────────────────────────────────────────────────────────────────

function BrainTile() {
  const sched = useSchedulerStatus(20_000);
  const queue = useQueueStatus(5_000);

  let body: ReactNode;
  if (!sched.data || !queue.data) {
    body = <Pending what="brain status" error={sched.error ?? queue.error} onRetry={() => { sched.refetch(); queue.refetch(); }} />;
  } else {
    const s = summariseScheduler(sched.data);
    const q = queue.data;
    const dead = ["failed", "dead", "dead_letter"].reduce((n, k) => n + (q.by_status[k] ?? 0), 0);
    const coordinator = q.coordinator;
    body = (
      <>
        <div className="home-line">
          <span>Scheduler</span>
          <a href={hrefTo("settings", { section: "scheduler" })} title={s.detail}>
            <Pill tone={s.tone}>{s.label}</Pill>
          </a>
          {/* TODO(scheduler-last-tick): this reads "last tick never" for up to a
              full interval (5 min) after every brain restart, which looks like a
              stalled scheduler. Seen 2026-10-11: `last_run_at` lives only in
              SchedulerService's in-memory state (services/scheduler.rs), and the
              loop sleeps `interval_secs` BEFORE its first tick, so a fresh
              process honestly has none; it filled in at the first tick
              (15:08 restart → 15:13). Fix later: persist the last tick (or read
              the latest dispatch from the graph), or say "first tick in ~N min"
              when the process is younger than one interval. */}
          <span className="home-dim">last tick {relTime(sched.data.state.last_run_at)}</span>
        </div>
        <div className="home-dim home-detail">{s.detail}</div>
        <div className="ui-stats">
          <Stat label="Running" value={q.running_now} tone={q.running_now > 0 ? "green" : undefined}
            href={hrefTo("work", { view: "active" })} title="Active jobs" />
          <Stat label="Pending" value={q.in_memory_pending} tone={q.in_memory_pending > 20 ? "yellow" : undefined}
            href={hrefTo("work", { view: "active" })} title="Active jobs" />
          <Stat label="Failed / dead" value={dead} tone={dead > 0 ? "red" : undefined}
            href={hrefTo("work", { view: "history", mode: "jobs", job_status: DEAD_JOB_STATUSES })}
            title="Every job that will not run again (all time, as the queue counts them)" />
        </div>
        {coordinator && !coordinator.alive && (
          <EmptyState tone="red" title="Queue coordinator is not alive"
            checked={`Last heartbeat ${relTime(coordinator.last_heartbeat)}. Jobs will not start until it recovers.`} />
        )}
        {(q.deferred_for_gpu_lease ?? 0) > 0 && (
          <div className="home-dim">{q.deferred_for_gpu_lease} local jobs deferred for a GPU lease</div>
        )}
      </>
    );
  }
  return <Card title="Brain">{body}<BuildLine /></Card>;
}

/** What changed in the brain's make-up since the newest architecture snapshot. */
function BuildLine() {
  const a = useArchitecture();
  const s = a.data?.since_snapshot;
  if (!a.data) return null;
  const href = hrefTo("system", { view: "architecture" });
  if (!s) return <div className="home-dim home-detail"><a href={href}>Architecture</a>: no snapshot yet.</div>;
  const changed = diffCounts(s.diff);
  const last = a.data.history[0];
  return (
    <div className="home-dim home-detail" title={s.summary}>
      <a href={href}>Architecture</a>
      {changed ? `: ${changed} since the snapshot ${relTime(s.taken_at)}` : `: unchanged since the snapshot ${relTime(s.taken_at)}`}
      {last?.previous_id && Object.keys(last.diff).length > 0 && ` (that week: ${diffCounts(last.diff)})`}
    </div>
  );
}

// ── Needs you ────────────────────────────────────────────────────────────────

/** The Feed in short: the same `total` as the nav badge and the Feed header. */
function NeedsYouTile() {
  const q = useFeed();
  const f = q.data;
  const overdue = f?.items.filter((t) => t.overdue).length ?? 0;
  const feedHref = hrefTo("work", { view: "feed" });

  return (
    <Card title="Needs you" actions={f ? <a href={feedHref}>{f.total} in the Feed →</a> : undefined}>
      <div className="ui-stats">
        <Stat label="Items for you" value={f ? f.items.length : "…"} tone={overdue ? "yellow" : f?.items.length ? "accent" : undefined}
          href={feedHref} title={overdue ? `${overdue} overdue` : undefined} />
        <Stat label="To read" value={f ? f.reads.length : "…"} tone={f?.reads.length ? "accent" : undefined} href={feedHref} />
        <Stat label="Signals" value={f ? f.digests.length : "…"} tone={f?.digests.length ? "purple" : undefined} href={feedHref}
          title={f?.digests.map((d) => `${d.count} ${d.title}`).join("\n")} />
      </div>

      {f && f.items.length > 0 && (
        <ul className="home-list">
          {f.items.slice(0, 4).map((t) => (
            <li key={t.id}>
              <a href={hrefTo("work", { view: "feed", todo: t.id })}>{t.title}</a>
              {t.overdue ? <span className="tone-text-yellow">overdue</span> : t.due_at ? <span className="home-dim">due {t.due_at.slice(0, 10)}</span> : null}
            </li>
          ))}
          {f.items.length > 4 && <li className="home-dim">+{f.items.length - 4} more in the Feed</li>}
        </ul>
      )}

      {f && f.reads.length > 0 && (
        <ul className="home-list">
          {f.reads.slice(0, 3).map((n) => (
            <li key={n.id}>
              {n.related_session_id
                ? <button type="button" className="wk-link" onClick={() => openSession(n.related_session_id)}>{n.context || "Message"}</button>
                : <a href={feedHref}>{n.context || "Message"}</a>}
              <span className="home-dim">{relTime(n.created_at)}</span>
            </li>
          ))}
        </ul>
      )}

      {q.error && <Pending what="the feed" error={q.error} onRetry={() => q.refetch()} />}

      {f && f.total === 0 && (
        <EmptyState
          title="Nothing is waiting on you"
          checked={`0 items for you${f.snoozed ? ` (${f.snoozed} snoozed)` : ""} · 0 unread messages · no signals`}
        />
      )}
    </Card>
  );
}

// ── Next up ──────────────────────────────────────────────────────────────────

function whenLabel(raw: string | null): string {
  const d = parseTime(raw);
  if (!d) return "—";
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const time = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  return sameDay ? time : `${d.toLocaleDateString(undefined, { weekday: "short" })} ${time}`;
}

function NextUpTile() {
  const schedules = useSchedules();
  const sched = useSchedulerStatus(20_000);
  const now = useNow();

  if (!schedules.data) {
    return <Card title="Next up"><Pending what="workflows" error={schedules.error} onRetry={() => schedules.refetch()} /></Card>;
  }
  // Only scheduled workflows have a next run; the rest run when started by name.
  const all = schedules.data.filter((s) => s.interval_seconds != null);
  const enabled = all.filter((s) => s.enabled);
  const upcoming = [...enabled].sort(
    (a, b) => (parseTime(a.next_run_at)?.getTime() ?? Infinity) - (parseTime(b.next_run_at)?.getTime() ?? Infinity),
  );
  const due = upcoming.filter((s) => (parseTime(s.next_run_at)?.getTime() ?? Infinity) < now);
  const paused = sched.data && !sched.data.config.enabled;

  return (
    <Card
      title="Next up"
      actions={<a className="home-dim" href={hrefTo("work", { view: "scheduled" })}>{enabled.length} enabled · {all.length - enabled.length} off</a>}
    >
      {enabled.length === 0 ? (
        <EmptyState title="No scheduled workflow is on" checked={`${all.length} scheduled workflows exist; all are turned off.`} />
      ) : (
        <ul className="home-list home-schedule">
          {upcoming.slice(0, 7).map((s: ScheduledTask) => {
            const isDue = due.includes(s);
            return (
              <li key={s.id} title={`${s.name}\nNext: ${formatLocal(s.next_run_at)}\nLast: ${formatLocal(s.last_run_at)}`}>
                <span className={isDue ? "tone-text-yellow" : "home-dim"}>{isDue ? "due" : whenLabel(s.next_run_at)}</span>
                <a href={hrefTo("work", { view: "scheduled", schedule: s.id })}>{s.name}</a>
              </li>
            );
          })}
        </ul>
      )}
      {due.length > 0 && (
        <div className="home-dim home-detail">
          {due.length} past their run time{paused ? " — the scheduler is paused, so they will not run until it is started" : " — they run at the next scheduler tick"}.
        </div>
      )}
    </Card>
  );
}

// ── Domains ──────────────────────────────────────────────────────────────────

function DomainsTile() {
  const topics = useTopics(120_000);
  if (!topics.data) {
    return <Card title="Domains"><Pending what="topics" error={topics.error} onRetry={() => topics.refetch()} /></Card>;
  }
  const { coverage: c } = topics.data;
  const list = [...topics.data.topics].sort((a, b) => b.week - a.week || b.notes - a.notes);
  const peak = Math.max(1, ...list.map((t) => t.week));
  return (
    <Card title="Domains" actions={<a className="home-dim" href={hrefTo("knowledge", { view: "map" })}>map →</a>}>
      {list.length === 0 ? (
        <EmptyState title="No topics declared" checked="plugins/core/topics/*.yaml defines none." />
      ) : (
        <ul className="home-list home-domains">
          {list.map((t) => (
            <li key={t.name} title={`${t.description}\n${t.notes.toLocaleString()} notes in all`}>
              <span className="home-bar" style={{ width: `${(t.week / peak) * 100}%` }} />
              <a href={hrefTo("knowledge", { view: "list", within: t.name })}>{t.label}</a>
              <span className="home-dim">{t.week} this week · {t.notes.toLocaleString()}</span>
            </li>
          ))}
        </ul>
      )}
      <div className="home-dim home-detail">
        {c.waiting > 0
          ? `${Math.round(assessedShare(c) * 100)}% of ${c.notes.toLocaleString()} notes classified so far; ${c.waiting.toLocaleString()} wait for an idle moment.`
          : `All ${c.notes.toLocaleString()} notes classified.`}
        {" "}{c.no_topic.toLocaleString()} fit no topic{c.failed > 0 ? `, ${c.failed} failed` : ""}
        {c.skipped > 0 ? `; ${c.skipped.toLocaleString()} activity records skipped` : ""}.
      </div>
    </Card>
  );
}

// ── Last 24 hours ────────────────────────────────────────────────────────────

const TASK_TONE: Record<string, Tone> = { completed: "green", failed: "red", in_progress: "yellow", created: "accent", blocked: "purple" };

function topEntries(b: Breakdown, n: number): [string, number][] {
  return Object.entries(b.by).sort((a, z) => z[1] - a[1]).slice(0, n);
}

function LastDayTile() {
  const activity = useActivity(24);
  if (!activity.data) {
    return <Card title="Last 24 hours"><Pending what="activity" error={activity.error} onRetry={() => activity.refetch()} /></Card>;
  }
  const a = activity.data;
  const since = a.since;
  const failedJobs = a.jobs ? ["failed", "dead", "dead_letter"].reduce((n, k) => n + (a.jobs!.by[k] ?? 0), 0) : 0;
  const claimsCorroborated = a.claims_checked?.by.corroborated ?? 0;

  return (
    <Card title="Last 24 hours" actions={<span className="home-dim">since {formatLocal(since)}</span>}>
      <div className="ui-stats">
        <Stat label="Notes" value={a.notes?.total ?? "?"} href={hrefTo("knowledge", { view: "list", since })} />
        <Stat label="Tasks done" value={a.tasks?.by.completed ?? 0} tone="green"
          href={hrefTo("work", { view: "history", mode: "tasks", task_status: "completed", since })} />
        <Stat label="Tasks failed" value={a.tasks?.by.failed ?? 0} tone={(a.tasks?.by.failed ?? 0) > 0 ? "red" : undefined}
          href={hrefTo("work", { view: "history", mode: "tasks", task_status: "failed", since })} />
        <Stat label="Claims checked" value={a.claims_checked?.total ?? "?"}
          title={a.claims_checked ? `${claimsCorroborated} corroborated · ${a.claims_checked.by.refuted ?? 0} refuted · ${a.claims_checked.by.unverified ?? 0} still unverified` : undefined}
          href={hrefTo("knowledge", { view: "list", type: "claim", verified_since: since })} />
        <Stat label="Schedule runs" value={a.schedule_runs ?? "?"} href={hrefTo("work", { view: "scheduled" })}
          title="Schedules whose last run falls in the window" />
      </div>

      {a.notes && a.notes.total > 0 && (
        <div className="home-chips">
          {topEntries(a.notes, 6).map(([type, n]) => (
            <a key={type} href={hrefTo("knowledge", { view: "list", since, type })}>
              <Pill tone="muted">{type} {n}</Pill>
            </a>
          ))}
          {Object.keys(a.notes.by).length > 6 && <span className="home-dim">+{Object.keys(a.notes.by).length - 6} types</span>}
        </div>
      )}

      {a.tasks && Object.keys(a.tasks.by).some((k) => k !== "completed" && k !== "failed") && (
        <div className="home-chips">
          {topEntries(a.tasks, 6).filter(([k]) => k !== "completed" && k !== "failed").map(([status, n]) => (
            <a key={status} href={hrefTo("work", { view: "history", mode: "tasks", task_status: status, since })}>
              <Pill tone={TASK_TONE[status] ?? "muted"}>tasks {status} {n}</Pill>
            </a>
          ))}
        </div>
      )}

      {failedJobs > 0 && (
        <div className="home-chips">
          <a href={hrefTo("work", { view: "history", mode: "jobs", job_status: DEAD_JOB_STATUSES, since })}>
            <Pill tone="red">{failedJobs} jobs failed or died</Pill>
          </a>
        </div>
      )}

      {a.errors.length > 0 && (
        <EmptyState tone="red" title="Some counts could not be read" checked={a.errors.join(" · ")} />
      )}
    </Card>
  );
}

// ── LLM usage ────────────────────────────────────────────────────────────────

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}k`;
  return String(n);
}

function LlmTile() {
  const usage = useModelUsage(24);
  const budget = useLlmBudget();
  const href = hrefTo("settings", { section: "models" });

  if (!usage.data) {
    return <Card title="LLM, last 24 hours"><Pending what="model usage" error={usage.error} onRetry={() => usage.refetch()} /></Card>;
  }
  const models = [...usage.data].sort((a, b) => b.total_calls - a.total_calls);
  const calls = models.reduce((n, m) => n + m.total_calls, 0);
  const failures = models.reduce((n, m) => n + m.failures, 0);
  const cost = models.reduce((n, m) => n + (m.total_cost ?? 0), 0);
  const paid = (budget.data?.providers ?? []).filter((p) => p.billing === "paid" && p.enabled);
  const cooling = (budget.data?.providers ?? []).flatMap((p) =>
    (p.models ?? []).filter((m) => m.cooling_down).map((m) => ({ provider: p.name, ...m })),
  );

  return (
    <Card title="LLM, last 24 hours" actions={<a className="home-dim" href={href}>providers →</a>}>
      <div className="ui-stats">
        <Stat label="Calls" value={calls.toLocaleString()} href={href} />
        <Stat label="Failed" value={failures} tone={failures > 0 ? "red" : undefined} href={href} />
        <Stat label="Spend" value={`$${cost.toFixed(2)}`} href={href} />
      </div>
      {models.length === 0 ? (
        <EmptyState title="No LLM calls recorded" checked="The model_usage ledger has no rows in the last 24 h." />
      ) : (
        <table className="home-table">
          <tbody>
            {models.slice(0, 5).map((m) => (
              <tr key={m.model}>
                <td className="home-model">{m.model}</td>
                <td>{m.total_calls}</td>
                <td className="home-dim">{fmtTokens(m.total_tokens_in)} in · {fmtTokens(m.total_tokens_out)} out</td>
                <td>{m.total_cost ? `$${m.total_cost.toFixed(2)}` : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {paid.map((p) => (
        <div key={p.name} className="home-dim home-detail">
          {p.name} (paid): ${(p.today?.spend_usd ?? 0).toFixed(2)} today of ${p.spend?.daily_usd ?? "?"} · ${(p.month_spend_usd ?? 0).toFixed(2)} this month of ${p.spend?.monthly_usd ?? "?"}
        </div>
      ))}
      {cooling.map((m) => (
        <div key={`${m.provider}/${m.model}`} className="tone-text-yellow home-detail">
          {m.model} cooling down until {formatLocal(m.cooldown_until)}{m.cooldown_reason ? ` (${m.cooldown_reason})` : ""}
        </div>
      ))}
    </Card>
  );
}
