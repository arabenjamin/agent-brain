import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import ForceGraph2D from "react-force-graph-2d";
import type { ForceGraphMethods } from "react-force-graph-2d";
import { callTool } from "../../api/mcp";

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
  relates_to:    "rgba(79,142,247,0.3)",
  mentions:      "rgba(251,146,60,0.4)",
  part_of:       "rgba(100,100,200,0.25)",
  summarized_by: "rgba(74,222,128,0.35)",
  reflects_on:   "rgba(167,139,250,0.4)",
  subtask_of:    "rgba(250,204,21,0.4)",
  derived_from:  "rgba(248,113,113,0.35)",
  depends_on:    "rgba(251,146,60,0.3)",
};

function nodeColorFor(raw: RawGraphNode): string {
  if (raw.type === "note") {
    return NOTE_TYPE_COLORS[raw.note_type ?? "semantic"] ?? "#7a8099";
  }
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

// ── Main component ────────────────────────────────────────────────────────────

export default function GraphPanel() {
  const [allNodes, setAllNodes]   = useState<RawGraphNode[]>([]);
  const [allEdges, setAllEdges]   = useState<RawGraphEdge[]>([]);
  const [graphData, setGraphData] = useState<GraphData>({ nodes: [], links: [] });
  const [loading, setLoading]     = useState(false);
  const [error, setError]         = useState<string | null>(null);
  const [searchQ, setSearchQ]     = useState("");
  const [selectedNode, setSelectedNode] = useState<SelectedNodeInfo | null>(null);

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

  // Client-side filter — no extra API calls when user types
  const applyFilter = useCallback((nodes: RawGraphNode[], edges: RawGraphEdge[], query: string) => {
    if (!query.trim()) {
      setGraphData({ nodes: nodes.map(toGraphNode), links: edges.map(toGraphLink) });
      return;
    }
    const q = query.toLowerCase();
    const matchIds = new Set(nodes.filter(n => n.label.toLowerCase().includes(q)).map(n => n.id));
    const filteredNodes = nodes.filter(n => matchIds.has(n.id));
    const filteredEdges = edges.filter(e => matchIds.has(e.source) && matchIds.has(e.target));
    setGraphData({ nodes: filteredNodes.map(toGraphNode), links: filteredEdges.map(toGraphLink) });
  }, []);

  // Single API call — brain returns Note + Entity + Task nodes and all edges
  const loadGraph = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const json = await callTool("export_graph_visualization", { max_nodes: 200 });
      const data = JSON.parse(json);
      const nodes: RawGraphNode[] = data.nodes ?? [];
      const edges: RawGraphEdge[] = data.edges ?? [];
      setAllNodes(nodes);
      setAllEdges(edges);
      applyFilter(nodes, edges, searchQ);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [searchQ, applyFilter]);

  useEffect(() => { loadGraph(); }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const handleSearch = () => applyFilter(allNodes, allEdges, searchQ);
  const handleKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") handleSearch();
  };

  const handleNodeClick = useCallback(async (node: GraphNode) => {
    if (node.nodeType === "entity") {
      setSelectedNode({ id: node.id, type: "entity", label: node.label, entity_type: node.entityType });
      return;
    }
    if (node.nodeType === "task") {
      setSelectedNode({ id: node.id, type: "task", label: node.label, status: node.taskStatus });
      return;
    }
    // Note node: fetch full content via get_note
    try {
      const json = await callTool("get_note", { id: node.id });
      const data = JSON.parse(json);
      if (data) {
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

  // Circle for notes, diamond for entities, square for tasks
  const paintNode = useCallback(
    (node: GraphNode, ctx: CanvasRenderingContext2D, globalScale: number) => {
      const fontSize = Math.max(8, 12 / globalScale);
      const r = Math.sqrt(node.val ?? 3) * 4;

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
      ctx.strokeStyle = "rgba(255,255,255,0.15)";
      ctx.lineWidth = 0.5;
      ctx.stroke();

      if (globalScale >= 1.5) {
        ctx.font = `${fontSize}px "JetBrains Mono", monospace`;
        ctx.fillStyle = "rgba(212,216,232,0.8)";
        ctx.textAlign = "center";
        ctx.fillText(node.label.slice(0, 30), node.x ?? 0, (node.y ?? 0) + r + fontSize + 1);
      }
    },
    []
  );

  const legendEntries = [
    ...Object.entries(NOTE_TYPE_COLORS).map(([type, color]) => ({ label: `note: ${type}`, color })),
    { label: "entity ◇", color: NODE_TYPE_COLORS.entity },
    { label: "task ▪",   color: NODE_TYPE_COLORS.task },
  ];

  return (
    <div className="panel">
      <div className="panel-header">
        🕸 Knowledge Graph
        {graphData.nodes.length > 0 && (
          <span className="badge">
            {graphData.nodes.length} nodes · {graphData.links.length} edges
          </span>
        )}
        {loading && (
          <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 8 }}>loading…</span>
        )}
        <button className="refresh-btn" onClick={loadGraph} title="Reload full graph from brain">↻</button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div ref={containerRef} className="graph-container">
        <div className="graph-search-bar">
          <input
            placeholder="Filter nodes… (Enter)"
            value={searchQ}
            onChange={(e) => setSearchQ(e.target.value)}
            onKeyDown={handleKey}
          />
          <button className="btn" onClick={handleSearch}>Filter</button>
          <button className="btn" style={{ marginLeft: 4 }} onClick={loadGraph}>Reload</button>
        </div>

        <div className="graph-overlay">
          <div className="graph-legend">
            <div className="graph-legend-title">Node type</div>
            {legendEntries.map(({ label, color }) => (
              <div key={label} className="legend-row">
                <span className="legend-dot" style={{ background: color }} />
                {label}
              </div>
            ))}
          </div>
        </div>

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

        {selectedNode && (
          <div className="graph-detail-overlay">
            <div className="graph-detail-header">
              <span className={`note-type-badge ${selectedNode.note_type ?? selectedNode.type}`}>
                {selectedNode.type === "note"
                  ? (selectedNode.note_type ?? "note")
                  : selectedNode.type}
              </span>
              <button className="close-btn" onClick={() => setSelectedNode(null)}>×</button>
            </div>
            <div className="graph-detail-content scroll">
              {selectedNode.type === "note" && selectedNode.content}
              {selectedNode.type === "entity" && (
                <div>
                  <strong>{selectedNode.label}</strong>
                  {selectedNode.entity_type && (
                    <div style={{ color: "var(--text-muted)", marginTop: 6 }}>
                      {selectedNode.entity_type}
                    </div>
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
