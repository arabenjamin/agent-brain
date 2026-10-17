/**
 * Programs (`clients/programs.rs`, PROGRAMS_PLAN.md): the brain's work
 * organized into departments, projects, investigations, cases and shared
 * services. They nest by `parent`; each owns its workflows (one program per
 * workflow). A status other than active pauses the program's workflows and
 * those of the programs under it, except its `keep_running`.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiRequest, apiSend, query } from "./client";
import { queryClient } from "./queries";
import type { Tone } from "../components/ui";

export const PROGRAM_KINDS = ["department", "project", "investigation", "case", "services"] as const;
export type ProgramKind = (typeof PROGRAM_KINDS)[number];
export type ProgramStatus = "active" | "paused" | "closed";

/** active: green · paused: yellow · closed (a cold case): muted. */
export function programTone(s: string): Tone {
  return s === "active" ? "green" : s === "paused" ? "yellow" : "muted";
}

/** Kinds that end with an answer: closing one needs a finding. */
export const QUESTION_KINDS: string[] = ["investigation", "case"];

/** The server's checks (services/programs.rs), mirrored for the form. */
export const PROGRAM_NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;
export const TEXT_MAX = 1500;

/** A program as a save body carries it. */
export interface ProgramDef {
  name: string;
  label?: string;
  kind: ProgramKind | string;
  parent?: string | null;
  status?: ProgramStatus;
  charter?: string;
  success?: string;
  topics?: string[];
  workflows?: string[];
  keep_running?: string[];
  agents?: string[];
  apis?: string[];
  review?: { every: string } | null;
  budget?: { monthly_usd: number } | null;
  finding?: string | null;
}

/** One row of the tree (`GET /api/programs`), depth-first. */
export interface ProgramRow {
  name: string;
  label: string;
  kind: string;
  status: ProgramStatus;
  parent: string | null;
  depth: number;
  workflows: number;
  /** Its own workflows (not its children's): who owns what, for the form. */
  workflow_names: string[];
  enabled_workflows: number;
  managed_by: "yaml" | "runtime";
  /** What is wrong or worth knowing; empty when nothing is. */
  issues: string[];
}

export interface ProgramTree {
  count: number;
  programs: ProgramRow[];
  /** Enabled workflows no program owns. */
  unassigned_enabled_workflows: string[];
}

/** One program in full (`GET /api/programs/{name}`). */
export interface ProgramDetail extends ProgramDef {
  label: string;
  status: ProgramStatus;
  managed_by: "yaml" | "runtime";
  plugin: string | null;
  /** The file it is seeded from; present ⇒ Reset can put it back. */
  source: string | null;
  status_changed_at: string | null;
  updated_at: string | null;
  children: string[];
  workflow_state: { name: string; enabled?: boolean; paused_reason?: string | null; missing?: boolean }[];
  open_todos: { id: string; title: string; status: string }[];
  issues: string[];
}

/** A `program_scorecard` row: the program and every program under it. */
export interface ProgramScore {
  program: string;
  workflows: number;
  enabled: number;
  runs: number;
  ok: number;
  failed: number;
  notes: number;
  reads: number;
  cited: number;
  /** null: no ledger rows for its runs (cost is recorded from 2026-10-17). */
  model_calls: number | null;
  tokens_in: number | null;
  cost_usd: number | null;
  budget_monthly_usd: number | null;
}

export interface ProgramSaved {
  program: ProgramDef;
  created: boolean;
  managed_by: "yaml" | "runtime";
  warnings: string[];
  /** Workflows paused or resumed by the save. */
  changes: string[];
}

export const programKeys = {
  all: ["programs"] as const,
  tree: ["programs", "tree"] as const,
  one: (name: string) => ["programs", "one", name] as const,
  scorecard: (days: number) => ["programs", "scorecard", days] as const,
};

export function usePrograms(pollMs = 60_000) {
  return useQuery({
    queryKey: programKeys.tree,
    queryFn: () => apiJson<ProgramTree>("/api/programs"),
    refetchInterval: pollMs,
  });
}

