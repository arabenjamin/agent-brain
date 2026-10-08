# HBI — Human Brain Interface

The web UI for Agent Brain: React 19 + Vite + TypeScript, served by nginx in the `hbi-frontend` compose service (host port 3002), which proxies `/mcp`, `/chat`, `/api`, `/todos`, `/scheduled-tasks` and `/scheduler-config` to the brain.

The redesign from tabs to workspaces is planned in [`project-docs/HBI_REDESIGN_PLAN.md`](../project-docs/HBI_REDESIGN_PLAN.md). The rules for new code are in `project-docs/subsystem-rules.md` § HBI frontend foundations.

## Commands

```bash
npm run dev      # Vite on :5173, proxied to the brain on localhost:3001
npm test         # vitest (api client, router, helpers, shared polling)
npm run lint
npm run build    # tsc -b + vite build
docker compose up -d --build hbi-frontend   # rebuild the served UI
```

Settings (brain URL, API key) live in the browser's localStorage via the ⚙ Settings modal; `VITE_BRAIN_URL` / `VITE_API_KEY` are the defaults.

## Layout

```
src/
  api/client.ts      the one HTTP client: apiRequest (raw Response), apiJson (throws ApiError)
  api/queries.ts     shared TanStack queries (queue/scheduler status, notes) + job-push invalidation
  api/mcp.ts         MCP SDK client (Streamable HTTP) + server-push fan-out
  api/chat.ts        /chat SSE stream
  router.ts          hash routing: #/<workspace>?<kind>=<id>
  lib/               time (UTC in, local out), ids
  components/ui/     kit: Pill, Card, Stat, Tabs, EmptyState, Drawer
  components/drawer/ entity drawer (openEntity / closeEntity)
  components/<panel>/ the workspaces
  styles/main.css    tokens (:root) + panel styles; styles/ui.css for the kit
```

## Conventions

- Fetch through `api/client.ts`; never build an auth header in a component.
- Anything shown in more than one view is a shared query in `api/queries.ts`.
- Open an entity with `openEntity("note", id)`. It is URL state, so every open note is a link.
- Colours come from the `:root` tokens. Empty and error states say what was checked.
