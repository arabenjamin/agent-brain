# Human Brain Interface (HBI) - Implementation Plan

Status as of 2026-02-28: **Phases 1–3 complete. Frontend running.**

---

## 1. Objectives & Features

- **Chat & Reasoning:** Chat interface with real-time `thinking`, `tool_call`, `tool_result`, and `message` SSE events.
- **Session Management:** Task & queue dashboard with live status and auto-refresh.
- **Knowledge Graph Visualization:** Force-directed graph of notes and similarity edges.
- **Graph Querying:** Search-and-browse panel for `search_notes` / `find_related_notes`.
- **Logs:** Planned (not yet built).

---

## 2. Tech Stack

| Concern | Choice |
|---|---|
| Framework | React 18 + Vite 7 (TypeScript, `verbatimModuleSyntax`) |
| Styling | Vanilla CSS — dark monospace theme, no utility framework |
| MCP protocol | `@modelcontextprotocol/sdk` — `StreamableHTTPClientTransport` |
| Graph visualization | `react-force-graph-2d` |
| Dev proxy | Vite proxy `/mcp` and `/chat` → `http://localhost:3001` |

---

## 3. Architecture

### 3A. Backend — Server-Side Chat (`POST /chat`)

The brain runs the full LLM ↔ tool loop server-side. No API key is needed in the frontend.

**Rust implementation:** `src/services/chat.rs` → `ChatService`
- Hooked into `McpServerCore` via `chat_service()` factory
- `HttpTransportConfig::with_chat_service()` attaches it to the `/chat` route

**Request** (`POST /chat`, `Content-Type: application/json`):
```json
{
  "message": "What do you know about robotics?",
  "history": [
    { "role": "user",      "content": "Hello!" },
    { "role": "assistant", "content": "Hi! How can I help?" }
  ],
  "session_id": "optional",
  "tools": ["search_notes", "reason"]
}
```
`history` and `tools` are optional.

**SSE response events:**

| Event | Payload |
|---|---|
| `thinking`    | `{"type":"thinking","content":"..."}` |
| `tool_call`   | `{"type":"tool_call","tool":"...","args":{}}` |
| `tool_result` | `{"type":"tool_result","tool":"...","success":true,"preview":"..."}` |
| `message`     | `{"type":"message","content":"..."}` |
| `error`       | `{"type":"error","message":"..."}` |
| `done`        | `{"type":"done"}` |

Provider strategy selected at runtime from `LlmConfig`:
- **Anthropic** — native `tool_use` blocks via `POST /v1/messages`
- **Ollama / Gemini** — text loop with `<tool_call>{"tool":"...","args":{}}</tool_call>` parsing

### 3B. MCP Client Layer

```typescript
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";

const transport = new StreamableHTTPClientTransport(
  new URL(MCP_URL, window.location.href),   // resolves relative paths
  { requestInit: { headers: { Authorization: "Bearer openclaw" } } }
);
```

> **Not** `SSEClientTransport` — that is the legacy SSE-only transport and uses a different handshake. The brain implements the MCP Streamable HTTP spec (POST `/mcp` + GET `/mcp` SSE).

### 3C. Frontend Structure

```
hbi-frontend/
├── src/
│   ├── api/
│   │   ├── config.ts          BRAIN_URL, API_KEY (VITE_* env or defaults)
│   │   ├── mcp.ts             StreamableHTTPClientTransport singleton + callTool()
│   │   └── chat.ts            streamChat() — fetch + SSE parser
│   ├── components/
│   │   ├── chat/ChatPanel     SSE stream UI: thinking/tool/message bubbles
│   │   ├── tasks/TaskPanel    list_tasks + queue_status, auto-refresh every 8s
│   │   ├── knowledge/         search_notes + find_related_notes split-pane reader
│   │   └── graph/GraphPanel   react-force-graph-2d, seed notes → edges
│   ├── styles/main.css        dark monospace design system
│   └── App.tsx                tab sidebar, lazy-loaded panels
├── vite.config.ts             dev proxy: /mcp, /chat → localhost:3001
└── package.json
```

---

## 4. Running It

```bash
# Start the brain (must be built with /chat support)
cd /home/ara/agent-brain
docker compose up -d --build

# Start the frontend dev server
cd hbi-frontend
npm run dev
# → http://localhost:5173
```

**Environment (optional `.env` in `hbi-frontend/`):**
```
VITE_BRAIN_URL=   # leave blank to use Vite proxy (recommended for dev)
VITE_API_KEY=openclaw
```

---

## 5. Status

| Panel | Status | Notes |
|---|---|---|
| Chat | ✅ Working | Streams all event types end-to-end |
| Tasks & Queue | ✅ Working | Polls every 8s, status filter |
| Knowledge | ✅ Working | search_notes + find_related_notes |
| Graph | ✅ Working | Force-graph renders; see known issues |

---

## 6. Known Issues / Next Session

- **Graph container sizing** — `ForceGraph2D` doesn't auto-fill its flex parent. Needs a `ResizeObserver` / `useLayoutEffect` to pass measured `width`/`height` props.
- **Graph node click** — clicking a node should open the full note content (currently no action).
- **MCP reconnection** — `getMcpClient()` singleton has no reconnect logic. If the brain restarts, the tab must be refreshed.
- **Knowledge initial load** — sends query `" "` (single space) as a workaround for an empty search. Should use a dedicated "recent notes" query or a `list_notes` tool.
- **Task panel subtask display** — subtasks (`parent_id` set) are shown flat alongside parent tasks. Could be indented into a tree view.
- **No logs panel** — telemetry / tracing output not yet surfaced in the UI.
- **No auth UI** — API key is hardcoded in `config.ts`. Should be a settings screen or read from `localStorage`.
- **Production build** — `VITE_BRAIN_URL` must be set; Vite proxy only works in dev mode.
