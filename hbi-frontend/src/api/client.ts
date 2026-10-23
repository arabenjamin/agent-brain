/**
 * The one HTTP client for the brain's REST surface.
 *
 * Every panel used to carry its own `apiFetch` / `authHeaders` pair (five
 * copies, plus ~40 inline `fetch` calls that each re-built the bearer header).
 * They all go through here now, so the base URL, the API key and the error
 * shape are decided in one place.
 *
 * - `apiRequest` returns the raw `Response` and never throws on a non-2xx —
 *   a drop-in for the old helpers, whose callers check `res.ok` themselves.
 * - `apiJson` throws `ApiError` on a non-2xx (carrying the brain's `{error}`
 *   message) and parses the body. New code, and every shared query, uses it.
 */
import { getApiKey, getBrainUrl } from "./config";

/** A non-2xx response. `message` is the brain's `{error}` text when it sent one. */
export class ApiError extends Error {
  readonly status: number;
  readonly path: string;

  constructor(status: number, path: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.path = path;
  }
}

/**
 * A plugin's routes (`Plugin::routes`), served under its name. They answer
 * 404 "plugin '<name>' is not loaded" while the plugin is off.
 */
export function pluginApi(name: string): string {
  return `/api/plugins/${name}`;
}

/** Absolute-or-proxied URL for a brain path such as `/api/notes`. */
export function apiUrl(path: string): string {
  return `${getBrainUrl()}${path}`;
}

/** The bearer header, or nothing when no key is configured. */
export function authHeaders(): Record<string, string> {
  const key = getApiKey();
  // Everything the HBI sends is the owner's own action, so an agent it runs
  // may use its paid model pin; the brain's scheduled work never does.
  const owner = { "x-brain-initiator": "owner" };
  return key ? { Authorization: `Bearer ${key}`, ...owner } : owner;
}

/** Builds a query string, dropping undefined/empty values. Includes the `?`. */
export function query(params: Record<string, string | number | boolean | undefined | null>): string {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === "") continue;
    qs.set(k, String(v));
  }
  const s = qs.toString();
  return s ? `?${s}` : "";
}

/**
 * Fetch a brain path with auth. A string body is sent as JSON unless the
 * caller set its own `Content-Type`. Never throws on HTTP status.
 */
export async function apiRequest(path: string, init: RequestInit = {}): Promise<Response> {
  const headers: Record<string, string> = {
    ...authHeaders(),
    ...((init.headers as Record<string, string> | undefined) ?? {}),
  };
  const hasContentType = Object.keys(headers).some((h) => h.toLowerCase() === "content-type");
  if (typeof init.body === "string" && !hasContentType) {
    headers["Content-Type"] = "application/json";
  }
  return fetch(apiUrl(path), { ...init, headers });
}

/** The brain's error text from a failed response, falling back to the status line. */
async function errorMessage(res: Response): Promise<string> {
  const fallback = `${res.status} ${res.statusText}`.trim();
  try {
    const text = await res.text();
    if (!text) return fallback;
    try {
      const body = JSON.parse(text) as { error?: unknown };
      if (typeof body.error === "string" && body.error) return body.error;
    } catch {
      // not JSON — use the text itself
    }
    return text.slice(0, 300);
  } catch {
    return fallback;
  }
}

/** Fetch and parse JSON; throws `ApiError` on a non-2xx. */
export async function apiJson<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await apiRequest(path, init);
  if (!res.ok) throw new ApiError(res.status, path, await errorMessage(res));
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** `apiJson` with a JSON-encoded body. */
export function apiSend<T>(method: "POST" | "PUT" | "PATCH" | "DELETE", path: string, body?: unknown): Promise<T> {
  return apiJson<T>(path, {
    method,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}
