/**
 * The domain map: the force graph organised under topics (plan §3).
 *
 * Each topic is a hub; around it, the entities its notes mention most. An
 * entity several topics share sits between them and is drawn with a ring —
 * those bridges are the point of the map. Drag anything (a dragged node stays
 * where you drop it); click a topic or entity to open it; double-click an
 * entity for its neighbourhood of notes.
 *
 * Size means one thing at a time, chosen and stated in the legend:
 * connections (notes mentioning it), certainty (the claims that mention it,
 * refuted → primary sources), or activity (notes in the last 7 days).
 */
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import ForceGraph2D, { type ForceGraphMethods } from "react-force-graph-2d";
import { assessedShare, useTopicMap, useTopics } from "../../api/topics";
import { buildMap, LABEL_BUDGET, type MapLink, type MapNode, type SizeBy } from "../../lib/topicMap";
import { goTo } from "../../nav";
import { openEntity } from "../drawer/entities";
import { EmptyState } from "../ui";

const SIZE_BY: { id: SizeBy; label: string; legend: string }[] = [
  { id: "connections", label: "Connections", legend: "notes that mention it" },
  { id: "certainty", label: "Certainty", legend: "how well-corroborated the claims mentioning it are (small: none, unverified or refuted; large: primary sources)" },
  { id: "activity", label: "Activity", legend: "notes in the last 7 days" },
];

const PER_TOPIC = [15, 25, 40];

