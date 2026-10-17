/**
 * Asking an agent to draft a program (PROGRAMS_PLAN.md Phase 2b), the way the
 * workflow form asks schedule-author for steps: the task carries the program
 * schema and what exists (programs, workflows in no program, topics,
 * agents), the answer is one JSON object, and the brain's own save check
 * (`POST /api/programs/check`) judges it before the form shows it as ready.
 */
import type { ProgramDef } from "../api/programs";
import { extractJson } from "./assist";

export interface DraftContext {
  request: string;
  /** The form as it stands (an edit, or what the person already typed). */
  current: Partial<ProgramDef>;
  schema: unknown;
  programs: { name: string; label: string; kind: string; parent: string | null }[];
  /** Workflows no program owns, the ones a new program may take. */
  freeWorkflows: { name: string; description?: string | null; enabled: boolean }[];
  topics: { name: string; label: string }[];
  agents: string[];
  /** A redraft: the previous answer and what the check said about it. */
  fix?: { previous: string; errors: string };
}

const clip = (s: string | null | undefined, n: number) => {
  const t = (s ?? "").replace(/\s+/g, " ").trim();
  return t.length > n ? `${t.slice(0, n - 1)}…` : t;
};

export function buildProgramTask(c: DraftContext): string {
  const lines = [
    "Draft a PROGRAM for the agent brain's HBI form. Return ONE JSON object that fits the schema below, and nothing after it.",
    "",
    `THE REQUEST: ${c.request || "(none: improve the current draft)"}`,
    "",
    "THE FORM AS IT STANDS (keep what the request does not change):",
    JSON.stringify(c.current),
    "",
    "THE PROGRAM SCHEMA (schemas/program.json):",
    JSON.stringify(c.schema),
    "",
    "EXISTING PROGRAMS (a `parent` must be one of these names):",
    ...c.programs.map((p) => `- ${p.name}: ${p.label} (${p.kind}${p.parent ? `, under ${p.parent}` : ""})`),
    "",
    "WORKFLOWS IN NO PROGRAM (take workflows only from these, unless the request names another):",
    ...(c.freeWorkflows.length
      ? c.freeWorkflows.map((w) => `- ${w.name}${w.enabled ? "" : " (off)"}: ${clip(w.description, 140)}`)
      : ["- (none: every workflow already has a program; leave `workflows` empty)"]),
    "",
    `TOPICS: ${c.topics.map((t) => `${t.name} (${t.label})`).join(", ")}`,
    `AGENTS: ${c.agents.join(", ")}`,
    "",
    "Rules: `name` is a lowercase slug and is the program's identity. Use only the names listed above. " +
      "Write `charter` (what it is for) and `success` (what good output looks like) plainly from the request. " +
      "Leave out `status` and `finding`. Set `review` ({\"every\": \"30d\"}) for a department.",
  ];
  if (c.fix) {
    lines.push(
      "",
      "YOUR PREVIOUS DRAFT WAS REFUSED. Fix exactly these problems and return the whole object again:",
      c.fix.errors,
      "Previous draft:",
      c.fix.previous,
    );
  }
  return lines.join("\n");
}

const KEYS = [
  "name", "label", "kind", "parent", "charter", "success", "topics", "workflows",
  "keep_running", "agents", "apis", "review", "budget",
] as const;

/**
 * The program in an agent's answer: one JSON object with at least `name` or
 * `kind`, keeping only the fields a form edits. A string says why there is
 * none.
 */
export function readProgramDraft(body: string): { draft: Partial<ProgramDef>; text: string } | string {
  const obj = extractJson(body, "object");
  if (!obj) return `No JSON object in the agent's reply:\n${body.slice(0, 400)}`;
  let o: Record<string, unknown>;
  try {
    o = JSON.parse(obj);
  } catch (e) {
    return `The agent's JSON does not parse (${e instanceof Error ? e.message : String(e)}):\n${obj.slice(0, 400)}`;
  }
  if (!o || typeof o !== "object" || (!("name" in o) && !("kind" in o))) {
    return `The agent's object is not a program (no name or kind):\n${obj.slice(0, 400)}`;
  }
  const draft: Record<string, unknown> = {};
  for (const k of KEYS) if (k in o) draft[k] = o[k];
  return { draft: draft as Partial<ProgramDef>, text: obj };
}
