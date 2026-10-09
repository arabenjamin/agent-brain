/**
 * Scheduled: every schedule (list), when they will run (timeline), and the
 * routing chains tasks are planned from (chains). `?mode=list|timeline|chains`.
 */
import { Fragment, useMemo, useState } from "react";
import { useSchedulerStatus } from "../../api/queries";
import { useTopics } from "../../api/topics";
import { useScheduleTopicMix, useWorkSchedules, type WorkSchedule } from "../../api/work";
import { groupSchedules, mixFlag, type Group, type GroupBy, type TopicMix } from "../../lib/scheduleGroups";
import { fmtEvery } from "../../lib/status";
import { bucketCounts, occurrences } from "../../lib/timeline";
import { formatLocal, parseTime, relTime } from "../../lib/time";
import { useNow } from "../../lib/useNow";
import { goTo } from "../../nav";
import { openEntity } from "../drawer/entities";
import { StatusPill } from "../drawer/parts";
import ChainsView from "../schedules/ChainsView";
import { EmptyState, Pill, Tabs } from "../ui";
import Row from "./Row";

type Mode = "list" | "timeline" | "chains";

export default function ScheduledLane({ mode, openId }: { mode?: string; openId?: string }) {
  const m: Mode = mode === "timeline" || mode === "chains" ? mode : "list";
  return (
    <div className="wk-lane-frame">
      <div className="wk-lane-bar">
        <Tabs
          tabs={[{ id: "list", label: "List" }, { id: "timeline", label: "Timeline" }, { id: "chains", label: "Routing chains" }]}
          active={m}
          onChange={(id) => goTo("work", { view: "scheduled", mode: id === "list" ? undefined : id })}
        />
        {m !== "chains" && (
          <button type="button" className="btn" onClick={() => openEntity("schedule", "new")}>+ New schedule</button>
        )}
      </div>
      {m === "list" && <ScheduleList openId={openId} />}
      {m === "timeline" && <Timeline openId={openId} />}
      {m === "chains" && <div className="wk-lane scroll"><ChainsView /></div>}
    </div>
  );
}

function useSchedulesState() {
  const q = useWorkSchedules();
  const sched = useSchedulerStatus(20_000);
  const paused = sched.data ? !sched.data.config.enabled : false;
  return { q, paused };
}

const GROUP_KEY = "hbi.schedules.groupBy";
const COLLAPSED_KEY = "hbi.schedules.collapsed";

function readStored<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v ? (JSON.parse(v) as T) : fallback;
  } catch {
    return fallback;
  }
}

function writeStored(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // private window: the choice lasts this page only
  }
}

/** Group-by choice and collapsed groups, remembered per browser. */
function useGrouping() {
  const [by, setByState] = useState<GroupBy>(() => {
    const v = readStored<string>(GROUP_KEY, "topic");
    return v === "kind" || v === "none" ? v : "topic";
  });
  const [collapsed, setCollapsed] = useState<string[]>(() => readStored<string[]>(COLLAPSED_KEY, []));
  const setBy = (v: GroupBy) => { setByState(v); writeStored(GROUP_KEY, v); };
  const isCollapsed = (key: string) => collapsed.includes(`${by}:${key}`);
  const toggle = (key: string) => {
    const k = `${by}:${key}`;
    const next = collapsed.includes(k) ? collapsed.filter((x) => x !== k) : [...collapsed, k];
    setCollapsed(next);
    writeStored(COLLAPSED_KEY, next);
  };
  const topicLabels = Object.fromEntries((useTopics().data?.topics ?? []).map((t) => [t.name, t.label]));
  return { by, setBy, isCollapsed, toggle, topicLabels };
}

function GroupByTabs({ by, setBy }: { by: GroupBy; setBy: (v: GroupBy) => void }) {
  return (
    <span className="wk-dim" style={{ display: "inline-flex", gap: 6, alignItems: "center" }}>
      group by
      <Tabs tabs={[{ id: "topic", label: "topic" }, { id: "kind", label: "kind" }, { id: "none", label: "none" }]} active={by}
        onChange={(id) => setBy(id as GroupBy)} />
    </span>
  );
}

function GroupHeader<T>({ g, collapsed, onToggle }: { g: Group<T>; collapsed: boolean; onToggle: () => void }) {
  return (
    <button type="button" className="wk-group-head" onClick={onToggle} aria-expanded={!collapsed}>
      <span>{collapsed ? "▸" : "▾"}</span>
      <span className={g.key ? "" : "wk-dim"}>{g.label}</span>
      <span className="wk-dim">{g.on} on{g.off > 0 ? ` · ${g.off} off` : ""}</span>
    </button>
  );
}

