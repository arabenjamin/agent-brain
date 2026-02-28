import { useCallback, useEffect, useRef, useState } from "react";
import ForceGraph2D from "react-force-graph-2d";
import type { ForceGraphMethods } from "react-force-graph-2d";
import { callTool } from "../../api/mcp";

// ── Types ─────────────────────────────────────────────────────────────────────

interface RawNote {
  id: string;
  content: string;
  note_type?: string;
}

interface RelatedNote {
  note_id?: string;
  id?: string;
  content: string;
  similarity?: number;
}

interface GraphNode {
  id: string;
  label: string;
  type: string;
  color: string;
  val: number;
  // injected by force-graph at runtime
  x?: number;
  y?: number;
  __bckgDimensions?: [number, number];
}

interface GraphLink {
  source: string;
  target: string;
  similarity?: number;
}

interface GraphData {
  nodes: GraphNode[];
  links: GraphLink[];
}

// ── Colour map ─────────────────────────────────────────────────────────────────

const TYPE_COLORS: Record<string, string> = {
  semantic:     "#4f8ef7",
  episodic:     "#22d3ee",
  reflection:   "#a78bfa",
  consolidated: "#4ade80",
  outcome:      "#fbbf24",
  inference:    "#f87171",
};

function nodeColor(type: string) {
  return TYPE_COLORS[type] ?? "#7a8099";
}

// ── Main component ────────────────────────────────────────────────────────────

export default function GraphPanel() {
  const [graphData, setGraphData] = useState<GraphData>({ nodes: [], links: [] });
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [searchQ, setSearchQ] = useState("");
  const fgRef = useRef<ForceGraphMethods<GraphNode, GraphLink>>(undefined);

  const buildGraph = useCallback(async (query: string) => {
    setLoading(true);
    setError(null);
    try {
      const json = await callTool("search_notes", { query: query || " ", limit: 40 });
      const data = JSON.parse(json);
      const notes: RawNote[] = data.notes ?? [];

      const nodeMap = new Map<string, GraphNode>();
      const links: GraphLink[] = [];

      // Add seed nodes.
      for (const n of notes) {
        nodeMap.set(n.id, {
          id: n.id,
          label: (n.content || "").slice(0, 60) + ((n.content || "").length > 60 ? "…" : ""),
          type: n.note_type ?? "semantic",
          color: nodeColor(n.note_type ?? "semantic"),
          val: 3,
        });
      }

      // Fetch related edges for up to 20 seed nodes (to avoid rate-limiting).
      const seeds = notes.slice(0, 20);
      const relatedResults = await Promise.allSettled(
        seeds.map((n) =>
          callTool("find_related_notes", { note_id: n.id }).then((j) => ({
            sourceId: n.id,
            related: (JSON.parse(j).related_notes ?? []) as RelatedNote[],
          }))
        )
      );

      for (const result of relatedResults) {
        if (result.status !== "fulfilled") continue;
        const { sourceId, related } = result.value;
        for (const r of related) {
          const targetId = r.note_id ?? r.id;
          if (!targetId) continue;

          // Add target node if not already present.
          if (!nodeMap.has(targetId)) {
            nodeMap.set(targetId, {
              id: targetId,
              label: (r.content || "").slice(0, 60) + ((r.content || "").length > 60 ? "…" : ""),
              type: "semantic",
              color: nodeColor("semantic"),
              val: 2,
            });
          }

          // Avoid duplicate links.
          const alreadyLinked = links.some(
            (l) =>
              (l.source === sourceId && l.target === targetId) ||
              (l.source === targetId && l.target === sourceId)
          );
          if (!alreadyLinked) {
            links.push({ source: sourceId, target: targetId, similarity: r.similarity });
          }
        }
      }

      setGraphData({ nodes: Array.from(nodeMap.values()), links });
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    buildGraph("");
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const handleKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") buildGraph(searchQ);
  };

  // Custom canvas node rendering.
  const paintNode = useCallback(
    (node: GraphNode, ctx: CanvasRenderingContext2D, globalScale: number) => {
      const fontSize = Math.max(8, 12 / globalScale);
      const label = node.label;
      const r = Math.sqrt(node.val ?? 3) * 4;

      ctx.beginPath();
      ctx.arc(node.x ?? 0, node.y ?? 0, r, 0, 2 * Math.PI, false);
      ctx.fillStyle = node.color;
      ctx.fill();
      ctx.strokeStyle = "rgba(255,255,255,0.15)";
      ctx.lineWidth = 0.5;
      ctx.stroke();

      if (globalScale >= 1.5) {
        ctx.font = `${fontSize}px "JetBrains Mono", monospace`;
        ctx.fillStyle = "rgba(212,216,232,0.8)";
        ctx.textAlign = "center";
        ctx.fillText((label || "").slice(0, 30), node.x ?? 0, (node.y ?? 0) + r + fontSize + 1);
      }
    },
    []
  );

  return (
    <div className="panel">
      <div className="panel-header">
        🕸 Knowledge Graph
        {graphData.nodes.length > 0 && (
          <span className="badge">{graphData.nodes.length} nodes · {graphData.links.length} edges</span>
        )}
        {loading && (
          <span style={{ color: "var(--text-muted)", fontSize: 11, marginLeft: 8 }}>loading…</span>
        )}
        <button className="refresh-btn" onClick={() => buildGraph(searchQ)} title="Refresh">↻</button>
      </div>

      {error && <div className="error-msg">{error}</div>}

      <div className="graph-container">
        <div className="graph-search-bar">
          <input
            placeholder="Filter graph… (Enter)"
            value={searchQ}
            onChange={(e) => setSearchQ(e.target.value)}
            onKeyDown={handleKey}
          />
          <button className="btn" onClick={() => buildGraph(searchQ)}>
            Load
          </button>
        </div>

        <div className="graph-overlay">
          <div className="graph-legend">
            <div className="graph-legend-title">Node type</div>
            {Object.entries(TYPE_COLORS).map(([type, color]) => (
              <div key={type} className="legend-row">
                <span className="legend-dot" style={{ background: color }} />
                {type}
              </div>
            ))}
          </div>
        </div>

        {graphData.nodes.length === 0 && !loading && (
          <div className="empty-state" style={{ position: "absolute", inset: 0 }}>
            <span className="icon">🕸</span>
            <span>No nodes — try refreshing once the brain has notes stored</span>
          </div>
        )}

        <ForceGraph2D
          ref={fgRef}
          graphData={graphData}
          nodeColor={(n) => (n as GraphNode).color}
          nodeVal={(n) => (n as GraphNode).val}
          nodeLabel={(n) => (n as GraphNode).label}
          linkColor={() => "rgba(79,142,247,0.25)"}
          linkWidth={(l) => {
            const ll = l as GraphLink;
            return ll.similarity ? ll.similarity * 2 : 0.5;
          }}
          backgroundColor="transparent"
          nodeCanvasObject={paintNode as Parameters<typeof ForceGraph2D>[0]["nodeCanvasObject"]}
          nodeCanvasObjectMode={() => "after"}
          width={undefined}
          height={undefined}
        />
      </div>
    </div>
  );
}