export default function TopicMap() {
  const [by, setBy] = useState<SizeBy>("connections");
  const [perTopic, setPerTopic] = useState(25);
  const map = useTopicMap(perTopic);
  const topics = useTopics(60_000);
  const graph = useMemo(() => (map.data ? buildMap(map.data, by) : null), [map.data, by]);

  const box = useRef<HTMLDivElement>(null);
  const [dims, setDims] = useState({ w: 800, h: 600 });
  useLayoutEffect(() => {
    if (!box.current) return;
    const ro = new ResizeObserver(([e]) => setDims({ w: Math.floor(e.contentRect.width), h: Math.floor(e.contentRect.height) }));
    ro.observe(box.current);
    return () => ro.disconnect();
  }, []);
  const lastClick = useRef<{ id: string; at: number } | null>(null);
  const fg = useRef<ForceGraphMethods<MapNode, MapLink>>(undefined);
  // Fit the whole map once it first settles; after that the view is the user's.
  const fitted = useRef(false);
  // Hubs push each other apart hard, so small topics don't sit on one another;
  // entity links stay short so each cluster holds together.
  useEffect(() => {
    const f = fg.current;
    if (!f || !graph) return;
    const charge = f.d3Force("charge") as unknown as { strength?: (s: (n: MapNode) => number) => void } | undefined;
    charge?.strength?.((n: MapNode) => (n.kind === "topic" ? -600 : -40));
    const link = f.d3Force("link") as unknown as { distance?: (d: (l: MapLink) => number) => void } | undefined;
    link?.distance?.((l: MapLink) => 30 + 60 / Math.sqrt(1 + l.notes));
    fitted.current = false;
    f.d3ReheatSimulation();
  }, [graph]);
  const [hover, setHover] = useState<MapNode | null>(null);

  const cov = topics.data?.coverage;
  const bridges = graph?.nodes.filter((n) => n.kind === "entity" && n.topics > 1).length ?? 0;
  const legend = SIZE_BY.find((s) => s.id === by)!;

  return (
    <div className="tm">
      <div className="tm-bar">
        <span className="wk-dim">Size by</span>
        {SIZE_BY.map((s) => (
          <button key={s.id} type="button" className={by === s.id ? "btn" : "btn-ghost"} onClick={() => setBy(s.id)}>{s.label}</button>
        ))}
        <span className="wk-dim">entities per topic</span>
        <select className="ui-input auto" value={perTopic} onChange={(e) => setPerTopic(Number(e.target.value))}>
          {PER_TOPIC.map((n) => <option key={n} value={n}>{n}</option>)}
        </select>
        {cov && (
          <span className="tm-cov" title="Every note is in a topic, classified as fitting none, waiting, or failed">
            {cov.with_topic.toLocaleString()} notes in a topic · {cov.no_topic.toLocaleString()} fit none
            {cov.waiting > 0 && <> · <span className="tone-text-yellow">{cov.waiting.toLocaleString()} waiting ({Math.round(assessedShare(cov) * 100)}% classified; it runs while the brain is idle)</span></>}
            {cov.failed > 0 && <> · <span className="tone-text-red">{cov.failed} failed</span></>}
            {cov.skipped > 0 && <span title="Activity records topics/_scope.yaml leaves out (e.g. one note per dispatched task)"> · {cov.skipped.toLocaleString()} activity records skipped</span>}
          </span>
        )}
      </div>
      <div className="tm-canvas" ref={box}>
        {map.error && <EmptyState tone="red" title="Couldn't load the map" checked={map.error.message} />}
        {!map.data && !map.error && <div className="loading">Loading the map…</div>}
        {graph && graph.nodes.length === 0 && (
          <EmptyState title="Nothing to map yet" checked="No topic has notes yet. Topics come from topics/*.yaml, and notes are classified while the brain is idle." />
        )}
        {graph && graph.nodes.length > 0 && (
          <ForceGraph2D
            ref={fg}
            graphData={graph}
            width={dims.w}
            height={dims.h}
            backgroundColor="transparent"
            nodeVal={(n) => (n as MapNode).val}
            nodeLabel={() => ""}
            linkColor={() => "rgba(140,150,170,0.18)"}
            linkWidth={(l) => Math.min(4, 0.4 + Math.sqrt((l as MapLink).notes) / 3)}
            d3VelocityDecay={0.35}
            cooldownTicks={200}
            onEngineStop={() => {
              if (fitted.current) return;
              fitted.current = true;
              fg.current?.zoomToFit(400, 60);
            }}
            onNodeHover={(n) => setHover((n as MapNode) ?? null)}
            onNodeDragEnd={(n) => {
              const m = n as MapNode;
              m.fx = m.x;
              m.fy = m.y;
            }}
            onNodeClick={(n) => {
              const m = n as MapNode;
              const now = Date.now();
              const dbl = lastClick.current?.id === m.id && now - lastClick.current.at < 350;
              lastClick.current = { id: m.id, at: now };
              if (m.kind === "topic") openEntity("topic", m.id.slice("topic:".length));
              else if (dbl) goTo("knowledge", { view: "graph", q: `"${m.label}"`, mode: "keyword" });
              else openEntity("entity", m.id);
            }}
            nodeCanvasObject={(n, ctx, scale) => {
              const m = n as MapNode & { x: number; y: number };
              const r = Math.sqrt(m.val) * 1.6;
              ctx.beginPath();
              ctx.arc(m.x, m.y, r, 0, 2 * Math.PI);
              ctx.fillStyle = m.kind === "topic" ? `hsla(${m.hue},70%,55%,0.9)` : `hsla(${m.hue},55%,62%,0.75)`;
              ctx.fill();
              if (m.kind === "entity" && m.topics > 1) {
                ctx.lineWidth = 2 / scale;
                ctx.strokeStyle = "rgba(255,255,255,0.85)";
                ctx.stroke();
              }
              const showLabel = m.labelled || scale > 2.5;
              if (showLabel) {
                const size = (m.kind === "topic" ? 14 : 10) / Math.max(scale, 0.6);
                ctx.font = `${m.kind === "topic" ? "600 " : ""}${size}px system-ui, sans-serif`;
                ctx.textAlign = "center";
                ctx.textBaseline = "top";
                ctx.fillStyle = m.kind === "topic" ? "rgba(235,238,245,0.95)" : "rgba(200,205,215,0.85)";
                ctx.fillText(m.label, m.x, m.y + r + 2 / scale);
              }
            }}
            nodePointerAreaPaint={(n, color, ctx) => {
              const m = n as MapNode & { x: number; y: number };
              ctx.beginPath();
              ctx.arc(m.x, m.y, Math.sqrt(m.val) * 1.6 + 2, 0, 2 * Math.PI);
              ctx.fillStyle = color;
              ctx.fill();
            }}
          />
        )}
        {hover && (
          <div className="tm-hover">
            <strong>{hover.label}</strong>
            <span>{hover.kind === "topic" ? "topic" : hover.detail}</span>
            {hover.kind === "topic" && <span>{hover.detail}</span>}
          </div>
        )}
        {graph && (
          <div className="tm-legend">
            <div><strong>Size:</strong> {by === "certainty" ? <>entities by {legend.legend}; topics fixed</> : legend.legend}</div>
            <div className="wk-dim">Names: the {LABEL_BUDGET} biggest entities; zoom in for the rest.</div>
            <div><strong>Colour:</strong> the topic whose notes mention it most · <span className="tm-ring" /> ring: shared by 2+ topics ({bridges} bridges)</div>
            <div className="wk-dim">
              {graph.nodes.filter((n) => n.kind === "topic").length} topics · {graph.nodes.length - graph.nodes.filter((n) => n.kind === "topic").length} entities (the top {map.data?.per_topic} per topic; urls and dates hidden) · drag to pin · double-click an entity for its notes
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