function ScheduleList({ openId }: { openId?: string }) {
  const { q, paused } = useSchedulesState();
  const now = useNow();
  const [search, setSearch] = useState("");
  const [enabledOnly, setEnabledOnly] = useState(false);
  const grouping = useGrouping();
  const mix = useScheduleTopicMix();
  const mixOf = useMemo(() => new Map((mix.data?.schedules ?? []).map((m) => [m.id, m])), [mix.data]);

  const rows = useMemo(() => {
    const all = q.data?.schedules ?? [];
    const s = search.trim().toLowerCase();
    return all
      .filter((x) => (!enabledOnly || x.enabled) && (!s || `${x.name} ${x.description ?? ""} ${x.step_tools.join(" ")}`.toLowerCase().includes(s)))
      .sort((a, b) => Number(b.enabled) - Number(a.enabled)
        || (parseTime(a.next_run_at)?.getTime() ?? Infinity) - (parseTime(b.next_run_at)?.getTime() ?? Infinity));
  }, [q.data, search, enabledOnly]);

  if (!q.data) {
    return <div className="wk-lane">{q.error ? <EmptyState tone="red" title="Couldn't load schedules" checked={q.error.message} /> : <div className="loading">Loading schedules…</div>}</div>;
  }
  const total = q.data.schedules.length;
  const on = q.data.schedules.filter((x) => x.enabled).length;

  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <input className="ui-input" placeholder="Filter by name, description or tool…" value={search} onChange={(e) => setSearch(e.target.value)} />
        <label className="wk-check"><input type="checkbox" checked={enabledOnly} onChange={(e) => setEnabledOnly(e.target.checked)} /> on only</label>
        <span className="wk-dim">{on} on · {total - on} off</span>
        <GroupByTabs by={grouping.by} setBy={grouping.setBy} />
      </div>
      {paused && <EmptyState tone="yellow" title="The scheduler is paused" checked="No schedule runs until it is started (Settings › Scheduler). Run now still works." />}
      {q.data.runs_error && <EmptyState tone="red" title="Last-run outcomes could not be read" checked={q.data.runs_error} />}
      {rows.length === 0 && <EmptyState title="No schedule matches" checked={`${total} schedules, none matching “${search}”${enabledOnly ? " that is on" : ""}.`} />}
      {mix.error && <EmptyState tone="yellow" title="What the runs produced could not be read" checked={mix.error.message} />}
      {groupSchedules(rows, grouping.by, grouping.topicLabels).map((g) => (
        <Fragment key={g.key || "_none"}>
          {grouping.by !== "none" && (
            <GroupHeader g={g} collapsed={grouping.isCollapsed(g.key)} onToggle={() => grouping.toggle(g.key)} />
          )}
          {!grouping.isCollapsed(g.key) && g.rows.map((s) => (
            <ScheduleRow key={s.id} s={s} now={now} openId={openId} by={grouping.by} mix={mixOf.get(s.id)} topicLabels={grouping.topicLabels} />
          ))}
        </Fragment>
      ))}
    </div>
  );
}

function ScheduleRow({ s, now, openId, by, mix, topicLabels }: {
  s: WorkSchedule; now: number; openId?: string; by: GroupBy; mix?: TopicMix; topicLabels: Record<string, string>;
}) {
  const due = s.enabled && (parseTime(s.next_run_at)?.getTime() ?? Infinity) < now;
  const flag = mixFlag(mix, topicLabels);
  return (
    <Row kind="schedule" id={s.id} active={openId === s.id}
      lead={<Pill tone={s.enabled ? "green" : "muted"}>{s.enabled ? "on" : "off"}</Pill>}
      meta={
        <>
          {s.last_run ? <StatusPill status={s.last_run.status} /> : <span className="wk-dim">never ran</span>}
          <span className={due ? "tone-text-yellow" : "wk-dim"} title={formatLocal(s.next_run_at)}>
            {!s.enabled ? "" : due ? "due" : `next ${relIn(s.next_run_at, now)}`}
          </span>
        </>
      }>
      <span className="wk-row-title">{s.name}</span>
      <span className="wk-dim"> · {fmtEvery(s.interval_seconds)} · {s.step_tools.length} steps{s.managed_by === "yaml" ? " · yaml" : ""}</span>
      {by !== "topic" && s.topic && <span className="wk-dim"> · {topicLabels[s.topic] ?? s.topic}</span>}
      {by !== "kind" && s.kind && <span className="wk-dim"> · {s.kind}</span>}
      {flag && <span className={`tone-text-${flag.tone}`} title="From the notes its runs wrote in the last 30 days"> · {flag.text}</span>}
      {s.legacy_steps > 0 && <span className="tone-text-red"> · {s.legacy_steps} broken steps</span>}
    </Row>
  );
}

