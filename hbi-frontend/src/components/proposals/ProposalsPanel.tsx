import React, { useCallback, useEffect, useState } from "react";
import { callTool } from "../../api/mcp";

// ── Types ─────────────────────────────────────────────────────────────────────

interface Proposal {
  filename: string;
  title: string;
  severity: "low" | "medium" | "high" | string;
  task_id: string;
  created: string;
  applied: boolean;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const SEVERITY_COLOR: Record<string, string> = {
  high:    "var(--red)",
  medium:  "var(--yellow)",
  low:     "var(--green)",
  unknown: "var(--text-dim)",
};

function SeverityBadge({ severity }: { severity: string }) {
  return (
    <span style={{
      fontSize: 10,
      fontWeight: 700,
      textTransform: "uppercase",
      letterSpacing: "0.08em",
      color: SEVERITY_COLOR[severity] ?? "var(--text-dim)",
      background: `${SEVERITY_COLOR[severity] ?? "var(--text-dim)"}22`,
      padding: "2px 7px",
      borderRadius: 10,
      border: `1px solid ${SEVERITY_COLOR[severity] ?? "var(--text-dim)"}55`,
    }}>
      {severity}
    </span>
  );
}

function formatDate(iso: string) {
  if (!iso) return "";
  try {
    return new Date(iso).toLocaleString(undefined, {
      month: "short", day: "numeric",
      hour: "2-digit", minute: "2-digit",
    });
  } catch {
    return iso;
  }
}

// ── Component ─────────────────────────────────────────────────────────────────

export default function ProposalsPanel() {
  const [proposals, setProposals]     = useState<Proposal[]>([]);
  const [selected, setSelected]       = useState<Proposal | null>(null);
  const [content, setContent]         = useState<string>("");
  const [loading, setLoading]         = useState(false);
  const [loadingContent, setLoadingContent] = useState(false);
  const [error, setError]             = useState<string | null>(null);
  const [showApplied, setShowApplied] = useState(false);
  const [dismissing, setDismissing]   = useState(false);
  const [dismissReason, setDismissReason] = useState<"applied" | "rejected" | "obsolete">("applied");
  const [confirmDismiss, setConfirmDismiss] = useState(false);

  const load = useCallback(async (includeApplied: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const raw = await callTool("list_proposals", { include_applied: includeApplied });
      const data = JSON.parse(raw);
      setProposals(data.proposals ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(showApplied); }, [load, showApplied]);

  const select = useCallback(async (p: Proposal) => {
    setSelected(p);
    setContent("");
    setConfirmDismiss(false);
    setLoadingContent(true);
    try {
      const raw = await callTool("read_proposal", { filename: p.filename });
      setContent(raw);
    } catch (e) {
      setContent(`Error loading proposal: ${e}`);
    } finally {
      setLoadingContent(false);
    }
  }, []);

  const dismiss = useCallback(async () => {
    if (!selected) return;
    setDismissing(true);
    try {
      await callTool("dismiss_proposal", { filename: selected.filename, reason: dismissReason });
      setSelected(null);
      setContent("");
      setConfirmDismiss(false);
      await load(showApplied);
    } catch (e) {
      setError(String(e));
    } finally {
      setDismissing(false);
    }
  }, [selected, dismissReason, load, showApplied]);

  const pending  = proposals.filter((p) => !p.applied);
  const applied  = proposals.filter((p) => p.applied);

  return (
    <div className="panel" style={{ flexDirection: "row" }}>
      {/* ── List ── */}
      <div style={{
        width: 320,
        minWidth: 260,
        borderRight: "1px solid var(--border)",
        display: "flex",
        flexDirection: "column",
        overflow: "hidden",
      }}>
        <div className="panel-header" style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span>PROPOSALS</span>
          {pending.length > 0 && (
            <span className="badge">{pending.length}</span>
          )}
          <label style={{ marginLeft: "auto", fontSize: 11, color: "var(--text-dim)", display: "flex", alignItems: "center", gap: 4, cursor: "pointer", fontWeight: 400 }}>
            <input
              type="checkbox"
              checked={showApplied}
              onChange={(e) => setShowApplied(e.target.checked)}
              style={{ cursor: "pointer" }}
            />
            show applied
          </label>
          <button
            className="btn-icon"
            onClick={() => load(showApplied)}
            disabled={loading}
            title="Refresh"
            style={{ marginLeft: 4 }}
          >
            ↻
          </button>
        </div>

        <div style={{ flex: 1, overflowY: "auto", padding: "8px" }}>
          {loading && <div style={{ color: "var(--text-dim)", padding: 12, textAlign: "center" }}>Loading…</div>}
          {error   && <div style={{ color: "var(--red)", padding: 12, fontSize: 12 }}>{error}</div>}

          {!loading && pending.length === 0 && applied.length === 0 && (
            <div style={{ color: "var(--text-dim)", padding: 16, textAlign: "center", fontSize: 13 }}>
              No proposals yet.
              <br />
              <span style={{ fontSize: 11, marginTop: 6, display: "block" }}>
                The brain will write proposals here when it diagnoses issues.
              </span>
            </div>
          )}

          {pending.length > 0 && (
            <>
              <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: "0.08em", color: "var(--text-dim)", padding: "4px 6px 6px" }}>
                PENDING
              </div>
              {pending.map((p) => (
                <ProposalRow
                  key={p.filename}
                  proposal={p}
                  selected={selected?.filename === p.filename}
                  onClick={() => select(p)}
                />
              ))}
            </>
          )}

          {showApplied && applied.length > 0 && (
            <>
              <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: "0.08em", color: "var(--text-dim)", padding: "12px 6px 6px" }}>
                APPLIED / DISMISSED
              </div>
              {applied.map((p) => (
                <ProposalRow
                  key={p.filename}
                  proposal={p}
                  selected={selected?.filename === p.filename}
                  onClick={() => select(p)}
                />
              ))}
            </>
          )}
        </div>
      </div>

      {/* ── Detail ── */}
      <div style={{ flex: 1, display: "flex", flexDirection: "column", overflow: "hidden" }}>
        {!selected ? (
          <div style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", color: "var(--text-dim)", fontSize: 13 }}>
            Select a proposal to review
          </div>
        ) : (
          <>
            <div className="panel-header" style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <SeverityBadge severity={selected.severity} />
              <span style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                {selected.title}
              </span>
              {!selected.applied && (
                <div style={{ display: "flex", alignItems: "center", gap: 6, marginLeft: "auto" }}>
                  {confirmDismiss ? (
                    <>
                      <select
                        value={dismissReason}
                        onChange={(e) => setDismissReason(e.target.value as typeof dismissReason)}
                        style={{ fontSize: 11, background: "var(--bg-input)", color: "var(--text)", border: "1px solid var(--border)", borderRadius: 4, padding: "2px 6px" }}
                      >
                        <option value="applied">applied</option>
                        <option value="rejected">rejected</option>
                        <option value="obsolete">obsolete</option>
                      </select>
                      <button
                        className="btn-sm btn-danger"
                        onClick={dismiss}
                        disabled={dismissing}
                      >
                        {dismissing ? "…" : "Confirm"}
                      </button>
                      <button
                        className="btn-sm"
                        onClick={() => setConfirmDismiss(false)}
                      >
                        Cancel
                      </button>
                    </>
                  ) : (
                    <button
                      className="btn-sm"
                      onClick={() => setConfirmDismiss(true)}
                    >
                      Dismiss
                    </button>
                  )}
                </div>
              )}
            </div>

            <div style={{ padding: "8px 16px 4px", fontSize: 11, color: "var(--text-dim)", borderBottom: "1px solid var(--border)", display: "flex", gap: 16 }}>
              <span>Task: <code style={{ color: "var(--text)" }}>{selected.task_id || "—"}</code></span>
              <span>{formatDate(selected.created)}</span>
              {selected.applied && <span style={{ color: "var(--green)" }}>✓ applied</span>}
            </div>

            <div style={{ flex: 1, overflowY: "auto", padding: "16px" }}>
              {loadingContent ? (
                <div style={{ color: "var(--text-dim)" }}>Loading…</div>
              ) : (
                <MarkdownContent content={content} />
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────────

function ProposalRow({ proposal, selected, onClick }: {
  proposal: Proposal;
  selected: boolean;
  onClick: () => void;
}) {
  return (
    <div
      onClick={onClick}
      style={{
        padding: "9px 10px",
        marginBottom: 4,
        borderRadius: 6,
        cursor: "pointer",
        background: selected ? "var(--accent-glow)" : "var(--bg-card)",
        border: `1px solid ${selected ? "var(--accent)" : "var(--border)"}`,
        opacity: proposal.applied ? 0.6 : 1,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 6, marginBottom: 4 }}>
        <SeverityBadge severity={proposal.severity} />
        {proposal.applied && (
          <span style={{ fontSize: 10, color: "var(--green)" }}>✓</span>
        )}
      </div>
      <div style={{ fontSize: 12, fontWeight: 500, lineHeight: 1.3, marginBottom: 3 }}>
        {proposal.title}
      </div>
      <div style={{ fontSize: 10, color: "var(--text-dim)" }}>
        {formatDate(proposal.created)}
      </div>
    </div>
  );
}

function MarkdownContent({ content }: { content: string }) {
  return (
    <div style={{ fontFamily: "inherit", fontSize: 13, lineHeight: 1.6, color: "var(--text)" }}>
      {content.split("\n").map((line, i) => {
        if (line.startsWith("# "))   return <h2 key={i} style={{ fontSize: 15, fontWeight: 700, marginBottom: 8, marginTop: i === 0 ? 0 : 20, color: "var(--text)" }}>{line.slice(2)}</h2>;
        if (line.startsWith("## "))  return <h3 key={i} style={{ fontSize: 13, fontWeight: 700, marginBottom: 6, marginTop: 16, color: "var(--accent)" }}>{line.slice(3)}</h3>;
        if (line.startsWith("### ")) return <h4 key={i} style={{ fontSize: 12, fontWeight: 700, marginBottom: 4, marginTop: 12, color: "var(--text-dim)" }}>{line.slice(4)}</h4>;
        if (line.startsWith("```"))  return null; // handled below
        if (line.startsWith("- **")) {
          const match = line.match(/- \*\*(.+?)\*\*[:\s]+(.+)/);
          if (match) return (
            <div key={i} style={{ marginBottom: 4 }}>
              <span style={{ color: "var(--text-dim)", fontWeight: 600 }}>{match[1]}: </span>
              <span>{match[2]}</span>
            </div>
          );
        }
        if (line.startsWith("---")) return <hr key={i} style={{ border: "none", borderTop: "1px solid var(--border)", margin: "16px 0" }} />;
        if (line.trim() === "") return <div key={i} style={{ height: 8 }} />;
        return <div key={i}>{renderInline(line)}</div>;
      })}
      {/* Code blocks */}
      {renderCodeBlocks(content)}
    </div>
  );
}

function renderInline(line: string) {
  const parts = line.split(/(`[^`]+`)/g);
  return (
    <>
      {parts.map((part, i) =>
        part.startsWith("`") && part.endsWith("`")
          ? <code key={i} style={{ background: "var(--bg-card)", padding: "1px 5px", borderRadius: 3, fontSize: 12, fontFamily: "monospace", color: "var(--accent)" }}>{part.slice(1, -1)}</code>
          : part
      )}
    </>
  );
}

function renderCodeBlocks(content: string) {
  const blocks: React.ReactElement[] = [];
  const re = /```(?:\w+)?\n([\s\S]*?)```/g;
  let match;
  let idx = 0;
  while ((match = re.exec(content)) !== null) {
    blocks.push(
      <pre key={idx++} style={{
        background: "var(--bg-card)",
        border: "1px solid var(--border)",
        borderRadius: 6,
        padding: "12px 14px",
        overflowX: "auto",
        fontSize: 12,
        fontFamily: "monospace",
        color: "var(--text)",
        margin: "8px 0",
      }}>
        <code>{match[1]}</code>
      </pre>
    );
  }
  return blocks;
}
