/**
 * System → Architecture: what the brain is made of, drawn from live data
 * (`GET /api/architecture`, HBI_REDESIGN_PLAN.md §4). Nothing here is
 * hand-written: skills, routes, agents, schedules, providers and services come
 * from the running brain; `architecture/layout.yaml` only says where each
 * group sits and how the groups connect. A live item the layout doesn't place
 * is drawn under "Unplaced", so a new skill shows up before anyone edits YAML.
 *
 * "What changed" compares the live inventory with the newest snapshot
 * (`snapshot_architecture`, weekly on brain-api-sync).
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  ARCH_CATEGORY,
  diffCounts,
  useArchitecture,
  useSystemHealth,
  type ArchDiff,
  type ArchGroup,
  type ArchNode,
  type Architecture,
  type ProbedComponent,
} from "../../api/system";
import { formatLocal, relTime } from "../../lib/time";
import { hrefTo } from "../../nav";
import { Card, EmptyState, Pill, Stat, Tabs, type Tone } from "../ui";

type Tab = "diagram" | "topology" | "history";

const STATUS_TONE: Record<string, Tone> = { up: "green", degraded: "yellow", down: "red", not_configured: "muted" };
const KIND_LABEL: Record<ArchNode["kind"], string> = {
  skill: "skill", routes: "routes", component: "service", provider: "provider",
  mcp: "MCP server", collection: "collection", static: "client",
};

export default function ArchitectureView() {
  const a = useArchitecture();
  const health = useSystemHealth();
  const [tab, setTab] = useState<Tab>("diagram");
  const [sel, setSel] = useState<ArchNode | null>(null);
  const status = useMemo(() => {
    const m = new Map<string, ProbedComponent>();
    for (const c of health.data?.components ?? []) m.set(c.name, c);
    return m;
  }, [health.data]);

  if (!a.data) {
    return (
      <div className="sys scroll">
        {a.error
          ? <EmptyState tone="red" title="Couldn't load the architecture" checked={`GET /api/architecture failed: ${a.error.message}`}
              action={<button type="button" className="btn" onClick={() => a.refetch()}>Retry</button>} />
          : <div className="loading">Reading what the brain is made of…</div>}
      </div>
    );
  }
  const d = a.data;
  const c = d.counts;
  return (
    <div className="sys scroll">
      <div className="ui-stats ar-counts">
        <Stat label="Tools" value={c.tools} title={`${c.skills} skills`} href={hrefTo("system", { view: "registry" })} />
        <Stat label="Skills" value={c.skills} href={hrefTo("system", { view: "registry" })} />
        <Stat label="Routes" value={c.routes} title="HTTP routes, read from the router source"
          onClick={() => { setTab("diagram"); setSel(allRoutes(d)); }} />
        <Stat label="Agents" value={c.agents} href={hrefTo("system", { view: "registry" })} />
        <Stat label="Workflows" value={c.workflows} href={hrefTo("work", { view: "scheduled" })} />
        <Stat label="Providers" value={c.providers} />
        <Stat label="Services" value={c.components} href={hrefTo("system", { view: "health" })} />
      </div>

      <ChangeBanner d={d} onHistory={() => setTab("history")} />
      {!d.layout_loaded && (
        <EmptyState tone="yellow" title="No layout"
          checked="plugins/core/architecture/layout.yaml was not found, so every item is unplaced." />
      )}
      {(d.unplaced.length > 0 || d.stale.length > 0) && <LayoutGaps d={d} />}

      <Tabs
        tabs={[
          { id: "diagram", label: "Diagram" },
          { id: "topology", label: "Containers" },
          { id: "history", label: `Snapshots (${d.history.length})` },
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "diagram" && (
        <div className="ar-main">
          <Diagram d={d} status={status} sel={sel} onSelect={setSel} />
          <NodeDetail d={d} node={sel} status={status} onClose={() => setSel(null)} />
        </div>
      )}
      {tab === "topology" && <Topology d={d} status={status} />}
      {tab === "history" && <History d={d} />}
    </div>
  );
}

/** Every route in one node, for the Routes count. */
function allRoutes(d: Architecture): ArchNode {
  const items = [...d.groups.flatMap((g) => g.nodes), ...d.unplaced]
    .filter((n) => n.kind === "routes")
    .flatMap((n) => n.items)
    .sort((x, y) => x.split(" ")[1].localeCompare(y.split(" ")[1]) || x.localeCompare(y));
  return { id: "routes:*", kind: "routes", name: "*", label: "All routes", count: items.length, items, detail: "", meta: {} };
}

