/** A code proposal the brain wrote: read it, then dismiss it with a reason. */
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { callTool, callToolStrict } from "../../api/mcp";
import { keys, queryClient, useProposals } from "../../api/queries";
import { formatLocal } from "../../lib/time";
import { Drawer, EmptyState, Pill } from "../ui";
import { closeEntity } from "./entities";
import { EntityLink, LoadState, Section } from "./parts";

const SEVERITY_TONE = { high: "red", medium: "yellow", low: "green" } as const;

export default function ProposalView({ filename }: { filename: string }) {
  const list = useProposals();
  const meta = list.data?.find((p) => p.filename === filename);
  const body = useQuery({
    queryKey: ["proposal", filename],
    queryFn: () => callTool("read_proposal", { filename }),
    retry: false,
  });
  const [reason, setReason] = useState<"applied" | "rejected" | "obsolete">("rejected");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const dismiss = async () => {
    setBusy(true);
    setError(null);
    try {
      await callToolStrict("dismiss_proposal", { filename, reason });
      await queryClient.invalidateQueries({ queryKey: keys.proposals });
      closeEntity();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Drawer
      title={<><span>Proposal</span>{meta?.severity && <Pill tone={SEVERITY_TONE[meta.severity as keyof typeof SEVERITY_TONE] ?? "muted"}>{meta.severity}</Pill>}</>}
      onClose={closeEntity}
      wide
    >
      <h3 className="dw-title">{meta?.title ?? filename}</h3>
      <div className="dw-dim">
        <code>{filename}</code>{meta?.created && <> · {formatLocal(meta.created)}</>}
        {meta?.task_id && <> · from task <EntityLink kind="task" id={meta.task_id}>{meta.task_id.slice(0, 8)}</EntityLink></>}
      </div>
      {list.data && !meta && (
        <EmptyState tone="yellow" title="Not in the open proposals"
          checked="list_proposals does not list this file; it may already have been dismissed." />
      )}
      <Section title="Proposal">
        {body.data
          ? <div className="entity-content markdown-body"><ReactMarkdown remarkPlugins={[remarkGfm]}>{body.data}</ReactMarkdown></div>
          : <LoadState what="proposal" id={filename} error={body.error} onRetry={() => body.refetch()} />}
      </Section>
      <Section title="Dismiss">
        <div className="settings-actions">
          <select className="ui-input auto" value={reason} onChange={(e) => setReason(e.target.value as typeof reason)}>
            <option value="applied">applied — the change was made</option>
            <option value="rejected">rejected — not doing it</option>
            <option value="obsolete">obsolete — no longer relevant</option>
          </select>
          <button type="button" className="btn" disabled={busy} onClick={dismiss}>{busy ? "Dismissing…" : "Dismiss"}</button>
        </div>
        {error && <div className="tone-text-red">{error}</div>}
      </Section>
    </Drawer>
  );
}
