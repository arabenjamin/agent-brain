# Next Steps — HBI Frontend Polish

Status: All four panels working. These are the known issues to address next.

---

## 1. Graph Container Sizing

**Problem:** `ForceGraph2D` renders at a fixed internal size instead of filling its flex parent. The graph either clips or leaves dead space.

**Fix:** Use a `ResizeObserver` (or `useLayoutEffect` + `useRef` on the container `div`) to measure the container's actual pixel dimensions and pass them as explicit `width` and `height` props to `ForceGraph2D`.

```tsx
// GraphPanel.tsx — inside the component
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

// Then:
<div ref={containerRef} className="graph-canvas-wrapper">
  <ForceGraph2D width={dims.w} height={dims.h} ... />
</div>
```

**File:** `hbi-frontend/src/components/graph/GraphPanel.tsx`

---

## 2. Graph Node Click — Open Note Content

**Problem:** Clicking a graph node does nothing.

**Fix:** Add `onNodeClick` prop to `ForceGraph2D`. On click, set a `selectedNode` state variable and render a side panel (or modal overlay) showing the full note content fetched from the brain.

```tsx
const [selectedNote, setSelectedNote] = useState<{ id: string; content: string } | null>(null);

const handleNodeClick = useCallback(async (node: GraphNode) => {
  // search_notes with the exact node id won't work — need a get_note tool or search by id
  // Workaround: search for the note content using its label as the query
  const json = await callTool("search_notes", { query: node.label, limit: 1 });
  const data = JSON.parse(json);
  const match = data.notes?.[0];
  if (match) setSelectedNote({ id: match.id, content: match.content });
}, []);

<ForceGraph2D onNodeClick={handleNodeClick} ... />
```

A `get_note` tool (by id) would be cleaner — currently `search_notes` is the only retrieval path.

**File:** `hbi-frontend/src/components/graph/GraphPanel.tsx`

---

## 3. MCP Client Reconnection

**Problem:** `getMcpClient()` returns a cached singleton. If the brain restarts, all MCP tool calls fail silently until the browser tab is refreshed.

**Fix:** Wrap `callTool()` to catch transport errors, call `resetMcpClient()`, and retry once.

```ts
// api/mcp.ts
export async function callTool(name: string, args: Record<string, unknown> = {}): Promise<string> {
  try {
    const client = await getMcpClient();
    const result = await client.callTool({ name, arguments: args });
    const content = result.content as Array<{ type: string; text?: string }>;
    return content.filter((c) => c.type === "text").map((c) => c.text ?? "").join("\n");
  } catch (e) {
    // Attempt reconnect once on transport error
    resetMcpClient();
    const client = await getMcpClient();
    const result = await client.callTool({ name, arguments: args });
    const content = result.content as Array<{ type: string; text?: string }>;
    return content.filter((c) => c.type === "text").map((c) => c.text ?? "").join("\n");
  }
}
```

**File:** `hbi-frontend/src/api/mcp.ts`

---

## 4. Knowledge Panel Initial Load

**Problem:** On mount, the Knowledge panel sends `query: " "` (a single space) as a workaround for an empty search string. This is a hack that may return arbitrary results.

**Fix options:**
- Add a `list_notes` / `recent_notes` tool to the brain that returns notes ordered by `last_accessed_at desc`.
- Or: use `review_due_notes` for initial load — shows the most "overdue" notes, which is a meaningful default.
- Or: render an empty state on mount and only search when the user types.

The simplest fix without a new backend tool is to show an empty state initially and only query on user input.

**File:** `hbi-frontend/src/components/knowledge/KnowledgePanel.tsx`

---

## 5. Task Panel — Subtask Tree View

**Problem:** Subtasks (tasks with `parent_id` set) appear flat in the task list alongside their parent tasks.

**Fix:** Group tasks by `parent_id` after fetching, render parent tasks with indented children beneath them.

```tsx
// Build a tree
const roots = tasks.filter(t => !t.parent_id);
const children = new Map<string, Task[]>();
tasks.filter(t => t.parent_id).forEach(t => {
  const list = children.get(t.parent_id!) ?? [];
  list.push(t);
  children.set(t.parent_id!, list);
});
// Render roots, then their children indented
```

**File:** `hbi-frontend/src/components/tasks/TaskPanel.tsx`

---

## 6. Auth UI — Settings Screen

**Problem:** `VITE_API_KEY` defaults to `"openclaw"` hardcoded in `config.ts`. No way to change it without modifying the source or env file.

**Fix:** Read from `localStorage` with a fallback to the env var. Add a small Settings panel (or gear icon modal) where the user can enter and save an API key.

```ts
// config.ts
export const API_KEY =
  localStorage.getItem("brain_api_key") ?? import.meta.env.VITE_API_KEY ?? "openclaw";
```

The settings panel only needs two fields: Brain URL and API key, with a Save button that writes to `localStorage` and reloads.

---

## 7. Production Build

**Problem:** `VITE_BRAIN_URL` must be set at build time for production; the Vite proxy is dev-only. Without it the frontend hardcodes empty string and all requests go to the same origin.

**Fix:** Document the production deployment steps clearly:
```bash
VITE_BRAIN_URL=http://your-brain-host:3001 VITE_API_KEY=your-key npm run build
# Serve the dist/ folder from any static host
```

Or: switch to a runtime-configurable approach (e.g., `window.__BRAIN_CONFIG__` injected by the server).

---

## Priority Order

1. **Graph sizing** (most visually broken — affects usability immediately)
2. **MCP reconnect** (silent failure is confusing in day-to-day use)
3. **Knowledge initial load** (minor but the `" "` hack is fragile)
4. **Graph node click** (nice-to-have — requires reading full note content)
5. **Task tree view** (cosmetic improvement)
6. **Auth UI** (low urgency while running locally)
7. **Production build** (only needed before deploying publicly)
