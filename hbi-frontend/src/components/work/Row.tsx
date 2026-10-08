import type { ReactNode } from "react";
import { openEntity, type DrawerKind } from "../drawer/entities";

/** One line in a Work lane; opens its entity in the drawer. */
export default function Row({
  kind,
  id,
  lead,
  children,
  meta,
  active,
}: {
  kind: DrawerKind;
  id: string;
  lead?: ReactNode;
  children: ReactNode;
  meta?: ReactNode;
  active?: boolean;
}) {
  return (
    <button type="button" className={`wk-row${active ? " active" : ""}`} onClick={() => openEntity(kind, id)}>
      {lead && <span className="wk-row-lead">{lead}</span>}
      <span className="wk-row-main">{children}</span>
      {meta && <span className="wk-row-meta">{meta}</span>}
    </button>
  );
}
