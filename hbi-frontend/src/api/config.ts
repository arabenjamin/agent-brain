// When running via `npm run dev` these paths are proxied by Vite to
// http://localhost:3001 — no cross-origin issues.
// In production, set VITE_BRAIN_URL to point at the server.
//
// Settings are read from localStorage so the user can change them via the
// Settings modal without rebuilding. Env vars serve as defaults.

export const getBrainUrl = (): string =>
  localStorage.getItem("brain_url") ?? import.meta.env.VITE_BRAIN_URL ?? "";

export const getApiKey = (): string =>
  localStorage.getItem("api_key") ?? import.meta.env.VITE_API_KEY ?? "openclaw";

export const getMcpUrl  = (): string => `${getBrainUrl()}/mcp`;
export const getChatUrl = (): string => `${getBrainUrl()}/chat`;
