/**
 * The history of a workflow, agent or program: who changed what and why, how
 * live differs from its repo file, putting an old version back, and the file
 * text for the owner to review and commit (the brain never writes a repo file).
 */
import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  fetchRevision, fetchRevisionFile, revertRevision, revisionKeys, useRevisionHistory,
  type RevisionDetail, type RevisionFile, type RevisionKind, type RevisionRow,
} from "../../api/revisions";
import { relTime } from "../../lib/time";
import { Section } from "./parts";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const preStyle = { whiteSpace: "pre-wrap", overflowX: "auto", fontSize: 12, margin: 0 } as const;

export function RevisionHistory({ kind, name, onReverted }: { kind: RevisionKind; name: string; onReverted?: () => void }) {
  const [open, setOpen] = useState(false);
  const history = useRevisionHistory(kind, name, open);
  const client = useQueryClient();
  const [shown, setShown] = useState<RevisionDetail | null>(null);
  const [file, setFile] = useState<RevisionFile | null>(null);
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const h = history.data;

  const show = async (r: RevisionRow) => {
    setError(null);
    if (shown?.version === r.version) return setShown(null);
    try {
      setShown(await fetchRevision(kind, name, r.version));
    } catch (e) {
      setError(message(e));
    }
  };

  const putBack = async () => {
    if (!shown || !h) return;
    setBusy(true);
    setError(null);
    try {
      await revertRevision(kind, name, shown.version, reason.trim(), h.version);
      setShown(null);
      setReason("");
      await client.invalidateQueries({ queryKey: revisionKeys.history(kind, name) });
      onReverted?.();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  };

  const showFile = async () => {
    setError(null);
    setCopied(false);
    try {
      setFile(await fetchRevisionFile(kind, name));
    } catch (e) {
      setError(message(e));
    }
  };

  const copy = async () => {
    if (!file) return;
    // Only on https or localhost; the text is on screen to select otherwise.
    try {
      await navigator.clipboard.writeText(file.yaml);
      setCopied(true);
    } catch {
      setError("The browser won't copy here: select the text below, or use the script.");
    }
  };

  return (
    <Section
      title="History"
      aside={<button type="button" className="btn-ghost" onClick={() => setOpen(!open)}>{open ? "Hide" : "Show"}</button>}
    >
      {open && history.error && <div className="tone-text-red">{message(history.error)}</div>}
      {open && h && (
        <>
          {h.problem && <div className="tone-text-red">This history was edited outside the save path: {h.problem}</div>}
          <div className="dw-dim">
            At version {h.version}.{" "}
            {h.drift === null
              ? <>Live matches {h.seed_source ? <code>{h.seed_source}</code> : "its file"}.</>
              : h.seed_source
                ? <>Live differs from <code>{h.seed_source}</code>.</>
                : <>No repo file: it exists only in the graph.</>}
          </div>
          {h.drift !== null && h.seed_source && <pre style={preStyle}>{h.drift}</pre>}
          {h.drift !== null && (
            <div className="item-actions">
              <button type="button" className="btn-ghost" onClick={showFile}>Show as a repo file</button>
            </div>
          )}
          {file && (
            <div className="dw-note">
              <div className="dw-dim">
                To keep this in the repo: put it in <code>{file.path}</code>
                {file.new_file && " (a suggestion: no file exists yet)"}, or run{" "}
                <code>scripts/adopt_revision.sh {kind} {name}</code>, then review the diff and commit. Comments in the
                old file aren't carried over. After that the file owns it again on the next start.
              </div>
              <div className="item-actions">
                <button type="button" className="btn-ghost" onClick={copy}>{copied ? "Copied" : "Copy"}</button>
                <button type="button" className="btn-ghost" onClick={() => setFile(null)}>Close</button>
              </div>
              <pre style={preStyle}>{file.yaml}</pre>
            </div>
          )}
          <ul className="dw-list">
            {h.revisions.map((r) => (
              <li key={r.version}>
                <button type="button" className="dw-link" onClick={() => show(r)}>v{r.version}</button>
                <span>{r.reason ?? <span className="dw-dim">no reason given</span>}</span>
                <span className="dw-dim">{r.op} · {r.author}{r.at ? ` · ${relTime(r.at)}` : ""}</span>
              </li>
            ))}
          </ul>
          {shown && (
            <div className="dw-note">
              <div className="dw-dim">
                v{shown.version}{shown.diff_from_previous === null ? " is the first version." : ", what it changed:"}
              </div>
              {shown.diff_from_previous && <pre style={preStyle}>{shown.diff_from_previous}</pre>}
              {shown.version !== h.version && shown.op !== "delete" && (
                <>
                  <input
                    className="ui-input"
                    value={reason}
                    onChange={(e) => setReason(e.target.value)}
                    placeholder={`Why go back to v${shown.version}?`}
                  />
                  <div className="item-actions">
                    <button type="button" className="btn" disabled={busy || !reason.trim()} onClick={putBack}>
                      {busy ? "Putting back…" : `Put v${shown.version} back`}
                    </button>
                  </div>
                </>
              )}
            </div>
          )}
        </>
      )}
      {error && <div className="tone-text-red" style={{ fontSize: 12, whiteSpace: "pre-wrap" }}>{error}</div>}
    </Section>
  );
}