/** "in 3h", "in 2d" for a future time. */
function relIn(raw: string, now: number): string {
  const t = parseTime(raw)?.getTime();
  if (t === undefined) return "—";
  const s = Math.max(0, Math.round((t - now) / 1000));
  if (s < 60) return "in <1m";
  if (s < 3600) return `in ${Math.round(s / 60)}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

const WINDOWS = { day: { label: "Next 24 h", ms: 86_400_000, buckets: 24 }, week: { label: "Next 7 days", ms: 7 * 86_400_000, buckets: 28 } } as const;

function Timeline({ openId }: { openId?: string }) {
  const { q, paused } = useSchedulesState();
  const now = useNow(60_000);
  const grouping = useGrouping();
  const [win, setWin] = useState<keyof typeof WINDOWS>("day");
  const { ms, buckets } = WINDOWS[win];
  const from = now;
  const to = now + ms;

  if (!q.data) {
    return <div className="wk-lane">{q.error ? <EmptyState tone="red" title="Couldn't load schedules" checked={q.error.message} /> : <div className="loading">Loading schedules…</div>}</div>;
  }
  const rows = q.data.schedules
    .filter((s) => s.enabled)
    .map((s) => ({ s, occ: occurrences(s.next_run_at, s.interval_seconds, from, to) }))
    .filter((r) => r.occ.length > 0)
    .sort((a, b) => a.occ[0].at - b.occ[0].at);
  const counts = bucketCounts(rows.map((r) => r.occ), from, to, buckets);
  const groups = groupSchedules(rows.map((r) => ({ ...r, id: r.s.id, enabled: r.s.enabled, topic: r.s.topic, kind: r.s.kind })), grouping.by, grouping.topicLabels);
  const peak = Math.max(1, ...counts);
  const pct = (t: number) => `${((t - from) / ms) * 100}%`;
  const ticks = Array.from({ length: win === "day" ? 7 : 8 }, (_, i) => from + (i * ms) / (win === "day" ? 6 : 7));
  const quiet = q.data.schedules.filter((s) => s.enabled).length - rows.length;

  return (
    <div className="wk-lane scroll">
      <div className="wk-toolbar">
        <Tabs tabs={Object.entries(WINDOWS).map(([id, w]) => ({ id: id as keyof typeof WINDOWS, label: w.label }))} active={win} onChange={setWin} />
        <span className="wk-dim">{rows.length} schedules run in this window{quiet > 0 ? ` · ${quiet} on but not due in it` : ""}</span>
        <GroupByTabs by={grouping.by} setBy={grouping.setBy} />
      </div>
      {paused && <EmptyState tone="yellow" title="The scheduler is paused" checked="These are when runs are due; none happen until it is started." />}

      <div className="tl">
        <div className="tl-axis">
          {ticks.map((t) => (
            <span key={t} style={{ left: pct(t) }}>
              {win === "day"
                ? new Date(t).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
                : new Date(t).toLocaleDateString(undefined, { weekday: "short", day: "numeric" })}
            </span>
          ))}
        </div>
        <div className="tl-row tl-load" title="Runs per bucket across all schedules — tall bars are where runs pile up">
          <span className="tl-name wk-dim">load</span>
          <span className="tl-track">
            {counts.map((c, i) => (
              <span key={i} className="tl-bar" style={{ left: `${(i / buckets) * 100}%`, width: `${100 / buckets}%`, height: `${(c / peak) * 100}%` }} title={`${c} runs`} />
            ))}
          </span>
        </div>
        {groups.map((g) => (
          <Fragment key={g.key || "_none"}>
            {grouping.by !== "none" && (
              <GroupHeader g={g} collapsed={grouping.isCollapsed(g.key)} onToggle={() => grouping.toggle(g.key)} />
            )}
            {!grouping.isCollapsed(g.key) && g.rows.map(({ s, occ }) => (
          <button key={s.id} type="button" className={`tl-row${openId === s.id ? " active" : ""}`} onClick={() => openEntity("schedule", s.id)}>
            <span className="tl-name" title={s.name}>{s.name}</span>
            <span className="tl-track">
              {occ.map((o) => (
                <span key={o.at} className={`tl-dot${o.due ? " due" : ""}`} style={{ left: pct(o.at) }}
                  title={o.due ? `Past due (was ${formatLocal(s.next_run_at)}); runs at the next tick` : formatLocal(new Date(o.at).toISOString())} />
              ))}
            </span>
            <span className="tl-count wk-dim">{occ.length}×</span>
          </button>
            ))}
          </Fragment>
        ))}
        {rows.length === 0 && <EmptyState title="Nothing runs in this window" checked={`${q.data.schedules.filter((s) => s.enabled).length} schedules are on; none is due before ${formatLocal(new Date(to).toISOString())}. Last checked ${relTime(new Date(now).toISOString())}.`} />}
      </div>
    </div>
  );
}
