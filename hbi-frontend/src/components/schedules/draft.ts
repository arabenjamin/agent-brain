/**
 * Asking `schedule-author` for workflow steps, and checking what it returns
 * before the form shows it as ready.
 *
 * An agent that sees only tool NAMES guesses their arguments (`reason.prompt`
 * for `question`, `store_note.note` for `content`), and an "edit these steps"
 * request that doesn't carry the steps becomes a fresh draft. The draft then
 * looked clean in the editor and failed only at Save (todo "Scheduled Tasks
 * Editor needs some love", 2026-10-16). So the task carries each tool's
 * arguments and the current workflow, and the draft goes through the brain's
 * own save check (`POST /workflows/check`) the moment it arrives.
 */
import { useEffect, useState } from "react";
import { apiRequest } from "../../api/client";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Schema = Record<string, any>;

let cachedSchemas: Record<string, Schema> | null = null;

/** Every registered tool's `input_schema`, by name ({} until loaded or on an older backend). */
export function useToolSchemas(): Record<string, Schema> {
  const [schemas, setSchemas] = useState<Record<string, Schema>>(cachedSchemas ?? {});
  useEffect(() => {
    if (cachedSchemas) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await apiRequest(`/api/tools?schemas=true`, { method: "GET" });
        if (!res.ok) return;
        const data = await res.json();
        if (data?.schemas && typeof data.schemas === "object") {
          cachedSchemas = data.schemas;
          if (!cancelled) setSchemas(data.schemas);
        }
      } catch { /* offline: names only */ }
    })();
    return () => { cancelled = true; };
  }, []);
  return schemas;
}

/**
 * One argument as the agent reads it: `name*` when required, then its choices
 * or its type. The type is shown even for a string: without it Gemma read
 * `notify_user.ask` (a question for the user) as a flag and wrote `ask: true`.
 */
function argSig(name: string, prop: Schema, required: boolean): string {
  const s = required ? `${name}*` : name;
  if (Array.isArray(prop?.enum) && prop.enum.length) return `${s}: ${prop.enum.join("|")}`;
  const t = Array.isArray(prop?.type) ? prop.type.filter((x: string) => x !== "null").join("|") : prop?.type;
  return t ? `${s}: ${t}` : s;
}

/**
 * One line per tool, `reason(question*, context, mode: infer|explain)`: the
 * arguments a step may pass (`*` = required). A tool with no schema is listed
 * by name alone.
 */
export function toolSignatures(tools: string[], schemas: Record<string, Schema>): string {
  return tools
    .map((t) => {
      const s = schemas[t];
      if (!s) return t;
      const req = new Set<string>(Array.isArray(s.required) ? s.required : []);
      const props = Object.entries((s.properties ?? {}) as Record<string, Schema>);
      // Required first, so a long optional list can't hide them.
      props.sort(([a], [b]) => Number(req.has(b)) - Number(req.has(a)));
      return `${t}(${props.map(([k, p]) => argSig(k, p, req.has(k))).join(", ")})`;
    })
    .join("\n");
}

/** What the agent is asked for: the request, the workflow as it stands, and the tools it may use. */
export function buildDraftTask({
  request,
  current,
  tools,
  schemas,
  fix,
}: {
  request: string;
  /** The workflow being edited; its steps are what the request changes. */
  current?: { name: string; every: string | null; steps: unknown[] } | null;
  tools: string[];
  schemas: Record<string, Schema>;
  /** A previous draft the save check refused, and why. */
  fix?: { previous: string; errors: string };
}): string {
  let task = current?.steps.length
    ? `Change this scheduled workflow as the request says. Keep every step, argument and setting the request ` +
      `does not ask to change, and return the WHOLE workflow, not only the changed steps. You may add an "id" ` +
      `to any step, so a later step can use its output as {{steps.<id>}}.\n\n` +
      `Request:\n"${request}"\n\nThe workflow now:\n` +
      JSON.stringify({ name: current.name, every: current.every, steps: current.steps }, null, 2)
    : `Draft a scheduled workflow for this request:\n"${request}"`;
  task +=
    `\n\nTools a step may use, with the arguments each takes (* = required; use these names exactly, ` +
    `and only these):\n${toolSignatures(tools, schemas)}`;
  if (fix) {
    task +=
      `\n\nYour previous draft was rejected by the save check:\n${fix.errors}\n\n` +
      `The draft was:\n${fix.previous}\n\nReturn the corrected object.`;
  }
  return task;
}

/** `refused`: the check ran and the draft fails it (worth sending back to the agent); else it couldn't run. */
export type CheckResult = { ok: true; warnings: string[] } | { ok: false; refused: boolean; error: string };

/**
 * Run the brain's save checks on a workflow body without saving it. `id`
 * checks it as an edit of that workflow; without, as a new one. A check that
 * can't run (an older backend, offline) says so rather than passing.
 */
export async function checkWorkflow(body: Record<string, unknown>): Promise<CheckResult> {
  try {
    const res = await apiRequest(`/workflows/check`, { method: "POST", body: JSON.stringify(body) });
    const data = (await res.json().catch(() => ({}))) as { warnings?: string[]; error?: string };
    if (res.ok) return { ok: true, warnings: data.warnings ?? [] };
    return { ok: false, refused: res.status === 400, error: data.error ?? `save check: HTTP ${res.status}` };
  } catch (e) {
    return { ok: false, refused: false, error: `save check could not run: ${e instanceof Error ? e.message : String(e)}` };
  }
}
