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

The brain URL and API key live in the browser's localStorage (Settings → Connection); `VITE_BRAIN_URL` / `VITE_API_KEY` are the defaults.

## Workspaces

`#/home` (dashboard) · `#/chat` · `#/work?view=inbox|active|scheduled|todos|history` · `#/knowledge?view=list|map|graph|timeline&q=&mode=auto|semantic|keyword|id&within=<topic>` · `#/system?view=health|logs|registry|architecture` · `#/settings?section=connection|agents|models|scheduler|about`. Ctrl-K jumps to any of them, opens a pasted note id, or searches notes. Old one-tab URLs (`#/tasks`, `#/graph`, …) redirect. The nav collapses to an icon rail («), remembered per browser.

## Layout

```
src/
  api/client.ts      the one HTTP client: apiRequest (raw Response), apiJson (throws ApiError)
  api/queries.ts     shared TanStack queries (queue/scheduler status, notes) + job-push invalidation
  api/search.ts      /api/search, entities, id lookup (one or many)
  api/entities.ts    entity merge proposals, merge / different / related, undo
  api/topics.ts      topics, coverage, the topic map, one topic
  api/system.ts      one-call system health, the registry, filtered logs, the live architecture
  api/mcp.ts         MCP SDK client (Streamable HTTP) + server-push fan-out
  api/chat.ts        /chat SSE stream
  router.ts          hash routing: #/<workspace>?<kind>=<id>
  workspaces.ts      the workspaces, their views, and legacy-URL redirects
  nav.ts             goTo / hrefTo: move between workspaces, keeping only the open drawer
  lib/               time (UTC in, local out), ids, scheduler state, useNow, chatDock (what a chat turn did, from its events)
  components/ui/     kit: Pill, Card, Stat, Tabs, EmptyState, Drawer, DrillBanner
  components/drawer/ entity drawer: note (a claim shows the article or video it was asserted in), entity, topic, task (lineage), job, schedule, todo, proposal
  components/chat/   Chat: the panel and the context dock under each answer
  components/system/ System: Health, Logs, Registry, Architecture (drawn from the developer plugin's /api/plugins/developer/architecture + plugins/developer/layout.yaml)
  components/home/   the dashboard
  components/knowledge/ search bar + Results / Map (topic hubs) / Neighbourhood / Timeline
  components/work/   Work's lanes (Inbox, Active, Scheduled + timeline, Todos, History); the Inbox holds entity merge proposals
  components/schedules/ the schedule and routing-chain editors; a save keeps every step field they don't edit
  components/todos/  todo model (overdue rule) and form
  components/settings/ the Settings page and its sections (Agents = AgentsEditor)
  components/palette/ Ctrl-K
  components/<panel>/ the workspaces
  styles/main.css    tokens (:root) + panel styles; styles/ui.css for the kit
```

## Conventions

- Fetch through `api/client.ts`; never build an auth header in a component. Action buttons call tools with `callToolStrict`, which throws on a tool error.
- Anything shown in more than one view is a shared query in `api/queries.ts`.
- Open an entity with `openEntity("note", id)`. It is URL state, so every open note is a link.
- Colours come from the `:root` tokens. Empty and error states say what was checked.
- A dashboard number links to its rows (`Stat href=…`); the target view takes the filter as a prop and shows a `DrillBanner`.
