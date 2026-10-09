/**
 * Asking an agent to help fill a form (HBI plan §9.4 `<AgentAssist>`): one
 * `run_agent` call, read-only so a draft is never stored anywhere, and the
 * parts of its result a form needs: the answer, the model it ran on, and the
 * tools it actually ran, which is the receipt for what the answer claims.
 */
import { apiRequest } from "../api/client";

export interface AssistRun {
  answer: string;
  /** False when the agent ran but produced no answer. */
  ok: boolean;
  /** The model the agent's loop started on. */
  model: string;
  agent: string;
  trace: { tool: string; success: boolean }[];
}

/**
 * The `run_agent` result as JSON, or an error naming what went wrong. A
 * rejection (an unknown agent, a model it cannot use) arrives as `isError`
 * with the reason as its text.
 */
export function parseRunAgentResult(rpc: unknown): AssistRun {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const result = (rpc as any)?.result;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const err = (rpc as any)?.error;
  if (err) throw new Error(err.message ?? JSON.stringify(err));
  const text: string = result?.content?.[0]?.text ?? "";
  if (result?.isError) throw new Error(text || "The agent run was rejected");
  let outer: Record<string, unknown>;
  try {
    outer = JSON.parse(text);
  } catch {
    return { answer: text, ok: !!text.trim(), model: "", agent: "", trace: [] };
  }
  return {
    answer: typeof outer.answer === "string" ? outer.answer : "",
    ok: outer.ok === true,
    model: typeof outer.model === "string" ? outer.model : "",
    agent: typeof outer.agent === "string" ? outer.agent : "",
    trace: Array.isArray(outer.tool_trace)
      ? (outer.tool_trace as { tool?: unknown; success?: unknown }[]).map((t) => ({
          tool: String(t.tool ?? ""),
          success: t.success === true,
        }))
      : [],
  };
}

/** Run `agent` on `task`, read-only, on `model` ("" = the agent's own). */
export async function runAssist(agent: string, task: string, model: string): Promise<AssistRun> {
  const args: Record<string, unknown> = { agent, task, read_only: true };
  if (model.trim()) args.model = model.trim();
  const res = await apiRequest(`/mcp`, {
    method: "POST",
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "run_agent", arguments: args } }),
  });
  if (!res.ok) throw new Error(`run_agent: HTTP ${res.status}`);
  return parseRunAgentResult(await res.json());
}

/**
 * The first fenced block of `lang` in an answer (```json … ```), else the
 * first bare JSON value of the wanted shape. `null` when there is none.
 */
export function extractJson(answer: string, shape: "array" | "object"): string | null {
  const open = shape === "array" ? "\\[" : "\\{";
  const close = shape === "array" ? "\\]" : "\\}";
  const fenced = answer.match(new RegExp("```(?:json)?\\s*(" + open + "[\\s\\S]*?" + close + ")\\s*```"));
  if (fenced) return fenced[1];
  const bare = answer.match(new RegExp(open + "[\\s\\S]*" + close));
  return bare ? bare[0] : null;
}

/**
 * The agent's own text, without the relay header and the tool receipt that
 * `format_subagent_findings` wraps it in (`[FINDINGS FROM …]` before,
 * `[TOOLS … ACTUALLY RAN …]` and `SOURCES:` after), for a form field.
 */
export function answerBody(answer: string): string {
  let s = answer.replace(/^\s*\[FINDINGS FROM [^\]]*\]\s*/, "");
  const cut = s.search(/\n+\[TOOLS `[^`]*` ACTUALLY RAN/);
  if (cut >= 0) s = s.slice(0, cut);
  return s.trim();
}
