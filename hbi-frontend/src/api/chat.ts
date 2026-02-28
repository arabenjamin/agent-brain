/**
 * /chat SSE streaming client.
 */
import { getChatUrl, getApiKey } from "./config";

export type ChatEventType =
  | "thinking"
  | "tool_call"
  | "tool_result"
  | "message"
  | "error"
  | "done";

export interface ChatEvent {
  type: ChatEventType;
  content?: string;
  message?: string;
  tool?: string;
  args?: unknown;
  success?: boolean;
  preview?: string;
}

export interface ChatHistoryMessage {
  role: "user" | "assistant";
  content: string;
}

export interface StreamChatOptions {
  message: string;
  history?: ChatHistoryMessage[];
  sessionId?: string;
  tools?: string[];
  onEvent: (event: ChatEvent) => void;
  signal?: AbortSignal;
}

/**
 * POST /chat and stream SSE events until `done` or abort.
 */
export async function streamChat(opts: StreamChatOptions): Promise<void> {
  const res = await fetch(getChatUrl(), {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${getApiKey()}`,
    },
    body: JSON.stringify({
      message: opts.message,
      history: opts.history ?? [],
      session_id: opts.sessionId,
      tools: opts.tools,
    }),
    signal: opts.signal,
  });

  if (!res.ok || !res.body) {
    opts.onEvent({
      type: "error",
      message: `HTTP ${res.status}: ${res.statusText}`,
    });
    return;
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });

    // Parse SSE lines.
    const lines = buf.split("\n");
    buf = lines.pop() ?? "";

    let eventType: string | null = null;
    for (const line of lines) {
      if (line.startsWith("event:")) {
        eventType = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        const data = line.slice(5).trim();
        if (!data) continue;
        try {
          const parsed: ChatEvent = JSON.parse(data);
          opts.onEvent(parsed);
          if (parsed.type === "done") return;
        } catch {
          // ignore malformed
        }
        eventType = null;
      }
    }
    void eventType; // suppress unused warning
  }
}
