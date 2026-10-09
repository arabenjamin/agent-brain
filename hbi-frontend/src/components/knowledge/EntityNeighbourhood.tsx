/**
 * An entity's neighbourhood on the map (HBI plan Phase 16,
 * `#/knowledge?view=map&focus=<entity id>&scope=<topic>`): the entity, the
 * entities it shares the most notes with by strength, and the pairs among
 * them. Click a node to expand its own neighbours (and open it); click a line
 * for the notes behind the pair. With a scope, only that topic's notes count
 * and entities kept out of it are left out.
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import ForceGraph2D, { type ForceGraphMethods } from "react-force-graph-2d";
import { fetchNeighbourhood } from "../../api/related";
import { useTopicMap, useTopics } from "../../api/topics";
import { mergeNeighbourhood, nodeVal, type NbGraph, type NbLink, type NbNode } from "../../lib/neighbourhood";
import { HUES } from "../../lib/topicMap";
import { goTo, hrefTo } from "../../nav";
import { openEntity } from "../drawer/entities";
import { PairNotes } from "../drawer/RelatedEntities";
import { EmptyState } from "../ui";

const endId = (v: unknown): string => (typeof v === "string" ? v : (v as { id: string }).id);

export default function EntityNeighbourhood({ id, scope }: { id: string; scope?: string }) {
  const [graph, setGraph] = useState<NbGraph>({ nodes: [], links: [] });
  const [total, setTotal] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pair, setPair] = useState<{ a: string; b: string } | null>(null);
  const [hover, setHover] = useState<NbNode | null>(null);
  const topics = useTopics();
  const map = useTopicMap(25);
  const hue = new Map((map.data?.topics ?? []).map((t, i) => [t.name, HUES[i % HUES.length]]));

  const expand = useCallback(async (nodeId: string, fresh = false) => {
    setBusy(true);
    setError(null);
    try {
      const r = await fetchNeighbourhood(nodeId, scope);
      if (nodeId === id) setTotal(r.total_related);
      setGraph((g) => mergeNeighbourhood(fresh ? { nodes: [], links: [] } : g, r, id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [id, scope]);

  useEffect(() => {
    setPair(null);
    expand(id, true);
  }, [id, scope, expand]);

  const box = useRef<HTMLDivElement>(null);
  const [dims, setDims] = useState({ w: 800, h: 600 });
  useLayoutEffect(() => {
    if (!box.current) return;
    const ro = new ResizeObserver(([e]) => setDims({ w: Math.floor(e.contentRect.width), h: Math.floor(e.contentRect.height) }));
    ro.observe(box.current);
    return () => ro.disconnect();
  }, []);
  const fg = useRef<ForceGraphMethods<NbNode, NbLink>>(undefined);
  useEffect(() => {
    const link = fg.current?.d3Force("link") as unknown as { distance?: (d: (l: NbLink) => number) => void } | undefined;
    link?.distance?.((l: NbLink) => (l.kind === "typed" ? 90 : 40 + 120 * (1 - Math.min(1, l.strength * 2))));
    fg.current?.d3ReheatSimulation();
  }, [graph]);

  const center = graph.nodes.find((n) => n.center);
  const names = new Map(graph.nodes.map((n) => [n.id, n.label]));

  return (
    <div className="tm">
      <div className="tm-bar">
        <a className="btn-ghost" href={hrefTo("knowledge", { view: "map" })}>← Topic map</a>
        <span className="wk-row-title">{center?.label ?? "…"}</span>
        <select className="ui-input auto" value={scope ?? ""} title="Count only one topic's notes"
          onChange={(e) => goTo("knowledge", { view: "map", focus: id, scope: e.target.value || undefined })}>
          <option value="">in all notes</option>
          {(topics.data?.topics ?? []).map((t) => <option key={t.name} value={t.name}>in {t.label}</option>)}
        </select>
        <button type="button" className="btn-ghost" disabled={busy} onClick={() => expand(id, true)}>Reset</button>
        <span className="wk-dim">
          {graph.nodes.length} entities · {graph.links.filter((l) => l.kind === "co").length} pairs · {graph.links.filter((l) => l.kind === "typed").length} relationships
          {total !== null && ` · ${total} share a note with ${center?.label ?? "it"}`}
          {busy && " · loading…"}
        </span>
      </div>
      <div className="tm-canvas" ref={box}>
        {error && <EmptyState tone="red" title="Couldn't load the neighbourhood" checked={error} />}
        {!error && !busy && graph.nodes.length <= 1 && (
          <EmptyState title="No neighbours" checked={scope ? `No ${scope} note that mentions it names another entity.` : "No note that mentions it names another entity (URLs and dates are not counted)."} />
        )}
        {graph.nodes.length > 0 && (
          <ForceGraph2D
            ref={fg}
            graphData={graph}
            width={dims.w}
            height={dims.h}
            backgroundColor="transparent"
            nodeVal={(n) => nodeVal(n as NbNode)}
            nodeLabel={() => ""}
            linkCurvature={(l) => ((l as NbLink).kind === "typed" ? 0.25 : 0)}
            linkDirectionalArrowLength={(l) => ((l as NbLink).kind === "typed" ? 5 : 0)}
            linkDirectionalArrowRelPos={0.9}
            linkLineDash={(l) => ((l as NbLink).kind === "typed" && (l as NbLink).status === "proposed" ? [3, 2] : null)}
            linkCanvasObjectMode={(l) => ((l as NbLink).kind === "typed" ? "after" : undefined)}
            linkCanvasObject={(l, ctx, scale) => {
              type End = string | { x: number; y: number };
              const k = l as unknown as { kind: string; label?: string; source: End; target: End };
              if (k.kind !== "typed" || scale < 1.2 || typeof k.source !== "object" || typeof k.target !== "object") return;
              const x = (k.source.x + k.target.x) / 2;
              const y = (k.source.y + k.target.y) / 2;
              ctx.font = `${9 / scale}px system-ui, sans-serif`;
              ctx.textAlign = "center";
              ctx.fillStyle = "rgba(120,200,255,0.95)";
              ctx.fillText(k.label ?? "", x, y);
            }}
            linkColor={(l) => {
              const k = l as NbLink;
              if (k.kind === "typed") return k.status === "proposed" ? "rgba(120,200,255,0.5)" : "rgba(120,200,255,0.9)";
              const sel = pair && ((endId(k.source) === pair.a && endId(k.target) === pair.b) || (endId(k.source) === pair.b && endId(k.target) === pair.a));
              return sel ? "rgba(255,210,90,0.9)" : `rgba(140,150,170,${0.15 + Math.min(0.6, k.strength * 1.5)})`;
            }}
            linkWidth={(l) => ((l as NbLink).kind === "typed" ? 1.6 : 0.6 + Math.min(6, (l as NbLink).strength * 14))}
            linkHoverPrecision={6}
            cooldownTicks={150}
            onEngineStop={() => fg.current?.zoomToFit(400, 60)}
            onNodeHover={(n) => setHover((n as NbNode) ?? null)}
            onNodeDragEnd={(n) => {
              const m = n as NbNode;
              m.fx = m.x;
              m.fy = m.y;
            }}
            onNodeClick={(n) => {
              const m = n as NbNode;
              if (!m.expanded) expand(m.id);
              openEntity("entity", m.id);
            }}
            onLinkClick={(l) => {
              const k = l as NbLink;
              setPair({ a: endId(k.source), b: endId(k.target) });
            }}
            nodeCanvasObject={(n, ctx, scale) => {
              const m = n as NbNode & { x: number; y: number };
              const r = Math.sqrt(nodeVal(m)) * 2;
              const h = m.topic ? hue.get(m.topic) : undefined;
              ctx.beginPath();
              ctx.arc(m.x, m.y, r, 0, 2 * Math.PI);
              ctx.fillStyle = h !== undefined ? `hsla(${h},60%,60%,0.85)` : "rgba(150,155,170,0.8)";
              ctx.fill();
              if (m.center || !m.expanded) {
                ctx.lineWidth = (m.center ? 2.5 : 1) / scale;
                ctx.setLineDash(m.center ? [] : [2 / scale, 2 / scale]);
                ctx.strokeStyle = "rgba(255,255,255,0.85)";
                ctx.stroke();
                ctx.setLineDash([]);
              }
              const size = (m.center ? 13 : 10) / Math.max(scale, 0.6);
              ctx.font = `${m.center ? "600 " : ""}${size}px system-ui, sans-serif`;
              ctx.textAlign = "center";
              ctx.textBaseline = "top";
              ctx.fillStyle = "rgba(215,220,230,0.92)";
              ctx.fillText(m.label, m.x, m.y + r + 2 / scale);
            }}
            nodePointerAreaPaint={(n, color, ctx) => {
              const m = n as NbNode & { x: number; y: number };
              ctx.beginPath();
              ctx.arc(m.x, m.y, Math.sqrt(nodeVal(m)) * 2 + 2, 0, 2 * Math.PI);
              ctx.fillStyle = color;
              ctx.fill();
            }}
          />
        )}
        {hover && (
          <div className="tm-hover">
            <strong>{hover.label}</strong>
            <span>{hover.entityType ?? "entity"} · {hover.mentions} notes{hover.topic ? ` · mostly ${hover.topic}` : ""}</span>
            <span>{hover.expanded ? "neighbours shown" : "click to show its neighbours"}</span>
          </div>
        )}
        {pair && (
          <div className="nb-pair">
            <div className="nb-pair-head">
              <strong>{names.get(pair.a)} + {names.get(pair.b)}</strong>
              <button type="button" className="btn-ghost" onClick={() => setPair(null)}>×</button>
            </div>
            <PairNotes a={pair.a} b={pair.b} topic={scope} />
          </div>
        )}
        <div className="tm-legend">
          <div><strong>Grey lines:</strong> named in the same notes; thicker = stronger (shared notes over how often each is mentioned)</div>
          <div><strong>Blue arrows:</strong> a typed relationship (zoom in for its name); dashed: proposed, waiting for your review</div>
          <div><strong>Colour:</strong> the topic most of its notes are in · solid ring: where you started · dashed: click to expand</div>
          <div className="wk-dim">Click a line for the notes behind it · drag to pin</div>
        </div>
      </div>
    </div>
  );
}
