/**
 * Revisions (`crates/api/src/revisions.rs`, OUTSIDE_REVIEW_PLAN Phase 2): the
 * history of a workflow, agent profile or program. A change of definition is
 * saved with a reason and the version it was read at; a revert is a new revision.
 */
import { useQuery } from "@tanstack/react-query";
import { apiJson, apiSend, query } from "./client";

export type RevisionKind = "workflow" | "profile" | "program";

export interface RevisionRow {
  version: number;
  /** save, revert, seed, baseline, delete or outside. */
  op: string;
  author: string;
  reason: string | null;
  based_on: number | null;
  at: string | null;
  content_hash: string;
}

export interface RevisionHistory {
  kind: RevisionKind;
  name: string;
  exists: boolean;
  version: number;
  managed_by: string | null;
  seed_source: string | null;
  /** How live differs from the file; null when it does not. */
  drift: string | null;
  /** A history edited outside the save path; null when it is intact. */
  problem: string | null;
  revisions: RevisionRow[];
}

export interface RevisionDetail extends RevisionRow {
  definition: unknown;
  diff_from_previous: string | null;
}

export interface RevisionFile {
  version: number;
  path: string;
  new_file: boolean;
  matches_file: boolean;
  yaml: string;
}

const base = (kind: RevisionKind, name: string) => `/api/revisions/${kind}/${encodeURIComponent(name)}`;

export const revisionKeys = {
  history: (kind: RevisionKind, name: string) => ["revisions", kind, name] as const,
};

export function useRevisionHistory(kind: RevisionKind, name: string, enabled: boolean) {
  return useQuery({
    queryKey: revisionKeys.history(kind, name),
    queryFn: () => apiJson<RevisionHistory>(`${base(kind, name)}${query({ limit: 50 })}`),
    enabled,
    retry: false,
  });
}

export const fetchRevision = (kind: RevisionKind, name: string, version: number) =>
  apiJson<RevisionDetail>(`${base(kind, name)}/${version}`);

export const fetchRevisionFile = (kind: RevisionKind, name: string) =>
  apiJson<RevisionFile>(`${base(kind, name)}/file`);

export const revertRevision = (kind: RevisionKind, name: string, version: number, reason: string, basedOn: number) =>
  apiSend<unknown>("POST", `${base(kind, name)}/revert`, { version, reason, based_on: basedOn });
