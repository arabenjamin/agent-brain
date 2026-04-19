import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import ForceGraph2D from "react-force-graph-2d";
import type { ForceGraphMethods } from "react-force-graph-2d";
import { getBrainUrl, getApiKey } from "../../api/config";

// ── Types ─────────────────────────────────────────────────────────────────────

interface RawGraphNode {
  id: string;
  label: string;
  type: "note" | "entity" | "task";
  note_type?: string;
  entity_type?: string;
  status?: string;
}

interface RawGraphEdge {
  source: string;
  target: string;
  type: string;
  weight?: number;
}

interface GraphNode {
  id: string;
  label: string;
  nodeType: "note" | "entity" | "task";
  noteType?: string;
  entityType?: string;
  taskStatus?: string;
  color: string;
  val: number;
  x?: number;
  y?: number;
  __bckgDimensions?: [number, number];
}

interface GraphLink {
  source: string;
  target: string;
  edgeType: string;
  weight?: number;
}

interface GraphData {
  nodes: GraphNode[];
  links: GraphLink[];
}

interface SelectedNodeInfo {
  id: string;
  type: "note" | "entity" | "task";
  label: string;
  content?: string;
  note_type?: string;
  entity_type?: string;
  status?: string;
  access_count?: number;
}

// ── Colour maps ───────────────────────────────────────────────────────────────

const NOTE_TYPE_COLORS: Record<string, string> = {
  semantic:     "#4f8ef7",
  episodic:     "#22d3ee",
  reflection:   "#a78bfa",
  consolidated: "#4ade80",
  outcome:      "#fbbf24",
  inference:    "#f87171",
};

const NODE_TYPE_COLORS: Record<string, string> = {
  entity: "#fb923c",
  task:   "#facc15",
};

const EDGE_COLORS: Record<string, string> = {
  relates_to:    "rgba(79,142,247,0.35)",
  mentions:      "rgba(251,146,60,0.45)",
  part_of:       "rgba(100,100,200,0.3)",
  summarized_by: "rgba(74,222,128,0.4)",
  reflects_on:   "rgba(167,139,250,0.45)",
  subtask_of:    "rgba(250,204,21,0.45)",
  derived_from:  "rgba(248,113,113,0.4)",
  depends_on:    "rgba(251,146,60,0.35)",
};

const ALL_NOTE_TYPES = Object.keys(NOTE_TYPE_COLORS);
const ALL_EDGE_TYPES = Object.keys(EDGE_COLORS);

function nodeColorFor(raw: RawGraphNode): string {
  if (raw.type === "note") return NOTE_TYPE_COLORS[raw.note_type ?? "semantic"] ?? "#7a8099";
  return NODE_TYPE_COLORS[raw.type] ?? "#7a8099";
}

function nodeValFor(raw: RawGraphNode): number {
  if (raw.type === "task")   return 5;
  if (raw.type === "entity") return 2;
  return 3;
}

function toGraphNode(raw: RawGraphNode): GraphNode {
  return {
    id:         raw.id,
    label:      raw.label,
    nodeType:   raw.type,
    noteType:   raw.note_type,
    entityType: raw.entity_type,
    taskStatus: raw.status,
    color:      nodeColorFor(raw),
    val:        nodeValFor(raw),
  };
}

function toGraphLink(raw: RawGraphEdge): GraphLink {
  return { source: raw.source, target: raw.target, edgeType: raw.type, weight: raw.weight };
}

// ── N-hop neighbourhood ───────────────────────────────────────────────────────

function computeNHop(edges: RawGraphEdge[], startId: string, depth: number): Set<string> {
  // Build adjacency list once — O(edges), then BFS is O(reachable) per hop.
  const adj = new Map<string, string[]>();
  for (const e of edges) {
    if (!adj.has(e.source)) adj.set(e.source, []);
    if (!adj.has(e.target)) adj.set(e.target, []);
    adj.get(e.source)!.push(e.target);
    adj.get(e.target)!.push(e.source);
  }
  const reachable = new Set([startId]);
  let frontier = [startId];
  for (let i = 0; i < depth && frontier.length > 0; i++) {
    const next: string[] = [];
    for (const id of frontier) {
      for (const neighbor of adj.get(id) ?? []) {
        if (!reachable.has(neighbor)) {
          reachable.add(neighbor);
          next.push(neighbor);
        }
      }
    }
    frontier = next;
  }
  return reachable;
}

