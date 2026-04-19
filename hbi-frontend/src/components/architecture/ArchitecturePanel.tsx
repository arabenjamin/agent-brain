import { useEffect, useState, useCallback } from "react";
import { callTool, onNotification } from "../../api/mcp";
import { getBrainUrl, getApiKey } from "../../api/config";

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

interface ContextProfile {
  name: string;
  description?: string;
  tools?: string[];
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
      const res = await window.fetch(`${getBrainUrl()}/api/scheduler/status`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const parsed = await res.json() as SchedulerStatus;
      setData(parsed);
      setError(null);
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
      await callTool("scheduler_control", { action: "stop" });
      showToast("Scheduler paused");
    } else {
      await callTool("scheduler_control", { action: "start" });
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
      const res = await window.fetch(`${getBrainUrl()}/api/queue/status`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const parsed = await res.json() as QueueStatus;
      setData(parsed);
      setError(null);
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
    await window.fetch(`${getBrainUrl()}/api/queue/drain`, {
      method: "POST",
      headers: { Authorization: `Bearer ${getApiKey()}` },
    });
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

// ── Context Profiles Card ─────────────────────────────────────────────────────
function ContextProfilesCard() {
  const [profiles, setProfiles] = useState<ContextProfile[]>([]);
  const [error, setError]       = useState<string | null>(null);
  const [loading, setLoading]   = useState(true);

  const fetch = useCallback(async () => {
    try {
      const res = await window.fetch(`${getBrainUrl()}/api/contexts`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const json = await res.json() as { profiles?: ContextProfile[] } | ContextProfile[];
      const list: ContextProfile[] = Array.isArray(json) ? json : (json.profiles ?? []);
      setProfiles(list);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetch(); }, [fetch]);

  const statusColor = error ? C.red : profiles.length > 0 ? C.cyan : C.muted;

  return (
    <Card accent={statusColor}>
      <CardHeader
        title="Context Profiles"
        icon="🧩"
        status={<StatusDot ok={!error && profiles.length > 0} />}
        pill={<Pill label={`${profiles.length} profiles`} color={statusColor} />}
      />
      <div style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 12 }}>
        {loading && <div style={{ color: C.muted, fontSize: 11 }}>Loading…</div>}
        {error   && <div style={{ color: C.red,   fontSize: 11 }}>Error: {error}</div>}

        {profiles.length > 0 && (
          <div style={{ display: "flex", flexDirection: "column", gap: 4, maxHeight: 180, overflow: "auto" }}>
            {profiles.map((p) => (
              <div key={p.name} style={{
                display: "flex", flexDirection: "column", gap: 2,
                padding: "5px 8px", background: C.panel,
                borderRadius: 4, border: `1px solid ${C.border}`,
              }}>
                <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                  <span style={{ fontSize: 10, fontWeight: 600, color: C.cyan }}>{p.name}</span>
                  {p.tools && (
                    <span style={{ fontSize: 9, color: C.muted }}>{p.tools.length} tools</span>
                  )}
                </div>
                {p.description && (
                  <span style={{ fontSize: 9, color: C.dim, lineHeight: 1.4 }}>{p.description}</span>
                )}
              </div>
            ))}
          </div>
        )}

        <ActionBtn label="Refresh" onClick={fetch} />
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
interface LlmCardModel { name: string; provider: string; active?: boolean; }
interface LlmCardData { models: LlmCardModel[]; }

function LlmCard() {
  const [data, setData]       = useState<LlmCardData | null>(null);
  const [error, setError]     = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const fetch = useCallback(async () => {
    try {
      const res = await window.fetch(`${getBrainUrl()}/api/models`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const json = await res.json() as {
        active_provider?: string;
        active_model?: string;
        catalog_models?: Array<{ name: string; provider: string; model?: string }>;
      };
      // Normalise REST response into the shape the card renders.
      const catalogModels: LlmCardModel[] = (json.catalog_models ?? []).map((m) => ({
        name: m.model ?? m.name,
        provider: m.provider,
        active:
          m.provider.toLowerCase() === (json.active_provider ?? "").toLowerCase() &&
          (m.model ?? m.name) === (json.active_model ?? ""),
      }));
      // If catalog is empty, synthesise a single entry from the active config.
      const models: LlmCardModel[] =
        catalogModels.length > 0
          ? catalogModels
          : json.active_model
            ? [{ name: json.active_model, provider: json.active_provider ?? "unknown", active: true }]
            : [];
      setData({ models });
      setError(null);
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
        <ContextProfilesCard />
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
    title: "McpServerCore — MCP Protocol Adapter",
    color: C.cyan,
    body:  "Thin protocol adapter that wraps BrainCore. Owns only MCP-specific concerns: the JSON-RPC session state machine, HTTP session management, and an optional separate LLM config for /chat. All skills, tools, and services live in BrainCore.",
    items: [
      "ServerState — JSON-RPC lifecycle: Uninitialized → Initializing → Running",
      "SessionManager — tracks per-connection state for HTTP/SSE clients",
      "ChatService — wires brain.tool_handler + brain.tool_registry into SSE tool-use loop",
      "chat_llm_config — optional separate LLM for /chat (e.g. cloud Anthropic while brain uses local Ollama)",
      "McpServer — thin stdio wrapper around McpServerCore for backward-compatible local use",
      "All skills, the LLM Arc, and all services live in BrainCore — McpServerCore only delegates",
    ],
  },
  "brain": {
    title: "BrainCore — The Central Engine",
    color: C.accent,
    body:  "The true heart of the system. BrainCore owns all stateful services: storage, LLM config, skill/tool registry, background jobs, context profiles, and the internal event bus. McpServerCore and the transports are just protocol adapters on top.",
    items: [
      "ToolRegistry — Arc<RwLock<ToolRegistry>>: 47 tools registered here; serves tools/list",
      "ToolHandler  — Arc<RwLock<Option<ToolHandler>>>: routes tools/call to the correct skill",
      "LlmConfig    — Arc<RwLock<Option<LlmConfig>>>: live-swappable Arc shared by all skills",
      "StorageConfig — Neo4j client + DuckDB telemetry + dataset dir + secrets provider",
      "JobServices   — Arc-wrapped QueueService + SchedulerService (both Option until build_skills)",
      "ContextBuilderService — 7 profiles + boot/init protocols; lazy-init in build_skills()",
      "EventBus — broadcast::Sender<BrainEvent>: scheduler ticked/slept/woke events",
      "initialize() calls build_skills() then seeds Neo4j schema and runs boot.yaml protocol",
    ],
  },
  "sk-memory": {
    title: "Memory Skills — 9 tools",
    color: C.green,
    body:  "Long-term, working, and resource memory. KnowledgeSkill is the core — it stores notes with hybrid BM25+vector embeddings and extracts entities.",
    items: [
      "KnowledgeSkill (6): store_note, search_notes, prune_old_notes, consolidate_memories, reason, synthesize_knowledge",
      "WorkingMemorySkill (2): push_context, summarise_session",
      "ResourceSkill (1): resource",
      "search_notes: hybrid BM25 + 1024-dim bge-m3 vectors with RRF + freshness boost",
      "consolidate_memories: LLM synthesis → SUMMARIZED_BY edges; auto-snapshots first",
      "Long notes (>1500 chars) are chunked with PART_OF edges",
    ],
  },
  "sk-auto": {
    title: "Automation Skills — 12 tools",
    color: C.purple,
    body:  "Background job execution and autonomous self-improvement. The scheduler wakes every 5 minutes and dispatches LLM job chains without human input.",
    items: [
      "AgentSkill (5): manage_job, set_worker_config, enqueue_jobs, dead_letter, update_job_progress",
      "SchedulerSkill (4): scheduler_control, run_scheduler_tick, manage_chain, manage_scheduled_task",
      "DynamicSkill (3): manage_dynamic_tool, execute_procedure, store_procedure",
      "Job queue: BinaryHeap priority 0–3, per-provider semaphores (Ollama×3, Anthropic×2, Gemini×5)",
      "Job chaining: step 2..N stored as 'parked'; promoted on predecessor success",
      "Scheduler perception scan: detects ≥3 tool failures in 7 days → creates analysis tasks",
      "DynamicSkill: new tools hot-registered immediately via Neo4j storage",
    ],
  },
  "sk-data": {
    title: "Data Skills — 8 tools",
    color: C.green,
    body:  "Goal tracking, graph maintenance, and context profile management. TaskSkill uses LLM decomposition to break goals into subtasks.",
    items: [
      "TaskSkill (5): create_task, reflect_on_work, decompose_goal, update_task, record_outcome",
      "QuerySkill (2): neo4j_query, duckdb_query",
      "ContextSkill (1): context",
      "decompose_goal: LLM → SUBTASK_OF edges + DEPENDS_ON edges",
      "record_outcome(success=false): auto-enqueues reflect_on_work → store_note chain",
      "Context profiles (7): general · knowledge-worker · task-manager · code-analyst · api-builder · scheduler · researcher",
      "Boot protocols (2): boot.yaml (every startup) · init.yaml (empty graph only)",
    ],
  },
  "sk-ext": {
    title: "External & Utility Skills — 18 tools",
    color: C.orange,
    body:  "Integrations with external APIs, model management, search, and codebase analysis.",
    items: [
      "HttpSkill (2): http_request, define_api_context",
      "CodebaseSkill (7): read_codebase_file, list_codebase_files, search_codebase, get_file_tree, get_git_log, get_git_diff, analyze_own_structure",
      "WsSkill (4): ws_connect, ws_send, ws_receive, ws_close",
      "ModelSkill (2): use_model, reload_models",
      "SearchSkill (1): search_web",
      "SleepSkill (2): digest_experiences, analyze_gaps",
    ],
  },
  "svc": {
    title: "Services Layer",
    color: C.cyan,
    body:  "Business logic sitting between the skills and the repository/Neo4j layer. Each service is constructed once in build_skills() and shared via Arc references.",
    items: [
      "KnowledgeService — RAG pipeline: BM25 + vector → RRF merge + freshness boost",
      "QueueService     — BinaryHeap + Tokio coordinator; per-provider semaphores; Neo4j persistence",
      "SchedulerService — background Tokio task; goal_to_steps() heuristic; perception_scan()",
      "SnapshotService  — gzip JSON snapshots (.json.gz via flate2); MERGE-safe restore",
      "SleepService     — experience digestion and training data export (JSONL)",
      "ContextBuilderService — 7 agent profiles + boot/init protocols from contexts/",
      "ResourceRegistry — named connection pool shared across skills (WsSkill sessions, tokens)",
      "ProcedureExecutor — template-substitution multi-step workflow runner",
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
      "Ollama      — local inference, qwen3.5:4b default, also serves bge-m3 embeddings",
      "OllamaCloud — Ollama Cloud via OpenAI-compat /v1/chat/completions; embeddings always local",
      "Anthropic   — claude-* via Messages API; native tool_use blocks for ChatService",
      "Gemini      — Google generativeLanguage API",
      "Config env: LLM_PROVIDER · OLLAMA_MODEL · OLLAMA_API_KEY · ANTHROPIC_API_KEY · GEMINI_API_KEY",
      "Runtime switch: use_model tool accepts provider + model + optional api_key",
      "Per-provider job semaphores: Ollama×3, Anthropic×2, Gemini×5",
      "Background jobs: provider_hint='ollama' always routes to OLLAMA_LOCAL_URL + OLLAMA_LOCAL_MODEL",
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
      "SnapshotService: gzip .json.gz via flate2; triggered by consolidate_memories flow",
      "SleepService: digest_experiences exports to JSONL; analyze_gaps reads DuckDB gaps table",
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
      <svg viewBox="0 0 960 750" style={{ width: "100%", height: "auto", minWidth: 640 }} fontFamily="'JetBrains Mono','Fira Code',monospace">
        <defs>
          {([["b", C.accent], ["p", C.purple], ["c", C.cyan], ["g", C.green]] as [string,string][]).map(([id, col]) => (
            <marker key={id} id={`arr-${id}`} markerWidth={8} markerHeight={6} refX={8} refY={3} orient="auto">
              <polygon points="0 0,8 3,0 6" fill={col} opacity={0.7} />
            </marker>
          ))}
        </defs>

        {/* ── Title ── */}
        <text x={480} y={20} textAnchor="middle" fill={C.text}  fontSize={12} fontWeight={700} letterSpacing={3}>AGENT BRAIN — SYSTEM ARCHITECTURE</text>
        <text x={480} y={34} textAnchor="middle" fill={C.dim}   fontSize={8}  letterSpacing={1}>BrainCore (engine) · McpServerCore (MCP adapter) · 47 tools · 15 skills · Neo4j · 4 LLM providers</text>
        <line x1={30} y1={41} x2={930} y2={41} stroke={C.border} strokeWidth={1} />

        {/* ── CLIENT LAYER ── */}
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
          <text x={582} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>💬 HBI Frontend</text>
          <text x={582} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>SSE streaming browser</text>
        </Box>
        <Box {...boxProps("c-scripts", "#101820","#182030", C.border, C.accent)} x={699} y={61} w={231} h={44}>
          <text x={814} y={79} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={600}>📜 Self-Learn Scripts</text>
          <text x={814} y={95} textAnchor="middle" fill={C.dim}    fontSize={8}>self_learn.py · self_reflect.py</text>
        </Box>
        {[136, 359, 582, 814].map(cx => <Conn key={cx} x1={cx} y1={105} x2={cx} y2={122} col={C.accent} marker="b" />)}

        {/* ── TRANSPORT ── */}
        <text x={30} y={122} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>TRANSPORT</text>
        <Box {...boxProps("t-stdio", "#0c1318","#121c24", C.border, C.cyan)} x={30}  y={126} w={200} h={48}>
          <text x={130} y={145} textAnchor="middle" fill={C.cyan}  fontSize={10} fontWeight={600}>stdio</text>
          <text x={130} y={159} textAnchor="middle" fill={C.dim}   fontSize={8}>McpServer wrapper</text>
          <text x={130} y={170} textAnchor="middle" fill={C.muted} fontSize={7}>line-delimited JSON-RPC 2.0</text>
        </Box>
        <Box {...boxProps("t-http", "#0c1318","#121c24", C.border, C.cyan)} x={240} y={126} w={690} h={48}>
          <text x={585} y={145} textAnchor="middle" fill={C.cyan}  fontSize={10} fontWeight={600}>HTTP / SSE Transport (Axum)</text>
          <text x={585} y={159} textAnchor="middle" fill={C.dim}   fontSize={8}>POST /mcp · GET /mcp (SSE) · POST /chat · GET /health · REST /api/* · Bearer auth</text>
          <text x={585} y={170} textAnchor="middle" fill={C.muted} fontSize={7}>initialize → notifications/initialized → tools/call</text>
        </Box>
        <Conn x1={480} y1={174} x2={480} y2={192} col={C.cyan} marker="c" />

        {/* ── MCP ADAPTER ── */}
        <text x={30} y={192} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>MCP ADAPTER</text>
        <Box {...boxProps("core", "#0c1020","#111828","#2a4a6a", C.cyan, 8)} x={30} y={196} w={900} h={62}>
          <text x={480} y={212} textAnchor="middle" fill={C.cyan} fontSize={11} fontWeight={700}>McpServerCore</text>
          <text x={480} y={223} textAnchor="middle" fill={C.muted} fontSize={7}>protocol adapter only — all engine logic is in BrainCore below</text>
          {([
            { x: 46,  w: 194, label: "ServerState",     sub: "JSON-RPC session machine" },
            { x: 250, w: 194, label: "SessionManager",  sub: "HTTP per-client state" },
            { x: 454, w: 194, label: "ChatService",     sub: "tool-use loop · SSE stream" },
            { x: 658, w: 212, label: "chat_llm_config", sub: "optional /chat LLM override" },
          ] as { x: number; w: number; label: string; sub: string }[]).map(({ x, w, label, sub }) => (
            <g key={label}>
              <rect x={x} y={229} width={w} height={24} rx={3} fill={C.card} stroke={C.border} strokeWidth={1} />
              <text x={x + w/2} y={239} textAnchor="middle" fill={C.text} fontSize={8.5} fontWeight={600}>{label}</text>
              <text x={x + w/2} y={249} textAnchor="middle" fill={C.dim}  fontSize={7}>{sub}</text>
            </g>
          ))}
        </Box>
        <Conn x1={480} y1={258} x2={480} y2={274} col={C.accent} marker="b" />

        {/* ── BRAIN CORE ── */}
        <text x={30} y={274} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>BRAIN CORE</text>
        <Box {...boxProps("brain", "#0d1420","#141d30","#3a5aaa", C.accent, 8)} x={30} y={278} w={900} h={80}>
          <text x={480} y={294} textAnchor="middle" fill={C.accent} fontSize={11} fontWeight={700}>BrainCore</text>
          <text x={480} y={305} textAnchor="middle" fill={C.muted} fontSize={7}>owns all state — storage · LLM · skills · queue · scheduler · event bus</text>
          {([
            { x: 50,  label: "ToolRegistry",   sub: "47 tools · tools/list" },
            { x: 195, label: "ToolHandler",    sub: "routes tool/call" },
            { x: 340, label: "LlmConfig Arc",  sub: "live-swap · 4 providers" },
            { x: 485, label: "JobServices",    sub: "queue + scheduler" },
            { x: 630, label: "ContextBuilder", sub: "7 profiles · 2 protocols" },
            { x: 775, label: "EventBus",       sub: "scheduler events" },
          ] as { x: number; label: string; sub: string }[]).map(({ x, label, sub }) => (
            <g key={label}>
              <rect x={x} y={312} width={130} height={38} rx={4} fill={C.card} stroke={`${C.accent}44`} strokeWidth={1} />
              <text x={x + 65} y={327} textAnchor="middle" fill={C.accent} fontSize={9} fontWeight={600}>{label}</text>
              <text x={x + 65} y={340} textAnchor="middle" fill={C.dim}    fontSize={7.5}>{sub}</text>
            </g>
          ))}
        </Box>
        <Conn x1={480} y1={358} x2={480} y2={374} col={C.purple} marker="p" />

        {/* ── SKILLS ── */}
        <text x={30} y={374} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>SKILLS — 15 SKILLS · 47 TOOLS</text>
        <Box {...boxProps("sk-memory", "#0c1218","#111c28","#2a3550", C.accent)} x={30}  y={378} w={222} h={150}>
          <text x={141} y={394} textAnchor="middle" fill={C.accent} fontSize={10} fontWeight={700}>MEMORY</text>
          <Chip x={42}  y={401} label="KnowledgeSkill"     sub="6 tools · RAG · entities"       col={C.green} />
          <Chip x={42}  y={433} label="WorkingMemorySkill" sub="2 tools · session scratchpad"    col={C.green} />
          <Chip x={42}  y={465} label="ResourceSkill"      sub="1 tool · connection registry"    col={C.green} />
          <text x={141} y={510} textAnchor="middle" fill={C.accent} fontSize={8}>9 tools total</text>
          <text x={141} y={521} textAnchor="middle" fill={C.muted}  fontSize={7}>BM25 · vector · RRF</text>
        </Box>
        <Box {...boxProps("sk-auto", "#0c0c18","#121228","#352a50", C.purple)} x={262} y={378} w={222} h={150}>
          <text x={373} y={394} textAnchor="middle" fill={C.purple} fontSize={10} fontWeight={700}>AUTOMATION</text>
          <Chip x={274} y={401} label="AgentSkill"     sub="5 tools · job queue · chaining"  col={C.purple} />
          <Chip x={274} y={433} label="SchedulerSkill" sub="4 tools · autonomous tick loop"  col={C.purple} />
          <Chip x={274} y={465} label="DynamicSkill"   sub="3 tools · runtime definition"    col={C.purple} />
          <text x={373} y={510} textAnchor="middle" fill={C.purple} fontSize={8}>12 tools total</text>
          <text x={373} y={521} textAnchor="middle" fill={C.muted}  fontSize={7}>Tokio · BinaryHeap</text>
        </Box>
        <Box {...boxProps("sk-data", "#0c1410","#121e14","#2a4030", C.green)} x={494} y={378} w={222} h={150}>
          <text x={605} y={394} textAnchor="middle" fill={C.green} fontSize={10} fontWeight={700}>DATA</text>
          <Chip x={506} y={401} label="TaskSkill"    sub="5 tools · goals · decompose"    col={C.cyan} />
          <Chip x={506} y={433} label="QuerySkill"   sub="2 tools · neo4j + duckdb"       col={C.cyan} />
          <Chip x={506} y={465} label="ContextSkill" sub="1 tool · profile management"    col={C.cyan} />
          <text x={605} y={510} textAnchor="middle" fill={C.green} fontSize={8}>8 tools total</text>
          <text x={605} y={521} textAnchor="middle" fill={C.muted} fontSize={7}>Neo4j · DuckDB</text>
        </Box>
        <Box {...boxProps("sk-ext", "#140e08","#1e1610","#3a2e10", C.yellow)} x={726} y={378} w={204} h={150}>
          <text x={828} y={394} textAnchor="middle" fill={C.yellow} fontSize={10} fontWeight={700}>EXT & UTILS</text>
          <Chip x={738} y={401} w={180} label="HttpSkill"        sub="2 tools · generic HTTP"        col={C.orange} />
          <Chip x={738} y={433} w={180} label="CodebaseSkill"    sub="7 tools · self-analysis · git" col={C.orange} />
          <Chip x={738} y={465} w={180} label="WsSkill + Others" sub="9 tools · ws · search · sleep" col={C.orange} />
          <text x={828} y={510} textAnchor="middle" fill={C.yellow} fontSize={8}>18 tools total</text>
          <text x={828} y={521} textAnchor="middle" fill={C.muted}  fontSize={7}>Axum · git · search</text>
        </Box>
        <Conn x1={480} y1={528} x2={480} y2={546} col={C.cyan} marker="c" />

        {/* ── SERVICES ── */}
        <text x={30} y={546} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>SERVICES</text>
        <Box {...boxProps("svc", "#0c1018","#111520", C.border, C.cyan)} x={30} y={550} w={900} h={46}>
          {([
            { x: 46,  w: 152, name: "KnowledgeService", sub: "RAG · BM25 · snapshots" },
            { x: 208, w: 152, name: "QueueService",     sub: "heap · semaphores · coord" },
            { x: 370, w: 152, name: "SchedulerService", sub: "Tokio task · perception" },
            { x: 532, w: 152, name: "SnapshotService",  sub: "gzip backup · flate2" },
            { x: 694, w: 226, name: "ContextBuilder + ResourceRegistry", sub: "profiles · connection pool" },
          ] as { x: number; w: number; name: string; sub: string }[]).map(({ x, w, name, sub }) => (
            <g key={name}>
              <rect x={x} y={556} width={w} height={34} rx={4} fill={C.card} stroke={C.border} strokeWidth={1} />
              <text x={x + w/2} y={570} textAnchor="middle" fill={C.text} fontSize={9}  fontWeight={600}>{name}</text>
              <text x={x + w/2} y={583} textAnchor="middle" fill={C.dim}  fontSize={7.5}>{sub}</text>
            </g>
          ))}
        </Box>
        <Conn x1={480} y1={596} x2={480} y2={614} col={C.green} marker="g" />

        {/* ── INFRASTRUCTURE ── */}
        <text x={30} y={614} fill={C.muted} fontSize={8} letterSpacing={2} fontWeight={600}>INFRASTRUCTURE</text>
        <Box {...boxProps("i-neo4j",  "#0a1410","#111e16","#2a4030", C.green)}  x={30}  y={618} w={218} h={98}>
          <text x={139} y={636} textAnchor="middle" fill={C.green}  fontSize={10} fontWeight={700}>Neo4j Graph DB</text>
          <text x={139} y={650} textAnchor="middle" fill={C.dim}    fontSize={8}>neo4rs · bolt://localhost:7687</text>
          <text x={139} y={664} textAnchor="middle" fill={C.dim}    fontSize={7.5}>Note · Entity · Task · AgentJob</text>
          <text x={139} y={676} textAnchor="middle" fill={C.dim}    fontSize={7.5}>Procedure · DynamicTool · etc.</text>
          <text x={139} y={688} textAnchor="middle" fill={C.green}  fontSize={7.5}>BM25 + 1024-dim bge-m3 vectors</text>
          <text x={139} y={700} textAnchor="middle" fill={C.muted}  fontSize={7}>MERGE-safe · 11 edge types · RRF</text>
        </Box>
        <Box {...boxProps("i-llm",    "#0e0b14","#14102a","#352a40", C.purple)} x={258} y={618} w={285} h={98}>
          <text x={400} y={636} textAnchor="middle" fill={C.purple} fontSize={10} fontWeight={700}>LLM Providers</text>
          {([
            [270, 652, "🦙 Ollama",       "qwen3.5:4b · local · embeddings"],
            [270, 667, "☁️ OllamaCloud",  "openai-compat · cloud · embed→local"],
            [270, 682, "🔮 Anthropic",    "claude-* · Messages API · tool_use"],
            [270, 697, "✨ Gemini",       "generativeLanguage API"],
          ] as [number,number,string,string][]).map(([x, y, name, info]) => (
            <g key={name}>
              <text x={x}      y={y} fill={C.orange} fontSize={9}>{name}</text>
              <text x={x + 96} y={y} fill={C.dim}    fontSize={7.5}>{info}</text>
            </g>
          ))}
          <text x={400} y={710} textAnchor="middle" fill={C.purple} fontSize={7.5}>runtime switch via use_model tool</text>
        </Box>
        <Box {...boxProps("i-secrets", "#12100a","#1c180e","#3a3010", C.yellow)} x={553} y={618} w={190} h={98}>
          <text x={648} y={636} textAnchor="middle" fill={C.yellow} fontSize={10} fontWeight={700}>Secret Store</text>
          <text x={648} y={650} textAnchor="middle" fill={C.dim}    fontSize={8}>automatic key injection</text>
          <text x={563} y={666} fill={C.dim} fontSize={7.5}>Local  — AES-256-GCM</text>
          <text x={563} y={680} fill={C.dim} fontSize={7.5}>Vault  — HashiCorp KV v2</text>
          <text x={563} y={694} fill={C.dim} fontSize={7.5}>AWS    — Secrets Manager</text>
          <text x={648} y={708} textAnchor="middle" fill={C.yellow} fontSize={7.5}>resource tool registration</text>
        </Box>
        <Box {...boxProps("i-persist", "#140c0c","#1e1010","#3a1818", C.red)} x={753} y={618} w={177} h={98}>
          <text x={841} y={636} textAnchor="middle" fill={C.red}   fontSize={10} fontWeight={700}>Telemetry + Snapshots</text>
          <text x={841} y={650} textAnchor="middle" fill={C.dim}   fontSize={7.5}>DuckDB brain_logs.db</text>
          <text x={841} y={664} textAnchor="middle" fill={C.dim}   fontSize={7.5}>digest_experiences → JSONL</text>
          <text x={841} y={678} textAnchor="middle" fill={C.dim}   fontSize={7.5}>analyze_gaps → knowledge</text>
          <text x={841} y={692} textAnchor="middle" fill={C.dim}   fontSize={7.5}>SnapshotService (flate2)</text>
          <text x={841} y={706} textAnchor="middle" fill={C.red}   fontSize={7.5}>SleepSkill · QuerySkill</text>
        </Box>

        {/* Autonomous self-improvement loop arc */}
        <path d="M 930 440 C 950 440 950 570 930 570" fill="none" stroke={C.purple} strokeWidth={1.5} strokeDasharray="5,3" opacity={0.6} markerStart="url(#arr-p)" />
        <text x={954} y={508} textAnchor="middle" fill={C.purple} fontSize={7.5} fontWeight={600} transform="rotate(90 954 508)">AUTONOMOUS SELF-IMPROVEMENT</text>

        {/* Footer */}
        <line x1={30} y1={722} x2={930} y2={722} stroke={C.border} strokeWidth={1} />
        <text x={36} y={735} fill={C.muted} fontSize={7.5}>FLOW:</text>
        {([
          [68,  C.accent, "MCP call"],
          [148, C.purple, "skill dispatch"],
          [255, C.cyan,   "service call"],
          [348, C.green,  "DB query"],
        ] as [number, string, string][]).map(([x, col, label]) => (
          <g key={label}>
            <line x1={x} y1={732} x2={x+28} y2={732} stroke={col} strokeWidth={1.5} strokeDasharray="4,2" opacity={0.7} />
            <text x={x+32} y={735} fill={C.dim} fontSize={7.5}>{label}</text>
          </g>
        ))}
        <text x={460} y={735} fill={C.muted} fontSize={7.5}>
          ↻ SchedulerService ticks every 5 min · perceives failures · enqueues LLM chains via BrainCore
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
            BrainCore · 47 tools · 15 skills
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