export function useProgram(name: string) {
  return useQuery({
    queryKey: programKeys.one(name),
    queryFn: () => apiJson<ProgramDetail>(`/api/programs/${encodeURIComponent(name)}`),
    retry: false,
  });
}

export function useProgramScorecard(days: number) {
  return useQuery({
    queryKey: programKeys.scorecard(days),
    queryFn: () => apiJson<{ rows: ProgramScore[]; sql_error?: string }>(`/api/programs/scorecard${query({ days })}`),
    staleTime: 5 * 60_000,
  });
}

async function refresh<T>(p: Promise<T>): Promise<T> {
  const r = await p;
  // A status change pauses or resumes workflows, so the Workflows lane too.
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: programKeys.all }),
    queryClient.invalidateQueries({ queryKey: ["work"] }),
  ]);
  return r;
}

const path = (name: string) => `/api/programs/${encodeURIComponent(name)}`;

export const createProgram = (def: ProgramDef) => refresh(apiSend<ProgramSaved>("POST", "/api/programs", def));
/** Keys left out keep their values; a key set to null goes back to its default. */
export const updateProgram = (name: string, patch: Partial<ProgramDef>) =>
  refresh(apiSend<ProgramSaved>("PUT", path(name), patch));
export const resetProgram = (name: string) => refresh(apiSend<ProgramSaved>("POST", `${path(name)}/reset`));
export const deleteProgram = (name: string) =>
  refresh(apiSend<{ deleted: string; changes: string[] }>("DELETE", path(name)));

/** What a save did, in one line for the user. */
export function savedNote(s: ProgramSaved): string {
  const parts = [s.created ? "Created." : "Saved."];
  if (s.changes.length) parts.push(s.changes.join("; ") + ".");
  if (s.warnings.length) parts.push(s.warnings.join(" "));
  return parts.join(" ");
}

/** `schemas/program.json`: what an agent drafting a program is given. */
export function useProgramSchema() {
  return useQuery({
    queryKey: ["programs", "schema"],
    queryFn: () => apiJson<Record<string, unknown>>("/api/programs/schema"),
    staleTime: Infinity,
    retry: false,
  });
}

export type ProgramCheck =
  | { ok: true; warnings: string[] }
  | { ok: false; refused: boolean; error: string };

/** Every check a save runs, nothing written (`POST /api/programs/check`). */
export async function checkProgram(program: Partial<ProgramDef>, create: boolean): Promise<ProgramCheck> {
  try {
    const res = await apiRequest("/api/programs/check", { method: "POST", body: JSON.stringify({ program, create }) });
    const data = (await res.json().catch(() => ({}))) as { warnings?: string[]; error?: string };
    if (res.ok) return { ok: true, warnings: data.warnings ?? [] };
    return { ok: false, refused: res.status === 400, error: data.error ?? `program check: HTTP ${res.status}` };
  } catch (e) {
    return { ok: false, refused: false, error: `program check could not run: ${e instanceof Error ? e.message : String(e)}` };
  }
}

/** An `agent_scorecard` row: one agent profile over the window. */
export interface AgentScore {
  agent: string;
  called_by_workflows: string[];
  relied_on_by: string[];
  /** Runs (recorded from 2026-10-17; null: none recorded). */
  runs: number | null;
  answered: number | null;
  tool_calls: number | null;
  tools_failed: number | null;
  avg_secs: number | null;
  model_calls: number | null;
  model_errors: number | null;
  tokens_in: number | null;
  cost_usd: number | null;
  /** Tool calls whose arguments did not fit the tool (from 2026-10-17). */
  arg_misfits: number | null;
}

/** The agents a program's work uses (its workflows call them or it relies on them). */
export function useProgramAgents(program: string, days = 30) {
  return useQuery({
    queryKey: ["programs", "agents", program, days],
    queryFn: () => apiJson<{ rows: AgentScore[]; sql_error?: string }>(`/api/programs/agents${query({ program, days })}`),
    staleTime: 5 * 60_000,
  });
}
