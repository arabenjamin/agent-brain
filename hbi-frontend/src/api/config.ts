// When running via `npm run dev` these paths are proxied by Vite to
// http://localhost:3001 — no cross-origin issues.
// In production, set VITE_BRAIN_URL to point at the server.
const BASE = import.meta.env.VITE_BRAIN_URL ?? "";

export const BRAIN_URL = BASE;
export const API_KEY   = import.meta.env.VITE_API_KEY ?? "openclaw";
export const MCP_URL   = `${BASE}/mcp`;
export const CHAT_URL  = `${BASE}/chat`;
