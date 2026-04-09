import { useEffect, useState, useCallback } from "react";
import { callTool, onNotification } from "../../api/mcp";

// ── Colour palette ────────────────────────────────────────────────────────────
const C = {
  bg:     "#0d0f14",
  panel:  "#13161f",
  card:   "#1a1e2a",
  border: "#2a2f3e",
  accent: "#4f8ef7",
  text:   "#d4d8e8",
  dim:    "#7a8099",
  muted:  "#4a5068",
  green:  "#4ade80",
  red:    "#f87171",
  yellow: "#fbbf24",
  purple: "#a78bfa",
  cyan:   "#22d3ee",
  orange: "#fb923c",
};

// ── Helpers ───────────────────────────────────────────────────────────────────
function relTime(iso: string | null | undefined): string {
  if (!iso) return "never";
  const secs = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
  if (secs < 5)    return "just now";
  if (secs < 60)   return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

function fmtBytes(b: number): string {
  if (b < 1024) return `${b} B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
  return `${(b / (1024 * 1024)).toFixed(2)} MB`;
}

function parseJson<T>(raw: string): T | null {
  try { return JSON.parse(raw) as T; } catch { return null; }
}

// ── Types ─────────────────────────────────────────────────────────────────────
interface SchedulerStatus {
  config: {
    enabled: boolean;
    interval_secs: number;
    sleep_interval_secs: number;
    idle_sleep_after_ticks: number;
    max_tasks_per_run: number;
    error_budget: number;
    session_id: string | null;
  };
  state: {
    is_running: boolean;
    is_sleeping: boolean;
    idle_ticks: number;
    tasks_dispatched: number;
    consecutive_errors: number;
    last_run_at: string | null;
    last_activity_at: string | null;
    last_error: string | null;
  };
}

interface QueueStatus {
  in_memory_pending: number;
  running_now: number;
  max_concurrent: number;
  enabled: boolean;
  by_status: Record<string, number>;
}

interface SnapshotMeta {
  file: string;
  exported_at: string;
  notes: number;
  entities: number;
  tasks: number;
  size_bytes: number;
  schema_version: number;
}

interface SnapshotList {
  count: number;
  snapshots: SnapshotMeta[];
}

interface IntegrityResult {
  total_issues: number;
  checks: {
    empty_notes:              { count: number };
    orphaned_chunks:          { count: number };
    suspicious_consolidated:  { count: number };
    duplicate_notes:          { count: number };
  };
}

// ── Small reusable components ─────────────────────────────────────────────────

function StatusDot({ ok, warn }: { ok: boolean; warn?: boolean }) {
  const col = !ok ? C.red : warn ? C.yellow : C.green;
  return (
    <span style={{
      display: "inline-block", width: 8, height: 8, borderRadius: "50%",
      background: col, flexShrink: 0,
      boxShadow: `0 0 6px ${col}88`,
    }} />
  );
}

function Pill({ label, color }: { label: string; color: string }) {
  return (
    <span style={{
      fontSize: 9, fontWeight: 700, padding: "1px 7px", borderRadius: 10,
      background: `${color}22`, color, border: `1px solid ${color}44`,
      letterSpacing: "0.05em", textTransform: "uppercase",
    }}>{label}</span>
  );
}

function Metric({ label, value, color }: { label: string; value: string | number; color?: string }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
      <div style={{ fontSize: 9, color: C.muted, textTransform: "uppercase", letterSpacing: "0.06em" }}>{label}</div>
      <div style={{ fontSize: 14, fontWeight: 700, color: color ?? C.text, lineHeight: 1 }}>{value}</div>
    </div>
  );
}

function ActionBtn({
  label, color = C.accent, disabled = false, danger = false, onClick,
}: {
  label: string; color?: string; disabled?: boolean; danger?: boolean; onClick: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const handle = async () => {
    if (busy || disabled) return;
    setBusy(true);
    try { await onClick(); } finally { setBusy(false); }
  };
  const col = danger ? C.red : color;
  return (
    <button
      onClick={handle}
      disabled={disabled || busy}
      style={{
        fontSize: 10, fontWeight: 600, padding: "4px 10px", borderRadius: 5,
        border: `1px solid ${col}44`,
        background: busy ? `${col}11` : "transparent",
        color: disabled ? C.muted : col,
        cursor: disabled ? "default" : "pointer",
        transition: "background 0.15s",
        opacity: disabled ? 0.5 : 1,
        fontFamily: "inherit",
      }}
    >{busy ? "…" : label}</button>
  );
}

function CardHeader({ title, icon, status, pill }: {
  title: string; icon: string; status?: React.ReactNode; pill?: React.ReactNode;
}) {
  return (
    <div style={{
      display: "flex", alignItems: "center", gap: 8,
      borderBottom: `1px solid ${C.border}`,
      padding: "10px 14px",
    }}>
      <span style={{ fontSize: 14 }}>{icon}</span>
      <span style={{ fontSize: 11, fontWeight: 700, color: C.text, flex: 1 }}>{title}</span>
      {pill}
      {status}
    </div>
  );
}

function Card({ children, accent }: { children: React.ReactNode; accent?: string }) {
  return (
    <div style={{
      background: C.card,
      border: `1px solid ${accent ? `${accent}33` : C.border}`,
      borderRadius: 8,
      display: "flex", flexDirection: "column",
      boxShadow: accent ? `0 0 20px ${accent}11` : undefined,
      overflow: "hidden",
    }}>
      {children}
    </div>
  );
}

// ── Scheduler Card ─────────────────────────────────────────────────────────────
function SchedulerCard() {
  const [data, setData]     = useState<SchedulerStatus | null>(null);
  const [error, setError]   = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [toast, setToast]   = useState<string | null>(null);

  const showToast = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 2500);
  };

  const fetch = useCallback(async () => {
    try {
      const raw = await callTool("get_scheduler_status", {});
      const parsed = parseJson<SchedulerStatus>(raw);
      if (parsed) { setData(parsed); setError(null); }
      else setError("bad response");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  // Poll every 20s; also refresh immediately on any job notification
  useEffect(() => {
    fetch();
    const interval = setInterval(fetch, 20_000);
    const unsub = onNotification((n) => {
      if (n.method === "notifications/agent_job") fetch();
    });
    return () => { clearInterval(interval); unsub(); };
  }, [fetch]);

  const s = data?.state;
  const cfg = data?.config;

  const isRunning  = s?.is_running ?? false;
  const isSleeping = s?.is_sleeping ?? false;
  const hasError   = (s?.consecutive_errors ?? 0) > 0;

  const statusLabel = !isRunning ? "paused" : isSleeping ? "sleeping" : hasError ? "errors" : "running";
  const statusColor = !isRunning ? C.yellow : isSleeping ? C.cyan : hasError ? C.red : C.green;

  const idlePct = cfg ? Math.min(100, ((s?.idle_ticks ?? 0) / cfg.idle_sleep_after_ticks) * 100) : 0;
  const errPct  = cfg ? Math.min(100, ((s?.consecutive_errors ?? 0) / cfg.error_budget) * 100) : 0;

  const handleToggle = async () => {
    if (isRunning) {
      await callTool("stop_scheduler", {});
      showToast("Scheduler paused");
    } else {
      await callTool("start_scheduler", {});
      showToast("Scheduler started");
    }
    await fetch();
  };

  const handleTick = async () => {
    showToast("Running tick…");
    await callTool("run_scheduler_tick", {});
    showToast("Tick complete");
    await fetch();
  };

  return (
    <Card accent={statusColor}>
      <CardHeader
        title="SchedulerService"
        icon="⏱"
        status={<StatusDot ok={isRunning} warn={isSleeping || hasError} />}
        pill={<Pill label={statusLabel} color={statusColor} />}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>

        {loading && <div style={{ color: C.muted, fontSize: 11 }}>Loading…</div>}
        {error   && <div style={{ color: C.red,   fontSize: 11 }}>Error: {error}</div>}

        {data && (
          <>
            {/* Metrics row */}
            <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 12 }}>
              <Metric label="Dispatched"  value={s?.tasks_dispatched ?? 0} color={C.accent} />
              <Metric label="Interval"    value={`${cfg?.interval_secs ?? 0}s`} />
              <Metric label="Last run"    value={relTime(s?.last_run_at)} />
              <Metric label="Last active" value={relTime(s?.last_activity_at)} />
            </div>

            {/* Idle progress bar */}
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div style={{ display: "flex", justifyContent: "space-between" }}>
                <span style={{ fontSize: 9, color: C.muted, textTransform: "uppercase", letterSpacing: "0.06em" }}>
                  Idle ticks ({s?.idle_ticks ?? 0}/{cfg?.idle_sleep_after_ticks ?? 3} → sleep)
                </span>
                <span style={{ fontSize: 9, color: isSleeping ? C.cyan : C.dim }}>
                  {isSleeping ? `sleep interval: ${cfg?.sleep_interval_secs}s` : `${idlePct.toFixed(0)}%`}
                </span>
              </div>
              <div style={{ height: 4, borderRadius: 2, background: C.border }}>
                <div style={{ height: "100%", borderRadius: 2, width: `${idlePct}%`, background: isSleeping ? C.cyan : C.accent, transition: "width 0.3s" }} />
              </div>
            </div>

            {/* Error budget bar */}
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div style={{ display: "flex", justifyContent: "space-between" }}>
                <span style={{ fontSize: 9, color: C.muted, textTransform: "uppercase", letterSpacing: "0.06em" }}>
                  Error budget ({s?.consecutive_errors ?? 0}/{cfg?.error_budget ?? 5} → auto-pause)
                </span>
                <span style={{ fontSize: 9, color: hasError ? C.red : C.dim }}>{errPct.toFixed(0)}%</span>
              </div>
              <div style={{ height: 4, borderRadius: 2, background: C.border }}>
                <div style={{ height: "100%", borderRadius: 2, width: `${errPct}%`, background: hasError ? C.red : C.green, transition: "width 0.3s" }} />
              </div>
            </div>

            {/* Last error */}
            {s?.last_error && (
              <div style={{ fontSize: 10, color: C.red, background: "#f8717122", padding: "6px 8px", borderRadius: 4 }}>
                Last error: {s.last_error}
              </div>
            )}

            {/* Actions */}
            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              <ActionBtn
                label={isRunning ? "Pause Scheduler" : "Start Scheduler"}
                color={isRunning ? C.yellow : C.green}
                onClick={handleToggle}
              />
              <ActionBtn label="Run Tick Now" color={C.accent} disabled={!isRunning} onClick={handleTick} />
              <ActionBtn label="Refresh" onClick={fetch} />
              {toast && <span style={{ fontSize: 10, color: C.dim, marginLeft: 4 }}>{toast}</span>}
            </div>
          </>
        )}
      </div>
    </Card>
  );
}

// ── Queue Card ─────────────────────────────────────────────────────────────────
function QueueCard() {
  const [data, setData]     = useState<QueueStatus | null>(null);
  const [error, setError]   = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [toast, setToast]   = useState<string | null>(null);

  const showToast = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 2500);
  };

  const fetch = useCallback(async () => {
    try {
      const raw = await callTool("queue_status", {});
      const parsed = parseJson<QueueStatus>(raw);
      if (parsed) { setData(parsed); setError(null); }
      else setError("bad response");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  // Poll every 5s; also update instantly on job push notifications
  useEffect(() => {
    fetch();
    const interval = setInterval(fetch, 5_000);
    const unsub = onNotification((n) => {
      if (n.method === "notifications/agent_job") fetch();
    });
    return () => { clearInterval(interval); unsub(); };
  }, [fetch]);

  const busy  = (data?.running_now ?? 0) > 0;
  const stuck = (data?.in_memory_pending ?? 0) > 20;
  const statusColor = !data?.enabled ? C.yellow : busy ? C.green : stuck ? C.red : C.dim;

  const handleDrain = async () => {
    if (!confirm("Drain all pending jobs? Running jobs continue.")) return;
    await callTool("drain_queue", {});
    showToast("Queue drained");
    fetch();
  };

  return (
    <Card accent={statusColor}>
      <CardHeader
        title="QueueService"
        icon="⚙"
        status={<StatusDot ok={data?.enabled ?? false} warn={stuck} />}
        pill={<Pill label={busy ? "active" : "idle"} color={busy ? C.green : C.dim} />}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>
        {loading && <div style={{ color: C.muted, fontSize: 11 }}>Loading…</div>}
        {error   && <div style={{ color: C.red,   fontSize: 11 }}>Error: {error}</div>}

        {data && (
          <>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 12 }}>
              <Metric label="Running"  value={data.running_now}       color={busy ? C.green : C.text} />
              <Metric label="Pending"  value={data.in_memory_pending}  color={stuck ? C.red : C.text} />
              <Metric label="Max conc" value={data.max_concurrent} />
            </div>

            {/* by_status breakdown */}
            {data.by_status && Object.keys(data.by_status).length > 0 && (
              <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
                {Object.entries(data.by_status).map(([status, count]) => (
                  <div key={status} style={{
                    fontSize: 10, padding: "2px 8px", borderRadius: 4,
                    background: C.panel, border: `1px solid ${C.border}`,
                    color: status === "running" ? C.green : status === "failed" || status === "dead" ? C.red : C.dim,
                  }}>
                    {status}: <strong>{count}</strong>
                  </div>
                ))}
              </div>
            )}

            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              <ActionBtn label="Drain Queue" color={C.red} danger onClick={handleDrain} />
              <ActionBtn label="Refresh" onClick={fetch} />
              {toast && <span style={{ fontSize: 10, color: C.dim, marginLeft: 4 }}>{toast}</span>}
            </div>
          </>
        )}
      </div>
    </Card>
  );
}

// ── Snapshot Card ──────────────────────────────────────────────────────────────
function SnapshotCard() {
  const [data, setData]     = useState<SnapshotList | null>(null);
  const [error, setError]   = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [toast, setToast]   = useState<string | null>(null);

  const showToast = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 3000);
  };

  const fetch = useCallback(async () => {
    try {
      const raw = await callTool("list_snapshots", {});
      const parsed = parseJson<SnapshotList>(raw);
      if (parsed) { setData(parsed); setError(null); }
      else setError("bad response");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  // Poll every 2 minutes — snapshots change rarely
  useEffect(() => {
    fetch();
    const interval = setInterval(fetch, 120_000);
    return () => clearInterval(interval);
  }, [fetch]);

  const latest = data?.snapshots?.[0];
  const hasSnaps = (data?.count ?? 0) > 0;
  const ageHours = latest
    ? (Date.now() - new Date(latest.exported_at).getTime()) / 3600000
    : Infinity;
  const stale = ageHours > 24;
  const statusColor = !hasSnaps ? C.red : stale ? C.yellow : C.green;

  const handleSnapshot = async () => {
    showToast("Taking snapshot…");
    try {
      await callTool("snapshot_knowledge", { label: "manual" });
      showToast("Snapshot saved");
      fetch();
    } catch (e) {
      showToast(`Failed: ${e}`);
    }
  };

  return (
    <Card accent={statusColor}>
      <CardHeader
        title="SnapshotService"
        icon="💾"
        status={<StatusDot ok={hasSnaps} warn={stale} />}
        pill={<Pill label={!hasSnaps ? "no backups" : stale ? "stale" : "healthy"} color={statusColor} />}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>
        {loading && <div style={{ color: C.muted, fontSize: 11 }}>Loading…</div>}
        {error   && <div style={{ color: C.red,   fontSize: 11 }}>Error: {error}</div>}

        {data && (
          <>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 12 }}>
              <Metric label="Snapshots" value={data.count}                      color={hasSnaps ? C.text : C.red} />
              <Metric label="Last saved" value={relTime(latest?.exported_at)}   color={stale ? C.yellow : C.text} />
              <Metric label="Size"       value={latest ? fmtBytes(latest.size_bytes) : "—"} />
            </div>

            {latest && (
              <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 12 }}>
                <Metric label="Notes"    value={latest.notes} />
                <Metric label="Entities" value={latest.entities} />
                <Metric label="Tasks"    value={latest.tasks} />
              </div>
            )}

            {/* Snapshot list */}
            {data.snapshots.length > 0 && (
              <div style={{ display: "flex", flexDirection: "column", gap: 4, maxHeight: 120, overflow: "auto" }}>
                {data.snapshots.map((s) => (
                  <div key={s.file} style={{
                    display: "flex", justifyContent: "space-between", alignItems: "center",
                    fontSize: 10, color: C.dim, padding: "3px 8px",
                    background: C.panel, borderRadius: 4, border: `1px solid ${C.border}`,
                  }}>
                    <span style={{ color: C.text, fontWeight: 600, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: "55%" }}>
                      {s.file}
                    </span>
                    <span style={{ display: "flex", gap: 12, flexShrink: 0 }}>
                      <span>{s.notes} notes</span>
                      <span>{fmtBytes(s.size_bytes)}</span>
                      <span>{relTime(s.exported_at)}</span>
                    </span>
                  </div>
                ))}
              </div>
            )}

            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              <ActionBtn label="Take Snapshot" color={C.green} onClick={handleSnapshot} />
              <ActionBtn label="Refresh" onClick={fetch} />
              {toast && <span style={{ fontSize: 10, color: C.dim, marginLeft: 4 }}>{toast}</span>}
            </div>
          </>
        )}
      </div>
    </Card>
  );
}

// ── Knowledge Integrity Card ───────────────────────────────────────────────────
function IntegrityCard() {
  const [data, setData]       = useState<IntegrityResult | null>(null);
  const [error, setError]     = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [checkedAt, setCheckedAt] = useState<string | null>(null);

  const runCheck = useCallback(async () => {
    setLoading(true);
    try {
      const raw = await callTool("verify_knowledge_integrity", {});
      const parsed = parseJson<IntegrityResult>(raw);
      if (parsed) { setData(parsed); setError(null); setCheckedAt(new Date().toISOString()); }
      else setError("bad response");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const issues = data?.total_issues ?? 0;
  const statusColor = !data ? C.dim : issues === 0 ? C.green : issues < 10 ? C.yellow : C.red;

  return (
    <Card accent={statusColor}>
      <CardHeader
        title="KnowledgeService — Integrity"
        icon="🧠"
        status={data ? <StatusDot ok={issues === 0} warn={issues > 0 && issues < 10} /> : undefined}
        pill={data ? <Pill label={issues === 0 ? "clean" : `${issues} issues`} color={statusColor} /> : <Pill label="not checked" color={C.muted} />}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>
        {error && <div style={{ color: C.red, fontSize: 11 }}>Error: {error}</div>}

        {data && (
          <div style={{ display: "grid", gridTemplateColumns: "repeat(2, 1fr)", gap: 10 }}>
            {[
              ["Duplicates",      data.checks.duplicate_notes.count,           C.red],
              ["Orphaned chunks", data.checks.orphaned_chunks.count,           C.red],
              ["Bad consolidated",data.checks.suspicious_consolidated.count,   C.yellow],
              ["Empty notes",     data.checks.empty_notes.count,               C.yellow],
            ].map(([label, count, col]) => (
              <div key={label as string} style={{
                display: "flex", justifyContent: "space-between", alignItems: "center",
                padding: "5px 8px", background: C.panel, borderRadius: 4, border: `1px solid ${C.border}`,
              }}>
                <span style={{ fontSize: 10, color: C.dim }}>{label as string}</span>
                <span style={{ fontSize: 12, fontWeight: 700, color: (count as number) > 0 ? col as string : C.green }}>
                  {count as number}
                </span>
              </div>
            ))}
          </div>
        )}

        {checkedAt && (
          <div style={{ fontSize: 9, color: C.muted }}>Last checked: {relTime(checkedAt)}</div>
        )}

        <div style={{ display: "flex", gap: 8 }}>
          <ActionBtn
            label={loading ? "Checking…" : "Check Integrity"}
            color={C.accent}
            disabled={loading}
            onClick={runCheck}
          />
        </div>
      </div>
    </Card>
  );
}

// ── LLM Provider Card ──────────────────────────────────────────────────────────
function LlmCard() {
  const [data, setData]     = useState<{ models: Array<{ name: string; provider: string; active?: boolean }> } | null>(null);
  const [error, setError]   = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const fetch = useCallback(async () => {
    try {
      const raw = await callTool("list_models", {});
      const parsed = parseJson<{ models: Array<{ name: string; provider: string; active?: boolean }> }>(raw);
      if (parsed) { setData(parsed); setError(null); }
      else setError("bad response");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetch(); }, [fetch]);

  const activeModel = data?.models?.find(m => m.active) ?? data?.models?.[0];

  return (
    <Card accent={C.purple}>
      <CardHeader
        title="LLM Provider"
        icon="🤖"
        status={<StatusDot ok={!error} />}
        pill={activeModel ? <Pill label={activeModel.provider} color={C.purple} /> : undefined}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>
        {loading && <div style={{ color: C.muted, fontSize: 11 }}>Loading…</div>}
        {error   && <div style={{ color: C.red,   fontSize: 11 }}>Error: {error}</div>}

        {data && (
          <>
            {activeModel && (
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 12 }}>
                <Metric label="Provider" value={activeModel.provider} color={C.purple} />
                <Metric label="Model"    value={activeModel.name}     color={C.text} />
              </div>
            )}

            {data.models && data.models.length > 1 && (
              <div style={{ display: "flex", flexDirection: "column", gap: 4, maxHeight: 100, overflow: "auto" }}>
                {data.models.map((m) => (
                  <div key={m.name} style={{
                    display: "flex", justifyContent: "space-between",
                    fontSize: 10, padding: "3px 8px",
                    background: m.active ? `${C.purple}22` : C.panel,
                    borderRadius: 4, border: `1px solid ${m.active ? C.purple + "44" : C.border}`,
                    color: m.active ? C.text : C.dim,
                  }}>
                    <span>{m.name}</span>
                    <span style={{ color: C.purple }}>{m.provider}</span>
                  </div>
                ))}
              </div>
            )}

            <ActionBtn label="Refresh" onClick={fetch} />
          </>
        )}
      </div>
    </Card>
  );
}

// ── Services Overview ──────────────────────────────────────────────────────────
// Each card manages its own polling interval and push-notification subscription.
// No central clock here.
function ServicesView() {
  return (
    <div style={{ flex: 1, overflow: "auto", padding: "16px 20px" }}>
      <div style={{
        display: "grid",
        gridTemplateColumns: "repeat(auto-fill, minmax(420px, 1fr))",
        gap: 16,
        alignItems: "start",
      }}>
        <SchedulerCard />
        <QueueCard />
        <SnapshotCard />
        <IntegrityCard />
        <LlmCard />
      </div>
    </div>
  );
}

// ══════════════════════════════════════════════════════════════════════════════
//  DIAGRAM VIEW (unchanged SVG)
// ══════════════════════════════════════════════════════════════════════════════

interface Detail {
  title:  string;
  body:   string;
  items?: string[];
  color?: string;
}

const DETAILS: Record<string, Detail> = {
  "c-claude": {
    title: "🤖 Claude Code — MCP stdio client",
    color: C.accent,
    body:  "The primary development interface. Claude Code reads tool schemas from the server via the MCP initialize handshake and dispatches tool/call requests over stdin/stdout as line-delimited JSON-RPC 2.0.",
    items: [
      "Transport: stdio (McpServer wrapper)",
      "Protocol: MCP JSON-RPC 2.0",
      "Tool discovery: tools/list at startup",
      "No auth required for local use",
    ],
  },
  "c-http": {
    title: "🌐 HTTP Clients",
    color: C.accent,
    body:  "Any HTTP client that can speak the MCP handshake. Connect via the HTTP/SSE transport. Optional Bearer token auth via MCP_API_KEY env var.",
    items: [
      "curl — quick shell scripting",
      "Python requests / httpx",
      "Any OpenAPI-compatible REST client",
      "MCP_API_KEY=your-secret for auth",
      "Session handshake: initialize → notifications/initialized → tools/call",
    ],
  },
  "c-webui": {
    title: "💬 OpenWebUI",
    color: C.accent,
    body:  "Browser-based chat interface connecting to the /mcp endpoint via SSE streaming. Supports the /chat endpoint for agentic tool-use conversations with real-time streaming events.",
    items: [
      "MCP URL (Docker): http://host.docker.internal:3000/mcp",
      "MCP URL (host): http://localhost:3000/mcp",
      "Authentication: Bearer token (if MCP_API_KEY set)",
      "SSE events: thinking · tool_call · tool_result · message · done",
    ],
  },
  "c-scripts": {
    title: "📜 Self-Learn & Self-Reflect Scripts",
    color: C.accent,
    body:  "Python scripts that bootstrap the agent's self-knowledge and trigger autonomous learning cycles. They use the MCP HTTP transport to call tools and populate the knowledge graph.",
    items: [
      "scripts/self_learn.py — stores foundational knowledge notes",
      "scripts/self_reflect.py — triggers reflection and consolidation",
      "scripts/session.env — helper for interactive brain() shell function",
      "Connects via HTTP transport, performs initialize handshake",
    ],
  },
  "t-stdio": {
    title: "stdio Transport",
    color: C.cyan,
    body:  "The McpServer struct is a thin backward-compatible wrapper around McpServerCore for local clients (primarily Claude Code). Messages are line-delimited JSON-RPC 2.0 over stdin/stdout.",
    items: [
      "McpServer wraps McpServerCore",
      "Line-delimited JSON-RPC 2.0",
      "Started via: cargo run",
      "No auth, no HTTP overhead",
      "boot.yaml context protocol runs on startup",
    ],
  },
  "t-http": {
    title: "HTTP / SSE Transport",
    color: C.cyan,
    body:  "Axum-based HTTP server with Server-Sent Events streaming. Every client must complete the two-step MCP handshake before calling tools, or the server rejects requests with 'Server not initialized'.",
    items: [
      "Bind: MCP_HTTP_BIND (default 0.0.0.0:3000)",
      "Auth: Bearer token via MCP_API_KEY",
      "POST /mcp — JSON-RPC tool calls",
      "POST /chat — SSE streaming agentic chat",
      "GET  /health — liveness check",
      "SessionManager tracks per-connection state",
      "Handshake: initialize → notifications/initialized → tools/call",
    ],
  },
  "core": {
    title: "McpServerCore — Central Dispatcher",
    color: C.accent,
    body:  "The heart of the server. Holds the live ToolRegistry, ToolHandler, ChatService, SessionManager, and ContextBuilderService. All skill registrations go through build_skills().",
    items: [
      "ToolRegistry — lists all 78+ tools for tools/list responses",
      "ToolHandler  — routes tools/call to the correct skill handler",
      "ChatService  — runs the LLM tool-use loop with SSE streaming",
      "SessionManager — tracks HTTP session state per client",
      "ContextBuilder — loads 6 YAML profiles from contexts/",
      "build_skills() init order: DynamicSkill → QueueService → SchedulerService → ContextStore → register all → spawn coordinator",
      "McpServer is a thin stdio wrapper around McpServerCore",
    ],
  },
  "sk-memory": {
    title: "Memory Skills — 21 tools",
    color: C.green,
    body:  "Long-term and working memory. KnowledgeSkill is the core — it stores notes with hybrid BM25+vector embeddings, extracts entities, supports spaced-repetition, and reasons over the graph.",
    items: [
      "KnowledgeSkill (15): store_note, search_notes, find_related_notes, list_notes, get_note, delete_note, update_note, prune_old_notes, consolidate_memories, review_due_notes, search_by_entity, reason, audit_action, explain_reasoning, export_graph_visualization",
      "WorkingMemorySkill (4): push_context, get_context, summarise_session, list_sessions",
      "ProcedureSkill (2): store_procedure, search_procedures",
      "search_notes: hybrid BM25 + 1024-dim bge-m3 vectors with RRF + freshness boost",
      "consolidate_memories: LLM synthesis → SUMMARIZED_BY edges; auto-snapshots first",
      "Long notes (>1500 chars) are chunked with PART_OF edges",
    ],
  },
  "sk-auto": {
    title: "Automation Skills — 17+ tools",
    color: C.purple,
    body:  "Background job execution and autonomous self-improvement. The scheduler wakes every 5 minutes, perceives failures and stale memory, and dispatches LLM job chains without human input.",
    items: [
      "AgentSkill (6): enqueue_jobs, queue_status, cancel_job, retry_job, set_worker_config, drain_queue (+ get_job_result as dynamic tool)",
      "SchedulerSkill (5): start_scheduler, stop_scheduler, get_scheduler_status, configure_scheduler, run_scheduler_tick",
      "DynamicSkill (4+N): define_tool, execute_procedure, list_dynamic_tools, remove_dynamic_tool",
      "Job queue: BinaryHeap priority 0–3, per-provider semaphores (Ollama×3, Anthropic×2, Gemini×5)",
      "Job chaining: step 2..N stored as 'parked'; promoted on predecessor success",
      "Scheduler perception scan: detects ≥3 tool failures in 7 days → creates analysis tasks",
      "DynamicSkill: new tools hot-registered immediately without restart",
    ],
  },
  "sk-data": {
    title: "Data Skills — 20 tools",
    color: C.green,
    body:  "Goal tracking, graph maintenance, and context profile management. TaskSkill uses LLM decomposition to break goals into subtasks. AdminSkill keeps the Neo4j graph healthy.",
    items: [
      "TaskSkill (6): create_task, reflect_on_work, decompose_goal, update_task, list_tasks, record_outcome",
      "AdminSkill (10): delete_api, purge_duplicate_endpoints, purge_orphaned_schemas, reset_graph, backfill_endpoint_embeddings, snapshot_knowledge, restore_knowledge, list_snapshots, verify_knowledge_integrity, analyze_own_structure",
      "ContextSkill (4): list_context_profiles, get_context_profile, auto_assign_context, build_agent_context",
      "decompose_goal: LLM → SUBTASK_OF edges + DEPENDS_ON edges",
      "record_outcome(success=false): auto-enqueues reflect_on_work → store_note chain",
      "Context profiles: general · knowledge-worker · task-manager · code-analyst · api-builder · scheduler",
    ],
  },
  "sk-ext": {
    title: "External Skills — 22 tools",
    color: C.orange,
    body:  "Integrations with external APIs, LLM model management, web search, and telemetry export. ApiSkill includes LLM-powered self-healing: on 4xx/5xx it corrects the request and persists a HealingEvent.",
    items: [
      "ApiSkill (14): ingest_openapi, graph_query_endpoint, execute_http_request, get_api_context, list_loaded_apis, clear_api_context, discover_openapi, build_openapi_from_docs, build_openapi_from_repo, export_openapi, diff_api_spec, configure_api_credential, list_api_credentials, delete_api_credential",
      "ModelSkill (4): list_models, use_model, select_model, reload_models",
      "SearchSkill (1): search_web (SerpApi / Brave / Google Custom Search)",
      "SleepSkill (2): digest_experiences (→ JSONL training data), analyze_gaps (DuckDB)",
      "Self-healing: 4xx/5xx → LLM corrects payload → retry → HealingEvent node on success",
      "select_model: capability filter + cheapest-first sort from registered ModelSpec nodes",
    ],
  },
  "svc": {
    title: "Services Layer",
    color: C.cyan,
    body:  "Business logic sitting between the skills and the repository/Neo4j layer. Each service is constructed once in build_skills() and shared via Arc references.",
    items: [
      "KnowledgeService — RAG pipeline: BM25 + vector → RRF merge + freshness boost; auto-snapshots",
      "QueueService     — BinaryHeap + Tokio coordinator; per-provider semaphores; Neo4j persistence",
      "SchedulerService — background Tokio task; goal_to_steps() heuristic; perception_scan()",
      "HealingService   — LLM error analysis; corrects & retries; persists HealingEvent nodes",
      "SnapshotService  — gzip JSON snapshots (.json.gz via flate2); MERGE-safe restore",
      "ModelSelector    — capability-match filter → sort by combined cost/1k tokens",
      "ContextBuilderService — loads YAML profiles; auto_assign by keyword; builds bundles",
    ],
  },
  "i-neo4j": {
    title: "Neo4j Graph DB",
    color: C.green,
    body:  "The persistent brain. All knowledge, tasks, jobs, and API schemas are stored as nodes with typed edges. Vector embeddings (1024-dim bge-m3) sit on Note nodes for semantic search.",
    items: [
      "Driver: neo4rs (bolt://localhost:7687 default)",
      "Nodes: Note · Entity · Task · AgentJob · Endpoint · Schema · Parameter · Procedure · ModelSpec · WorkingMemory · Resource",
      "Edges: RELATES_TO · MENTIONS · PART_OF · SUMMARIZED_BY · REFLECTS_ON · DERIVED_FROM · SUBTASK_OF · DEPENDS_ON · RETURNS_SCHEMA · ACCEPTS_SCHEMA · LINKS_TO",
      "Vector index: 1024-dim bge-m3 embeddings on Note.embedding",
      "BM25 full-text index on Note.content",
      "MERGE-based restore — safe on non-empty graph",
      "Snapshots: compressed .json.gz (embeddings excluded, use backfill after restore)",
    ],
  },
  "i-llm": {
    title: "LLM Providers — 4 backends",
    color: C.purple,
    body:  "All four providers implement the LlmProvider trait. Switch at runtime with the use_model tool. The active provider is shared as Arc<RwLock<Option<LlmConfig>>> so all skills see the change immediately.",
    items: [
      "Ollama   — local inference, granite3.3:8b default, also serves bge-m3 embeddings",
      "Anthropic — claude-* via Messages API; native tool_use blocks for ChatService",
      "Gemini   — Google generativeLanguage API",
      "vLLM     — OpenAI-compatible (LM Studio, Groq, Together AI, any /v1/chat/completions)",
      "Config env: LLM_PROVIDER · OLLAMA_MODEL · ANTHROPIC_API_KEY · GEMINI_API_KEY · VLLM_URL",
      "Runtime switch: use_model tool accepts provider + model + optional api_key",
      "Per-provider job semaphores: Ollama×3, Anthropic×2, Gemini×5",
    ],
  },
  "i-secrets": {
    title: "Secret Store — 3 backends",
    color: C.yellow,
    body:  "Stores API credentials for automatic injection into execute_http_request calls. Three backend options selected by SECRET_PROVIDER env var.",
    items: [
      "local — AES-256-GCM encrypted .secrets.enc; key via SECRETS_ENCRYPTION_KEY",
      "vault — HashiCorp Vault KV v2; VAULT_ADDR + VAULT_TOKEN + optional VAULT_NAMESPACE",
      "aws   — AWS Secrets Manager; AWS_REGION + AWS_SECRET_PREFIX",
      "none  — plaintext (dev only)",
      "configure_api_credential: stores key for named API; injected as header or query param",
      "Injection types: api_key · bearer · basic · oauth2_client_credentials",
    ],
  },
  "i-persist": {
    title: "Telemetry & Snapshots",
    color: C.red,
    body:  "Two independent persistence channels: DuckDB for interaction telemetry (training data), and gzip JSON snapshots for full knowledge graph backups.",
    items: [
      "DuckDB (brain_logs.db) — TELEMETRY_DB_PATH env var",
      "digest_experiences — exports successful interactions to JSONL for fine-tuning",
      "analyze_gaps — reads knowledge_gaps table to identify weak areas",
      "Snapshots: /home/agent/snapshots/*.json.gz via SnapshotService (flate2)",
      "AUTO_SNAPSHOT_BEFORE_CONSOLIDATION=true — safety net before every LLM consolidation",
      "AUTO_SNAPSHOT_BEFORE_PRUNE=false — optional; enable for extra safety on prune",
      "list_snapshots returns newest-first with node counts and file sizes",
      "restore_knowledge uses MERGE — safe to run on non-empty graph",
    ],
  },
};

type HId = string | null;

function Box({
  id, x, y, w, h, rx = 6,
  base, hover, stroke, hstroke,
  hovered, onHover, onSelect, children,
}: {
  id: string; x: number; y: number; w: number; h: number; rx?: number;
  base: string; hover: string; stroke: string; hstroke: string;
  hovered: HId; onHover: (id: HId) => void; onSelect: (id: string) => void;
  children?: React.ReactNode;
}) {
  const isH = hovered === id;
  return (
    <g
      onMouseEnter={() => onHover(id)}
      onMouseLeave={() => onHover(null)}
      onClick={() => onSelect(id)}
      style={{ cursor: "pointer" }}
    >
      <rect x={x} y={y} width={w} height={h} rx={rx} fill={isH ? hover : base} stroke={isH ? hstroke : stroke} strokeWidth={isH ? 2 : 1.5} />
      {children}
    </g>
  );
}

function Chip({ x, y, w = 196, label, sub, col }: {
  x: number; y: number; w?: number; label: string; sub: string; col: string;
}) {
  return (
    <g>
      <rect x={x} y={y} width={w} height={28} rx={4} fill={C.card} stroke={C.border} strokeWidth={1} />
      <text x={x + w / 2} y={y + 12} textAnchor="middle" fill={col}   fontSize={9} fontWeight={600}>{label}</text>
      <text x={x + w / 2} y={y + 23} textAnchor="middle" fill={C.dim} fontSize={7.5}>{sub}</text>
    </g>
  );
}

function Conn({ x1, y1, x2, y2, col, marker }: {
  x1: number; y1: number; x2: number; y2: number; col: string; marker: string;
}) {
  return (
    <line x1={x1} y1={y1} x2={x2} y2={y2} stroke={col} strokeWidth={1} opacity={0.45} strokeDasharray="4,3" markerEnd={`url(#arr-${marker})`} />
  );
}

function DetailModal({ id, onClose }: { id: string; onClose: () => void }) {
  const d = DETAILS[id];
  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);
  if (!d) return null;
  const accentCol = d.color ?? C.accent;
  return (
    <div onClick={onClose} style={{ position: "fixed", inset: 0, zIndex: 100, background: "rgba(0,0,0,0.6)", display: "flex", alignItems: "center", justifyContent: "center", backdropFilter: "blur(2px)" }}>
      <div onClick={e => e.stopPropagation()} style={{ background: C.panel, border: `1px solid ${accentCol}55`, borderRadius: 10, width: "min(560px, 90vw)", maxHeight: "80vh", overflow: "hidden", display: "flex", flexDirection: "column", boxShadow: `0 0 40px ${accentCol}22`, fontFamily: "'JetBrains Mono','Fira Code',monospace" }}>
        <div style={{ padding: "14px 18px", borderBottom: `1px solid ${C.border}`, display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
          <div style={{ fontSize: 13, fontWeight: 700, color: accentCol }}>{d.title}</div>
          <button onClick={onClose} style={{ background: "none", border: "none", color: C.dim, cursor: "pointer", fontSize: 16, lineHeight: 1, padding: "2px 6px", borderRadius: 4 }}>✕</button>
        </div>
        <div style={{ overflow: "auto", padding: "16px 18px", display: "flex", flexDirection: "column", gap: 12 }}>
          <p style={{ fontSize: 12, color: C.text, lineHeight: 1.7, margin: 0 }}>{d.body}</p>
          {d.items && d.items.length > 0 && (
            <ul style={{ margin: 0, paddingLeft: 0, listStyle: "none", display: "flex", flexDirection: "column", gap: 6 }}>
              {d.items.map((item, i) => (
                <li key={i} style={{ display: "flex", gap: 8, fontSize: 11, color: C.dim, lineHeight: 1.6 }}>
                  <span style={{ color: accentCol, flexShrink: 0, marginTop: 1 }}>›</span>
                  <span>{item}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
        <div style={{ padding: "8px 18px", borderTop: `1px solid ${C.border}`, fontSize: 10, color: C.muted, textAlign: "right" }}>click outside or press Esc to close</div>
      </div>
    </div>
  );
}

function DiagramView() {
  const [hovered,  setHovered]  = useState<HId>(null);
  const [selected, setSelected] = useState<HId>(null);

  const boxProps = (id: string, base: string, hover: string, stroke: string, hstroke: string, rx = 6) =>
    ({ id, base, hover, stroke, hstroke, rx, hovered, onHover: setHovered, onSelect: setSelected });

  return (
    <div style={{ flex: 1, overflow: "auto", padding: "16px 20px" }}>
      <svg viewBox="0 0 960 715" style={{ width: "100%", height: "auto", minWidth: 640 }} fontFamily="'JetBrains Mono','Fira Code',monospace">
        <defs>
          {([["b", C.accent], ["p", C.purple], ["c", C.cyan], ["g", C.green]] as [string,string][]).map(([id, col]) => (
            <marker key={id} id={`arr-${id}`} markerWidth={8} markerHeight={6} refX={8} refY={3} orient="auto">
              <polygon points="0 0,8 3,0 6" fill={col} opacity={0.7} />
            </marker>
          ))}
        </defs>

        <text x={480} y={20} textAnchor="middle" fill={C.text}  fontSize={12} fontWeight={700} letterSpacing={3}>AGENT BRAIN — SYSTEM ARCHITECTURE</text>
        <text x={480} y={34} textAnchor="middle" fill={C.dim}   fontSize={8}  letterSpacing={1}>78 MCP tools · 13 skills · Neo4j graph · 4 LLM providers · autonomous scheduler</text>
        <line x1={30} y1={41} x2={930} y2={41} stroke={C.border} strokeWidth={1} />

        <text x={30} y={57} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>CLIENT LAYER</text>
        <Box {...boxProps("c-claude",  "#101820","#182030", C.border, C.accent)} x={30}  y={61} w={213} h={44}>
          <text x={136} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>🤖 Claude Code</text>
          <text x={136} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>MCP stdio client</text>
        </Box>
        <Box {...boxProps("c-http",    "#101820","#182030", C.border, C.accent)} x={253} y={61} w={213} h={44}>
          <text x={359} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>🌐 HTTP Clients</text>
          <text x={359} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>curl · requests · REST</text>
        </Box>
        <Box {...boxProps("c-webui",   "#101820","#182030", C.border, C.accent)} x={476} y={61} w={213} h={44}>
          <text x={582} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>💬 OpenWebUI</text>
          <text x={582} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>SSE streaming browser</text>
        </Box>
        <Box {...boxProps("c-scripts", "#101820","#182030", C.border, C.accent)} x={699} y={61} w={231} h={44}>
          <text x={814} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>📜 Self-Learn Scripts</text>
          <text x={814} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>self_learn.py · self_reflect.py</text>
        </Box>
        {[136, 359, 582, 814].map(cx => <Conn key={cx} x1={cx} y1={105} x2={cx} y2={130} col={C.accent} marker="b" />)}

        <text x={30} y={130} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>TRANSPORT</text>
        <Box {...boxProps("t-stdio", "#0c1318","#121c24", C.border, C.cyan)} x={30}  y={134} w={200} h={50}>
          <text x={130} y={153} textAnchor="middle" fill={C.cyan}  fontSize={10} fontWeight={600}>stdio</text>
          <text x={130} y={168} textAnchor="middle" fill={C.dim}   fontSize={8}>McpServer wrapper</text>
          <text x={130} y={179} textAnchor="middle" fill={C.muted} fontSize={7}>line-delimited JSON-RPC 2.0</text>
        </Box>
        <Box {...boxProps("t-http", "#0c1318","#121c24", C.border, C.cyan)} x={240} y={134} w={690} h={50}>
          <text x={585} y={153} textAnchor="middle" fill={C.cyan}  fontSize={10} fontWeight={600}>HTTP / SSE Transport</text>
          <text x={585} y={168} textAnchor="middle" fill={C.dim}   fontSize={8}>Axum · SessionManager · MCP_API_KEY auth · /chat streaming · /health</text>
          <text x={585} y={179} textAnchor="middle" fill={C.muted} fontSize={7}>initialize → notifications/initialized → tools/call</text>
        </Box>
        <Conn x1={480} y1={184} x2={480} y2={208} col={C.cyan} marker="c" />

        <text x={30} y={208} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>CORE</text>
        <Box {...boxProps("core", "#0d1420","#141d30","#2a4a8a", C.accent, 8)} x={30} y={212} w={900} h={82}>
          <text x={480} y={230} textAnchor="middle" fill={C.accent} fontSize={11} fontWeight={700}>McpServerCore</text>
          {([
            { x: 46,  label: "ToolRegistry",   sub: "78+ tools listed" },
            { x: 222, label: "ToolHandler",    sub: "dispatches tool/call" },
            { x: 398, label: "ChatService",    sub: "tool-use loop · SSE" },
            { x: 574, label: "SessionManager", sub: "HTTP session state" },
            { x: 750, label: "ContextBuilder", sub: "6 YAML profiles" },
          ] as { x: number; label: string; sub: string }[]).map(({ x, label, sub }) => (
            <g key={label}>
              <rect x={x} y={238} width={160} height={46} rx={4} fill={C.card} stroke={C.border} strokeWidth={1} />
              <text x={x + 80} y={255} textAnchor="middle" fill={C.text} fontSize={9}  fontWeight={600}>{label}</text>
              <text x={x + 80} y={269} textAnchor="middle" fill={C.dim}  fontSize={7.5}>{sub}</text>
            </g>
          ))}
        </Box>
        <Conn x1={480} y1={294} x2={480} y2={316} col={C.purple} marker="p" />

        <text x={30} y={316} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>SKILLS — 13 GROUPS · 78+ TOOLS</text>
        <Box {...boxProps("sk-memory", "#0c1218","#111c28","#2a3550", C.accent)} x={30}  y={322} w={222} h={152}>
          <text x={141} y={338} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={700}>MEMORY</text>
          <Chip x={42}  y={345} label="KnowledgeSkill"     sub="15 tools · RAG · entities · spaced-rep" col={C.green}  />
          <Chip x={42}  y={377} label="WorkingMemorySkill" sub="4 tools · session scratchpad"            col={C.green}  />
          <Chip x={42}  y={409} label="ProcedureSkill"     sub="2 tools · stored workflows"              col={C.green}  />
          <text x={141} y={453} textAnchor="middle" fill={C.accent} fontSize={8}>21 tools total</text>
          <text x={141} y={464} textAnchor="middle" fill={C.muted}  fontSize={7}>BM25 · vector · RRF</text>
        </Box>
        <Box {...boxProps("sk-auto", "#0c0c18","#121228","#352a50", C.purple)} x={262} y={322} w={222} h={152}>
          <text x={373} y={338} textAnchor="middle" fill={C.purple} fontSize={10} fontWeight={700}>AUTOMATION</text>
          <Chip x={274} y={345} label="AgentSkill"     sub="8 tools · job queue · chaining"  col={C.purple} />
          <Chip x={274} y={377} label="SchedulerSkill" sub="5 tools · autonomous tick loop"  col={C.purple} />
          <Chip x={274} y={409} label="DynamicSkill"   sub="4+N tools · runtime definition"  col={C.purple} />
          <text x={373} y={453} textAnchor="middle" fill={C.purple} fontSize={8}>17+ tools total</text>
          <text x={373} y={464} textAnchor="middle" fill={C.muted}  fontSize={7}>Tokio · BinaryHeap</text>
        </Box>
        <Box {...boxProps("sk-data", "#0c1410","#121e14","#2a4030", C.green)} x={494} y={322} w={222} h={152}>
          <text x={605} y={338} textAnchor="middle" fill={C.green} fontSize={10} fontWeight={700}>DATA</text>
          <Chip x={506} y={345} label="TaskSkill"    sub="6 tools · goals · decompose · reflect" col={C.cyan} />
          <Chip x={506} y={377} label="AdminSkill"   sub="10 tools · graph maintenance"           col={C.cyan} />
          <Chip x={506} y={409} label="ContextSkill" sub="4 tools · profile management"           col={C.cyan} />
          <text x={605} y={453} textAnchor="middle" fill={C.green} fontSize={8}>20 tools total</text>
          <text x={605} y={464} textAnchor="middle" fill={C.muted} fontSize={7}>Neo4j · snapshots</text>
        </Box>
        <Box {...boxProps("sk-ext", "#140e08","#1e1610","#3a2e10", C.yellow)} x={726} y={322} w={204} h={152}>
          <text x={828} y={338} textAnchor="middle" fill={C.yellow} fontSize={10} fontWeight={700}>EXTERNAL</text>
          <Chip x={738} y={345} w={180} label="ApiSkill"     sub="14 tools · OpenAPI · self-heal"   col={C.orange} />
          <Chip x={738} y={377} w={180} label="ModelSkill"   sub="5 tools · LLM registry + select"  col={C.orange} />
          <Chip x={738} y={409} w={180} label="Search+Sleep" sub="3 tools · web · telemetry"         col={C.orange} />
          <text x={828} y={453} textAnchor="middle" fill={C.yellow} fontSize={8}>22 tools total</text>
          <text x={828} y={464} textAnchor="middle" fill={C.muted}  fontSize={7}>SerpApi · DuckDB</text>
        </Box>
        <Conn x1={480} y1={474} x2={480} y2={496} col={C.cyan} marker="c" />

        <text x={30} y={496} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>SERVICES</text>
        <Box {...boxProps("svc", "#0c1018","#111520", C.border, C.cyan)} x={30} y={500} w={900} h={50}>
          {([
            { x: 46,  w: 157, name: "KnowledgeService", sub: "RAG · BM25 · snapshots" },
            { x: 213, w: 157, name: "QueueService",     sub: "heap · semaphores · coord" },
            { x: 380, w: 157, name: "SchedulerService", sub: "Tokio task · perception" },
            { x: 547, w: 157, name: "HealingService",   sub: "LLM correction · events" },
            { x: 714, w: 206, name: "SnapshotService + ModelSelector", sub: "gzip backup · capability match" },
          ] as { x: number; w: number; name: string; sub: string }[]).map(({ x, w, name, sub }) => (
            <g key={name}>
              <rect x={x} y={506} width={w} height={38} rx={4} fill={C.card} stroke={C.border} strokeWidth={1} />
              <text x={x + w / 2} y={521} textAnchor="middle" fill={C.text} fontSize={9}  fontWeight={600}>{name}</text>
              <text x={x + w / 2} y={536} textAnchor="middle" fill={C.dim}  fontSize={7.5}>{sub}</text>
            </g>
          ))}
        </Box>
        <Conn x1={480} y1={550} x2={480} y2={572} col={C.green} marker="g" />

        <text x={30} y={572} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>INFRASTRUCTURE</text>
        <Box {...boxProps("i-neo4j",  "#0a1410","#111e16","#2a4030", C.green)}  x={30}  y={576} w={218} h={104}>
          <text x={139} y={594} textAnchor="middle" fill={C.green}  fontSize={10} fontWeight={700}>Neo4j Graph DB</text>
          <text x={139} y={608} textAnchor="middle" fill={C.dim}    fontSize={8}>neo4rs · bolt://localhost:7687</text>
          <text x={139} y={622} textAnchor="middle" fill={C.dim}    fontSize={7.5}>Note · Entity · Task · Endpoint</text>
          <text x={139} y={634} textAnchor="middle" fill={C.dim}    fontSize={7.5}>Schema · AgentJob · Procedure</text>
          <text x={139} y={646} textAnchor="middle" fill={C.green}  fontSize={7.5}>BM25 + 1024-dim bge-m3 vectors</text>
          <text x={139} y={658} textAnchor="middle" fill={C.muted}  fontSize={7}>MERGE-safe · 11 edge types</text>
          <text x={139} y={671} textAnchor="middle" fill={C.dim}    fontSize={7.5}>RRF hybrid search + freshness</text>
        </Box>
        <Box {...boxProps("i-llm",    "#0e0b14","#14102a","#352a40", C.purple)} x={258} y={576} w={285} h={104}>
          <text x={400} y={594} textAnchor="middle" fill={C.purple} fontSize={10} fontWeight={700}>LLM Providers</text>
          {([
            [270, 610, "🦙 Ollama",     "granite3.3:8b · local · embeddings"],
            [270, 626, "🔮 Anthropic",  "claude-* · Messages API · tool_use"],
            [270, 642, "✨ Gemini",     "generativeLanguage API"],
            [270, 658, "⚡ vLLM",      "OpenAI-compat · any server"],
          ] as [number,number,string,string][]).map(([x, y, name, info]) => (
            <g key={name}>
              <text x={x}      y={y} fill={C.orange} fontSize={9}>{name}</text>
              <text x={x + 88} y={y} fill={C.dim}    fontSize={7.5}>{info}</text>
            </g>
          ))}
          <text x={400} y={674} textAnchor="middle" fill={C.purple} fontSize={7.5}>runtime switch via use_model</text>
        </Box>
        <Box {...boxProps("i-secrets", "#12100a","#1c180e","#3a3010", C.yellow)} x={553} y={576} w={190} h={104}>
          <text x={648} y={594} textAnchor="middle" fill={C.yellow} fontSize={10} fontWeight={700}>Secret Store</text>
          <text x={648} y={608} textAnchor="middle" fill={C.dim}    fontSize={8}>automatic key injection</text>
          <text x={563} y={624} fill={C.dim} fontSize={7.5}>Local  — AES-256-GCM</text>
          <text x={563} y={638} fill={C.dim} fontSize={7.5}>Vault  — HashiCorp KV v2</text>
          <text x={563} y={652} fill={C.dim} fontSize={7.5}>AWS    — Secrets Manager</text>
          <text x={648} y={668} textAnchor="middle" fill={C.yellow} fontSize={7.5}>configure_api_credential</text>
        </Box>
        <Box {...boxProps("i-persist", "#140c0c","#1e1010","#3a1818", C.red)} x={753} y={576} w={177} h={104}>
          <text x={841} y={594} textAnchor="middle" fill={C.red}   fontSize={10} fontWeight={700}>Telemetry + Snapshots</text>
          <text x={841} y={608} textAnchor="middle" fill={C.dim}   fontSize={7.5}>DuckDB brain_logs.db</text>
          <text x={841} y={622} textAnchor="middle" fill={C.dim}   fontSize={7.5}>digest_experiences → JSONL</text>
          <text x={841} y={636} textAnchor="middle" fill={C.dim}   fontSize={7.5}>analyze_gaps → knowledge</text>
          <text x={841} y={650} textAnchor="middle" fill={C.dim}   fontSize={7.5}>/home/agent/snapshots/*.json.gz</text>
          <text x={841} y={664} textAnchor="middle" fill={C.dim}   fontSize={7.5}>auto pre_consolidate backup</text>
          <text x={841} y={676} textAnchor="middle" fill={C.red}   fontSize={7.5}>SleepSkill · AdminSkill</text>
        </Box>

        <path d="M 930 398 C 948 398 948 528 930 528" fill="none" stroke={C.purple} strokeWidth={1.5} strokeDasharray="5,3" opacity={0.6} markerStart="url(#arr-p)" />
        <text x={952} y={466} textAnchor="middle" fill={C.purple} fontSize={7.5} fontWeight={600} transform="rotate(90 952 466)">AUTONOMOUS SELF-IMPROVEMENT</text>

        <line x1={30} y1={692} x2={930} y2={692} stroke={C.border} strokeWidth={1} />
        <text x={36} y={705} fill={C.muted} fontSize={7.5}>FLOW:</text>
        {([
          [68,  C.accent, "MCP call"],
          [148, C.purple, "skill dispatch"],
          [255, C.cyan,   "service call"],
          [348, C.green,  "DB query"],
        ] as [number, string, string][]).map(([x, col, label]) => (
          <g key={label}>
            <line x1={x} y1={702} x2={x+28} y2={702} stroke={col} strokeWidth={1.5} strokeDasharray="4,2" opacity={0.7} />
            <text x={x+32} y={705} fill={C.dim} fontSize={7.5}>{label}</text>
          </g>
        ))}
        <text x={460} y={705} fill={C.muted} fontSize={7.5}>
          ↻ SchedulerService ticks every 5 min · perceives failures · enqueues LLM chains
        </text>
      </svg>

      {selected && <DetailModal id={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

// ── Main panel ────────────────────────────────────────────────────────────────
type Tab = "diagram" | "services";

export default function ArchitecturePanel() {
  const [tab, setTab] = useState<Tab>("diagram");

  return (
    <div style={{ flex: 1, overflow: "hidden", background: C.bg, display: "flex", flexDirection: "column", fontFamily: "'JetBrains Mono','Fira Code',monospace" }}>

      {/* Header with tab bar */}
      <div style={{
        padding: "0 20px",
        borderBottom: `1px solid ${C.border}`,
        background: C.panel,
        display: "flex", alignItems: "center", gap: 0, flexShrink: 0,
      }}>
        <span style={{
          fontSize: 11, fontWeight: 700, letterSpacing: "0.1em",
          textTransform: "uppercase", color: C.accent,
          padding: "14px 0", marginRight: 20,
          display: "flex", alignItems: "center", gap: 8,
        }}>
          🏗 Architecture
          <span style={{ fontSize: 10, background: "#2a4a8a", color: C.accent, padding: "1px 6px", borderRadius: 10 }}>
            78 tools · 13 skills
          </span>
        </span>

        {(["diagram", "services"] as Tab[]).map(t => (
          <button
            key={t}
            onClick={() => setTab(t)}
            style={{
              fontSize: 10, fontWeight: 600, padding: "14px 16px",
              background: "none", border: "none",
              borderBottom: tab === t ? `2px solid ${C.accent}` : "2px solid transparent",
              color: tab === t ? C.accent : C.dim,
              cursor: "pointer", textTransform: "uppercase", letterSpacing: "0.08em",
              fontFamily: "inherit", transition: "color 0.15s",
            }}
          >{t === "diagram" ? "Diagram" : "Services Live"}</button>
        ))}

        {tab === "services" && (
          <span style={{ marginLeft: "auto", fontSize: 10, color: C.muted, padding: "14px 0" }}>
            queue: SSE push + 5s · scheduler: SSE push + 20s · snapshots: 2m · others: on demand
          </span>
        )}
        {tab === "diagram" && (
          <span style={{ marginLeft: "auto", fontSize: 10, color: C.muted, padding: "14px 0" }}>
            click any block for details
          </span>
        )}
      </div>

      {tab === "diagram"  && <DiagramView />}
      {tab === "services" && <ServicesView />}
    </div>
  );
}