// ── Main component ────────────────────────────────────────────────────────────

export default function GraphPanel() {
  // Raw data from API
  const [allNodes, setAllNodes] = useState<RawGraphNode[]>([]);
  const [allEdges, setAllEdges] = useState<RawGraphEdge[]>([]);
  const [loading, setLoading]   = useState(false);
  const [error, setError]       = useState<string | null>(null);

  // Filter state
  const [searchQ, setSearchQ]           = useState("");
  const [visNoteTypes, setVisNoteTypes] = useState<Set<string>>(() => new Set(ALL_NOTE_TYPES));
  const [visEntities, setVisEntities]   = useState(true);
  const [visTasks, setVisTasks]         = useState(true);
  const [visEdgeTypes, setVisEdgeTypes] = useState<Set<string>>(() => new Set(ALL_EDGE_TYPES));

  // Focus / neighbourhood mode
  const [focusNodeId, setFocusNodeId] = useState<string | null>(null);
  const [focusDepth, setFocusDepth]   = useState(2);

  // UI state
  const [selectedNode, setSelectedNode]   = useState<SelectedNodeInfo | null>(null);
  const [frozen, setFrozen]               = useState(false);
  const [controlsOpen, setControlsOpen]  = useState(false);

  const fgRef        = useRef<ForceGraphMethods<GraphNode, GraphLink>>(undefined);
  const containerRef = useRef<HTMLDivElement>(null);
  const [dims, setDims] = useState({ w: 800, h: 600 });

  useLayoutEffect(() => {
    if (!containerRef.current) return;
    const ro = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      setDims({ w: Math.floor(width), h: Math.floor(height) });
    });
    ro.observe(containerRef.current);
    return () => ro.disconnect();
  }, []);

  // ── Computed view ─────────────────────────────────────────────────────────────
  // Reactively recomputes whenever raw data or any filter state changes.

  const graphData: GraphData = useMemo(() => {
    let nodes = allNodes;
    let edges = allEdges;

    // 1. Edge type visibility
    edges = edges.filter((e) => visEdgeTypes.has(e.type));

    // 2. Focus / neighbourhood
    if (focusNodeId) {
      const reachable = computeNHop(edges, focusNodeId, focusDepth);
      nodes = nodes.filter((n) => reachable.has(n.id));
      edges = edges.filter((e) => reachable.has(e.source) && reachable.has(e.target));
    }

    // 3. Node type visibility
    nodes = nodes.filter((n) => {
      if (n.type === "entity") return visEntities;
      if (n.type === "task")   return visTasks;
      return visNoteTypes.has(n.note_type ?? "semantic");
    });
    const visIds = new Set(nodes.map((n) => n.id));
    edges = edges.filter((e) => visIds.has(e.source) && visIds.has(e.target));

    // 4. Text search
    if (searchQ.trim()) {
      const q = searchQ.toLowerCase();
      const matchIds = new Set(nodes.filter((n) => n.label.toLowerCase().includes(q)).map((n) => n.id));
      nodes = nodes.filter((n) => matchIds.has(n.id));
      edges = edges.filter((e) => matchIds.has(e.source) && matchIds.has(e.target));
    }

    return { nodes: nodes.map(toGraphNode), links: edges.map(toGraphLink) };
  }, [allNodes, allEdges, searchQ, visNoteTypes, visEntities, visTasks, visEdgeTypes, focusNodeId, focusDepth]);

  // ── Load ─────────────────────────────────────────────────────────────────────

  const loadGraph = useCallback(async () => {
    setLoading(true);
    setError(null);
    setFocusNodeId(null);
    setSelectedNode(null);
    try {
      const res = await fetch(`${getBrainUrl()}/api/graph?max_nodes=200`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      setAllNodes(data.nodes ?? []);
      setAllEdges(data.edges ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { loadGraph(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Layout controls ───────────────────────────────────────────────────────────

  const toggleFreeze = () => {
    if (frozen) {
      fgRef.current?.resumeAnimation();
    } else {
      fgRef.current?.pauseAnimation();
    }
    setFrozen((v) => !v);
  };

  const fitView = () => fgRef.current?.zoomToFit(400, 40);

  // ── Node click ────────────────────────────────────────────────────────────────

  const handleNodeClick = useCallback(async (node: GraphNode) => {
    if (node.nodeType === "entity") {
      setSelectedNode({ id: node.id, type: "entity", label: node.label, entity_type: node.entityType });
      return;
    }
    if (node.nodeType === "task") {
      setSelectedNode({ id: node.id, type: "task", label: node.label, status: node.taskStatus });
      return;
    }
    try {
      const res = await fetch(`${getBrainUrl()}/api/notes/${node.id}`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      if (res.ok) {
        const data = await res.json();
        setSelectedNode({
          id:           data.id ?? node.id,
          type:         "note",
          label:        node.label,
          content:      data.content,
          note_type:    data.note_type,
          access_count: data.access_count,
        });
      }
    } catch (e) {
      console.error("Failed to fetch note:", e);
    }
  }, []);

  // ── Node painter ──────────────────────────────────────────────────────────────

  const paintNode = useCallback(
    (node: GraphNode, ctx: CanvasRenderingContext2D, globalScale: number) => {
      const fontSize = Math.max(8, 12 / globalScale);
      const r = Math.sqrt(node.val ?? 3) * 4;
      const isFocal = node.id === focusNodeId;

      ctx.beginPath();
      if (node.nodeType === "entity") {
        ctx.moveTo(node.x ?? 0, (node.y ?? 0) - r);
        ctx.lineTo((node.x ?? 0) + r, node.y ?? 0);
        ctx.lineTo(node.x ?? 0, (node.y ?? 0) + r);
        ctx.lineTo((node.x ?? 0) - r, node.y ?? 0);
        ctx.closePath();
      } else if (node.nodeType === "task") {
        ctx.rect((node.x ?? 0) - r, (node.y ?? 0) - r, r * 2, r * 2);
      } else {
        ctx.arc(node.x ?? 0, node.y ?? 0, r, 0, 2 * Math.PI, false);
      }
      ctx.fillStyle = node.color;
      ctx.fill();
      if (isFocal) {
        ctx.strokeStyle = "rgba(255,255,255,0.9)";
        ctx.lineWidth = 2 / globalScale;
      } else {
        ctx.strokeStyle = "rgba(255,255,255,0.15)";
        ctx.lineWidth = 0.5;
      }
      ctx.stroke();

      if (globalScale >= 1.5) {
        ctx.font = `${fontSize}px "JetBrains Mono", monospace`;
        ctx.fillStyle = "rgba(212,216,232,0.8)";
        ctx.textAlign = "center";
        ctx.fillText(node.label.slice(0, 30), node.x ?? 0, (node.y ?? 0) + r + fontSize + 1);
      }
    },
    [focusNodeId]
  );

  // ── Toggle helpers ────────────────────────────────────────────────────────────

  const toggleNoteType = (t: string) =>
    setVisNoteTypes((prev) => {
      const next = new Set(prev);
      if (next.has(t)) next.delete(t); else next.add(t);
      return next;
    });

  const toggleEdgeType = (t: string) =>
    setVisEdgeTypes((prev) => {
      const next = new Set(prev);
      if (next.has(t)) next.delete(t); else next.add(t);
      return next;
    });

  const allVisible = ALL_NOTE_TYPES.every((t) => visNoteTypes.has(t)) && visEntities && visTasks;
  const allEdgesVisible = ALL_EDGE_TYPES.every((t) => visEdgeTypes.has(t));

  const focusedLabel = focusNodeId
    ? (allNodes.find((n) => n.id === focusNodeId)?.label ?? focusNodeId.slice(0, 8))
    : null;

  // ── Render ────────────────────────────────────────────────────────────────────

  return (
    <div className="panel">
      <div className="panel-header">
        🕸 Knowledge Graph
        {graphData.nodes.length > 0 && (
          <span className="badge">
            {graphData.nodes.length} nodes · {graphData.links.length} edges
          </span>
        )}
        {loading && <span style={{ color: "var(--text-muted)", fontSize: 11 }}>loading…</span>}
        <div style={{ marginLeft: "auto", display: "flex", gap: 4, alignItems: "center" }}>
          <button
            className="btn"
            style={{ padding: "3px 9px", fontSize: 14 }}
            title={frozen ? "Resume simulation" : "Freeze layout"}
            onClick={toggleFreeze}
          >
            {frozen ? "▶" : "⏸"}
          </button>
          <button
            className="btn"
            style={{ padding: "3px 9px", fontSize: 14 }}
            title="Fit all nodes in view"
            onClick={fitView}
          >
            ⊡
          </button>
          <button className="refresh-btn" style={{ marginLeft: 0 }} onClick={loadGraph} title="Reload graph">↻</button>
        </div>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div ref={containerRef} className="graph-container">

        {/* ── Left panel: search + controls ── */}
        <div className="graph-left-panel">
          <div className="graph-search-bar" style={{ position: "static" }}>
            <input
              placeholder="Search nodes…"
              value={searchQ}
              onChange={(e) => setSearchQ(e.target.value)}
              onKeyDown={(e) => e.key === "Escape" && setSearchQ("")}
            />
            {focusedLabel && (
              <div className="focus-badge">
                <span>Focus: <strong>{focusedLabel.slice(0, 18)}</strong></span>
                <span className="focus-depth-label">±{focusDepth}</span>
                <button className="focus-clear-btn" onClick={() => setFocusNodeId(null)} title="Clear focus">×</button>
              </div>
            )}
          </div>

          <button
            className="graph-controls-toggle"
            onClick={() => setControlsOpen((v) => !v)}
          >
            {controlsOpen ? "◀ Controls" : "▶ Controls"}
          </button>

          {controlsOpen && (
            <div className="graph-controls-panel">

              {/* Node types */}
              <div className="gc-section">
                <div className="gc-title">
                  Nodes
                  <button
                    className="gc-toggle-all"
                    onClick={() => {
                      if (allVisible) {
                        setVisNoteTypes(new Set());
                        setVisEntities(false);
                        setVisTasks(false);
                      } else {
                        setVisNoteTypes(new Set(ALL_NOTE_TYPES));
                        setVisEntities(true);
                        setVisTasks(true);
                      }
                    }}
                  >
                    {allVisible ? "hide all" : "show all"}
                  </button>
                </div>
                {ALL_NOTE_TYPES.map((t) => (
                  <label key={t} className="gc-row">
                    <input type="checkbox" checked={visNoteTypes.has(t)} onChange={() => toggleNoteType(t)} />
                    <span className="legend-dot" style={{ background: NOTE_TYPE_COLORS[t] }} />
                    {t}
                  </label>
                ))}
                <label className="gc-row">
                  <input type="checkbox" checked={visEntities} onChange={() => setVisEntities((v) => !v)} />
                  <span className="legend-dot" style={{ background: NODE_TYPE_COLORS.entity }} />
                  entities
                </label>
                <label className="gc-row">
                  <input type="checkbox" checked={visTasks} onChange={() => setVisTasks((v) => !v)} />
                  <span className="legend-dot" style={{ background: NODE_TYPE_COLORS.task }} />
                  tasks
                </label>
              </div>

              {/* Edge types */}
              <div className="gc-section">
                <div className="gc-title">
                  Edges
                  <button
                    className="gc-toggle-all"
                    onClick={() => setVisEdgeTypes(allEdgesVisible ? new Set() : new Set(ALL_EDGE_TYPES))}
                  >
                    {allEdgesVisible ? "hide all" : "show all"}
                  </button>
                </div>
                {ALL_EDGE_TYPES.map((t) => (
                  <label key={t} className="gc-row">
                    <input type="checkbox" checked={visEdgeTypes.has(t)} onChange={() => toggleEdgeType(t)} />
                    <span
                      className="legend-dot"
                      style={{ background: EDGE_COLORS[t]?.replace(/[\d.]+\)$/, "0.85)") }}
                    />
                    {t.replace(/_/g, " ")}
                  </label>
                ))}
              </div>

              {/* Focus depth */}
              <div className="gc-section">
                <div className="gc-title">Focus depth</div>
                <div className="gc-depth-row">
                  {[1, 2, 3, 4].map((d) => (
                    <button
                      key={d}
                      className={`gc-depth-btn${focusDepth === d ? " active" : ""}`}
                      onClick={() => setFocusDepth(d)}
                    >
                      {d}
                    </button>
                  ))}
                </div>
                {!focusNodeId && (
                  <div className="gc-hint">Click a node then "Focus" to show its neighbourhood</div>
                )}
              </div>

            </div>
          )}
        </div>

        {/* ── Right: legend (hidden behind detail overlay when open) ── */}
        {!selectedNode && (
          <div className="graph-overlay">
            <div className="graph-legend">
              <div className="graph-legend-title">Node type</div>
              {ALL_NOTE_TYPES.map((type) => (
                <div key={type} className="legend-row">
                  <span className="legend-dot" style={{ background: NOTE_TYPE_COLORS[type] }} />
                  note: {type}
                </div>
              ))}
              <div className="legend-row">
                <span className="legend-dot" style={{ background: NODE_TYPE_COLORS.entity }} />
                entity ◇
              </div>
              <div className="legend-row">
                <span className="legend-dot" style={{ background: NODE_TYPE_COLORS.task }} />
                task ▪
              </div>
            </div>
          </div>
        )}

        {graphData.nodes.length === 0 && !loading && (
          <div className="empty-state" style={{ position: "absolute", inset: 0 }}>
            <span className="icon">🕸</span>
            <span>No nodes — try reloading once the brain has notes stored</span>
          </div>
        )}

        <ForceGraph2D
          ref={fgRef}
          graphData={graphData}
          onNodeClick={handleNodeClick}
          nodeColor={(n) => (n as GraphNode).color}
          nodeVal={(n) => (n as GraphNode).val}
          nodeLabel={(n) => (n as GraphNode).label}
          linkColor={(l) => EDGE_COLORS[(l as GraphLink).edgeType] ?? "rgba(79,142,247,0.25)"}
          linkWidth={(l) => {
            const ll = l as GraphLink;
            return ll.weight ? ll.weight * 2 : 0.5;
          }}
          backgroundColor="transparent"
          nodeCanvasObject={paintNode as Parameters<typeof ForceGraph2D>[0]["nodeCanvasObject"]}
          nodeCanvasObjectMode={() => "after"}
          width={dims.w}
          height={dims.h}
        />

        {/* ── Node detail overlay ── */}
        {selectedNode && (
          <div className="graph-detail-overlay">
            <div className="graph-detail-header">
              <span className={`note-type-badge ${selectedNode.note_type ?? selectedNode.type}`}>
                {selectedNode.type === "note" ? (selectedNode.note_type ?? "note") : selectedNode.type}
              </span>
              <div style={{ display: "flex", gap: 4, marginLeft: "auto", alignItems: "center" }}>
                {focusNodeId !== selectedNode.id ? (
                  <button
                    className="btn"
                    style={{ padding: "2px 8px", fontSize: 11 }}
                    title={`Show ${focusDepth}-hop neighbourhood`}
                    onClick={() => setFocusNodeId(selectedNode.id)}
                  >
                    Focus ±{focusDepth}
                  </button>
                ) : (
                  <button
                    className="btn"
                    style={{ padding: "2px 8px", fontSize: 11 }}
                    onClick={() => setFocusNodeId(null)}
                  >
                    Unfocus
                  </button>
                )}
                <button className="close-btn" onClick={() => setSelectedNode(null)}>×</button>
              </div>
            </div>
            <div className="graph-detail-content scroll">
              {selectedNode.type === "note" && selectedNode.content}
              {selectedNode.type === "entity" && (
                <div>
                  <strong>{selectedNode.label}</strong>
                  {selectedNode.entity_type && (
                    <div style={{ color: "var(--text-muted)", marginTop: 6 }}>{selectedNode.entity_type}</div>
                  )}
                </div>
              )}
              {selectedNode.type === "task" && (
                <div>
                  <strong>{selectedNode.label}</strong>
                  {selectedNode.status && (
                    <div className={`task-status-badge ${selectedNode.status}`}
                         style={{ marginTop: 8, display: "inline-block" }}>
                      {selectedNode.status}
                    </div>
                  )}
                </div>
              )}
            </div>
            <div className="graph-detail-footer">
              {selectedNode.access_count !== undefined && (
                <span>Accessed {selectedNode.access_count}× · </span>
              )}
              ID: {selectedNode.id.slice(0, 8)}…
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