// ── What changed ─────────────────────────────────────────────────────────────

function ChangeBanner({ d, onHistory }: { d: Architecture; onHistory: () => void }) {
  const s = d.since_snapshot;
  if (!s) {
    return (
      <div className="sys-banner">
        <Pill tone="muted">no snapshot</Pill>
        <span>Nothing to compare with yet. The brain takes a snapshot at its first start and weekly on <code>brain-api-sync</code>.</span>
      </div>
    );
  }
  const changed = Object.keys(s.diff).length > 0;
  const prev = d.history[0];
  return (
    <div className={`sys-banner ${changed ? "degraded" : "ok"}`}>
      <Pill tone={changed ? "yellow" : "green"}>{changed ? diffCounts(s.diff) : "unchanged"}</Pill>
      <span title={formatLocal(s.taken_at)}>
        {changed ? <>Since the snapshot {relTime(s.taken_at)}: {s.summary}</> : <>Nothing has changed since the snapshot {relTime(s.taken_at)}.</>}
        {prev && prev.previous_id && Object.keys(prev.diff).length > 0 && (
          <span className="wk-dim"> That snapshot itself recorded: {diffCounts(prev.diff)}.</span>
        )}
      </span>
      <button type="button" className="btn-ghost sys-checked" onClick={onHistory}>snapshots →</button>
    </div>
  );
}

function LayoutGaps({ d }: { d: Architecture }) {
  return (
    <div className="sys-issues">
      {d.unplaced.length > 0 && (
        <div className="sys-issue">
          <Pill tone="yellow">unplaced</Pill>
          <span>
            {d.unplaced.length} live {d.unplaced.length === 1 ? "item has" : "items have"} no group in <code>architecture/layout.yaml</code>:{" "}
            {d.unplaced.map((n) => n.id).join(", ")}. They are drawn under “Unplaced”.
          </span>
        </div>
      )}
      {d.stale.length > 0 && (
        <div className="sys-issue">
          <Pill tone="muted">stale</Pill>
          <span>Layout entries that match nothing running: {d.stale.join(", ")}.</span>
        </div>
      )}
    </div>
  );
}

// ── Diagram ──────────────────────────────────────────────────────────────────

/** Boxes a group shows before "+N more". */
const GROUP_SHOWN = 10;

interface EdgePath { from: string; to: string; label: string; d: string }

function nodeTone(n: ArchNode, status: Map<string, ProbedComponent>): Tone | null {
  if (n.kind === "component") return STATUS_TONE[status.get(n.name)?.status ?? ""] ?? "muted";
  if (n.kind === "provider") return !n.meta.enabled ? "muted" : n.meta.key_configured === false ? "yellow" : "green";
  return null;
}

