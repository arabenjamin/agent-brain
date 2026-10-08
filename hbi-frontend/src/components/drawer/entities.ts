/**
 * Opening and closing the entity drawer. The drawer is URL state
 * (`?note=<id>`), so these are navigations, callable from anywhere.
 */
import { navigate } from "../../router";

/** Route params that open the drawer, in priority order. */
export const DRAWER_KINDS = ["note", "entity", "topic", "task", "job", "schedule", "todo", "proposal"] as const;
export type DrawerKind = (typeof DRAWER_KINDS)[number];

export function openEntity(kind: DrawerKind, id: string): void {
  const clear = Object.fromEntries(DRAWER_KINDS.map((k) => [k, null]));
  navigate({ params: { ...clear, [kind]: id } });
}

export function closeEntity(): void {
  navigate({ params: Object.fromEntries(DRAWER_KINDS.map((k) => [k, null])) });
}

/** The entity the URL has open, if any. */
export function drawerTarget(params: Record<string, string>): { kind: DrawerKind; id: string } | null {
  for (const kind of DRAWER_KINDS) {
    const id = params[kind];
    if (id) return { kind, id };
  }
  return null;
}