function Diagram({ d, status, sel, onSelect }: {
  d: Architecture; status: Map<string, ProbedComponent>; sel: ArchNode | null; onSelect: (n: ArchNode | null) => void;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const cards = useRef(new Map<string, HTMLElement>());
  const [paths, setPaths] = useState<EdgePath[]>([]);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const toggle = (id: string) => setOpen((o) => {
    const n = new Set(o);
    if (n.has(id)) n.delete(id); else n.add(id);
    return n;
  });

  const columns = useMemo(() => {
    const groups: ArchGroup[] = [...d.groups];
    if (d.unplaced.length) {
      groups.push({ id: "unplaced", label: "Unplaced", column: 2, blurb: "Live, but not in layout.yaml yet.", nodes: d.unplaced });
    }
    const n = Math.max(0, ...groups.map((g) => g.column)) + 1;
    const cols: ArchGroup[][] = Array.from({ length: n }, () => []);
    for (const g of groups) cols[g.column].push(g);
    return cols.filter((c) => c.length > 0);
  }, [d]);

  // Edges are measured from the rendered cards, so they follow any layout.
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const measure = () => {
      const base = el.getBoundingClientRect();
      const out: EdgePath[] = [];
      for (const e of d.edges) {
        const f = cards.current.get(e.from)?.getBoundingClientRect();
        const t = cards.current.get(e.to)?.getBoundingClientRect();
        if (!f || !t) continue;
        let path: string;
        if (t.left >= f.right - 1 || t.right <= f.left + 1) {
          const forward = t.left >= f.right - 1;
          const x1 = (forward ? f.right : f.left) - base.left;
          const x2 = (forward ? t.left : t.right) - base.left;
          const y1 = f.top + f.height / 2 - base.top;
          const y2 = t.top + t.height / 2 - base.top;
          const dx = (x2 - x1) / 2;
          path = `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
        } else {
          const down = t.top >= f.bottom;
          const x = f.left + f.width / 2 - base.left;
          const y1 = (down ? f.bottom : f.top) - base.top;
          const y2 = (down ? t.top : t.bottom) - base.top;
          path = `M${x},${y1} L${x},${y2}`;
        }
        out.push({ ...e, d: path });
      }
      setPaths(out);
    };
    // Fires once on observe, then on every resize.
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    for (const c of cards.current.values()) ro.observe(c);
    return () => ro.disconnect();
  }, [d, columns, open]);

  const selGroup = sel ? columns.flat().find((g) => g.nodes.some((n) => n.id === sel.id))?.id : undefined;

  return (
    <div className="ar-scroll">
      <div className="ar-diagram" ref={wrap} style={{ gridTemplateColumns: `repeat(${columns.length}, minmax(180px, 1fr))` }}>
        <svg className="ar-edges" aria-hidden="true">
          <defs>
            <marker id="ar-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
              <path d="M0,0 L8,4 L0,8 z" className="ar-arrow" />
            </marker>
          </defs>
          {paths.map((p) => {
            const on = selGroup !== undefined && (p.from === selGroup || p.to === selGroup);
            return (
              <g key={`${p.from}-${p.to}`} className={on ? "ar-edge on" : "ar-edge"}>
                <path d={p.d} markerEnd="url(#ar-arrow)"><title>{p.label}</title></path>
              </g>
            );
          })}
        </svg>
        {columns.map((col, i) => (
          <div key={i} className="ar-col">
            {col.map((g) => (
              <section key={g.id} ref={(el) => { if (el) cards.current.set(g.id, el); else cards.current.delete(g.id); }}
                className={`ar-group${g.id === "unplaced" ? " unplaced" : ""}${g.id === selGroup ? " on" : ""}`}>
                <header>
                  <strong>{g.label}</strong>
                  {g.blurb && <span className="wk-dim">{g.blurb}</span>}
                </header>
                <div className="ar-nodes">
                  {g.nodes.length === 0 && <span className="wk-dim">nothing live in this group</span>}
                  {(open.has(g.id) || g.nodes.length <= GROUP_SHOWN + 1 ? g.nodes
                    : g.nodes.slice(0, GROUP_SHOWN).concat(sel && g.nodes.slice(GROUP_SHOWN).some((n) => n.id === sel.id) ? [sel] : [])
                  ).map((n) => {
                    const tone = nodeTone(n, status);
                    return (
                      <button key={n.id} type="button" title={n.detail || n.id}
                        className={`ar-node kind-${n.kind}${sel?.id === n.id ? " on" : ""}`}
                        onClick={() => onSelect(sel?.id === n.id ? null : n)}>
                        {tone && <span className={`ar-dot tone-${tone}`} />}
                        <span className="ar-node-label">{n.label}</span>
                        {n.count > 0 && <span className="ar-count">{n.count}</span>}
                      </button>
                    );
                  })}
                  {g.nodes.length > GROUP_SHOWN + 1 && (
                    <button type="button" className="ar-more" onClick={() => toggle(g.id)}>
                      {open.has(g.id) ? "show fewer" : `+${g.nodes.length - GROUP_SHOWN} more`}
                    </button>
                  )}
                </div>
              </section>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

function NodeDetail({ d, node, status, onClose }: {
  d: Architecture; node: ArchNode | null; status: Map<string, ProbedComponent>; onClose: () => void;
}) {
  const [filter, setFilter] = useState("");
  if (!node) {
    return (
      <Card title="Flows">
        <div className="wk-dim ar-hint">Click any box to see what is behind it: a skill's tools, an area's routes, a service's live status.</div>
        <ul className="ar-flows">
          {d.edges.map((e) => {
            const label = (id: string) => d.groups.find((g) => g.id === id)?.label ?? id;
            return <li key={`${e.from}-${e.to}`}><strong>{label(e.from)}</strong> → <strong>{label(e.to)}</strong> <span className="wk-dim">{e.label}</span></li>;
          })}
        </ul>
      </Card>
    );
  }
  const live = node.kind === "component" ? status.get(node.name) : undefined;
  const q = filter.trim().toLowerCase();
  const items = q ? node.items.filter((i) => i.toLowerCase().includes(q)) : node.items;
  const link =
    node.kind === "skill" ? { href: hrefTo("system", { view: "registry" }), text: "tools in Registry →" }
    : node.kind === "component" ? { href: hrefTo("system", { view: "health" }), text: "Health →" }
    : node.id === "collection:workflows" ? { href: hrefTo("work", { view: "scheduled" }), text: "Workflows →" }
    : node.kind === "collection" || node.kind === "mcp" || node.kind === "provider" ? { href: hrefTo("system", { view: "registry" }), text: "Registry →" }
    : null;
  return (
    <Card title={<>{node.label} <Pill tone="muted">{KIND_LABEL[node.kind]}</Pill></>}
      actions={<button type="button" className="btn-ghost" onClick={onClose} title="Close">✕</button>}>
      <div className="ar-detail">
        {node.detail && <div className="wk-dim">{node.kind === "routes" ? <>handled in <code>{node.detail}</code></> : node.detail}</div>}
        {live && (
          <div>
            <Pill tone={STATUS_TONE[live.status]}>{live.status.replace("_", " ")}</Pill>
            {live.latency_ms !== null && <span className="wk-dim"> {live.latency_ms} ms</span>}
            {live.detail && <span className="wk-dim"> · {live.detail}</span>}
          </div>
        )}
        {node.kind === "component" && node.meta.required && <div className="wk-dim">Required: the brain is broken without it.</div>}
        {node.kind === "provider" && (
          <div className="wk-dim">{node.meta.kind}{node.meta.enabled ? "" : " · disabled"}{node.meta.key_configured === false ? " · key missing" : ""}</div>
        )}
        {node.items.length > 0 && (
          <>
            {node.items.length > 12 && (
              <input className="ui-input" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder={`filter ${node.items.length}…`} />
            )}
            <ul className="ar-items">
              {items.map((i) => <li key={i}><code>{i}</code></li>)}
              {items.length === 0 && <li className="wk-dim">none match “{filter}”</li>}
            </ul>
          </>
        )}
        {link && <a className="wk-dim" href={link.href}>{link.text}</a>}
      </div>
    </Card>
  );
}

// ── Containers ───────────────────────────────────────────────────────────────

function Topology({ d, status }: { d: Architecture; status: Map<string, ProbedComponent> }) {
  const t = d.topology;
  if (t.containers.length === 0) {
    return <EmptyState tone="yellow" title="No topology" checked="architecture/layout.yaml has no topology section." />;
  }
  const comp = (name: string | null) => {
    const c = name ? status.get(name) : undefined;
    return c ? <Pill tone={STATUS_TONE[c.status]}>{c.status.replace("_", " ")}</Pill> : <span className="wk-dim">—</span>;
  };
  return (
    <div className="ar-topo">
      <table className="reg-table">
        <thead>
          <tr>
            <th>Container</th>
            {t.networks.map((n) => (
              <th key={n.name} title={n.note}>{n.name}{n.internal ? " · internal" : ""}{n.external ? " · external" : ""}</th>
            ))}
            <th>Published on the host</th>
            <th>Status</th>
          </tr>
        </thead>
        <tbody>
          {t.containers.map((c) => (
            <tr key={c.name}>
              <td><strong>{c.name}</strong>{c.note && <div className="wk-dim">{c.note}</div>}</td>
              {t.networks.map((n) => <td key={n.name} className="ar-cell">{c.networks.includes(n.name) ? "●" : ""}</td>)}
              <td>{c.ports.length ? c.ports.map((p) => <code key={p}>{p} </code>) : <span className="wk-dim">not published</span>}</td>
              <td>{c.name === "agent-brain" ? <Pill tone="green">serving this page</Pill> : comp(c.component)}</td>
            </tr>
          ))}
          {t.host.map((h) => (
            <tr key={h.name}>
              <td><strong>{h.name}</strong> <Pill tone="muted">host</Pill>{h.note && <div className="wk-dim">{h.note}</div>}</td>
              {t.networks.map((n) => <td key={n.name} className="ar-cell wk-dim">·</td>)}
              <td className="wk-dim">on the host</td>
              <td>{comp(h.component)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <ul className="ar-flows">
        {t.networks.map((n) => <li key={n.name}><strong>{n.name}</strong> <span className="wk-dim">{n.note}</span></li>)}
      </ul>
      <div className="wk-dim reg-note">Checked against <code>docker-compose.yml</code> by a unit test, so this table can't drift from it.</div>
    </div>
  );
}

// ── Snapshots ────────────────────────────────────────────────────────────────

function DiffList({ diff }: { diff: ArchDiff }) {
  const cats = Object.keys(ARCH_CATEGORY).filter((c) => diff[c]);
  if (cats.length === 0) return <div className="wk-dim">No changes.</div>;
  return (
    <ul className="ar-diff">
      {cats.map((c) => (
        <li key={c}>
          <strong>{ARCH_CATEGORY[c]}</strong>
          {diff[c].added.map((n) => <span key={`+${n}`} className="ar-add">+ {n}</span>)}
          {diff[c].removed.map((n) => <span key={`-${n}`} className="ar-rm">− {n}</span>)}
        </li>
      ))}
    </ul>
  );
}

function History({ d }: { d: Architecture }) {
  if (d.history.length === 0) {
    return <EmptyState title="No snapshots yet" checked="No (:ArchSnapshot) nodes. One is taken at the first start and weekly by brain-api-sync." />;
  }
  return (
    <div className="ar-history">
      {d.since_snapshot && Object.keys(d.since_snapshot.diff).length > 0 && (
        <Card title="Since the newest snapshot (live)">
          <DiffList diff={d.since_snapshot.diff} />
        </Card>
      )}
      {d.history.map((s) => (
        <details key={s.id} className="ar-snap" open={s === d.history[0]}>
          <summary>
            <span title={formatLocal(s.taken_at)}>{formatLocal(s.taken_at)}</span>
            <Pill tone="muted">{s.trigger}</Pill>
            {s.version && <code>{s.version.slice(0, 10)}</code>}
            <span className="wk-dim">{s.previous_id ? (diffCounts(s.diff) || "no changes") : "baseline"}</span>
          </summary>
          {s.previous_id ? <DiffList diff={s.diff} /> : <div className="wk-dim">{s.summary}</div>}
        </details>
      ))}
    </div>
  );
}
