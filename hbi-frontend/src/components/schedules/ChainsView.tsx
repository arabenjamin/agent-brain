import { useCallback, useEffect, useRef, useState } from "react";
import { apiRequest } from "../../api/client";
import { fmtDatetime, jsonToStepDrafts, makeDefaultStep, stepDraftsToJson, type SchedulerChain, type StepDraft } from "./model";
import StepCard from "./StepCard";
import { useMergedToolGroups } from "./useToolGroups";

// ─── ChainsView ───────────────────────────────────────────────────────────────

export default function ChainsView() {
  const [chains, setChains] = useState<SchedulerChain[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editChain, setEditChain] = useState<SchedulerChain | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await apiRequest(`/api/scheduler/chains`);
      if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
      const data = await res.json();
      setChains(data.chains ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  const handleFormClose = () => {
    setShowForm(false);
    setEditChain(null);
  };

  const handleFormSaved = () => {
    handleFormClose();
    refresh();
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", flex: 1, padding: "16px", gap: "12px", overflow: "hidden" }}>
      {/* Header */}
      <div style={{ display: "flex", alignItems: "center", gap: "12px", flexWrap: "wrap" }}>
        <h2 style={{ margin: 0, fontSize: "1.1rem" }}>Routing Chains</h2>
        <span style={{ fontSize: "0.8rem", opacity: 0.55 }}>
          Goal → chain mapping. Lower priority = matched first.
        </span>
        <div style={{ marginLeft: "auto", display: "flex", gap: "8px" }}>
          <button className="sidebar-btn" onClick={refresh} disabled={loading}>
            {loading ? "..." : "Refresh"}
          </button>
          <button className="sidebar-btn active" onClick={() => { setEditChain(null); setShowForm(true); }}>
            + New Chain
          </button>
        </div>
      </div>

      {error && (
        <div style={{ color: "var(--error, #f87171)", fontSize: "0.85rem" }}>{error}</div>
      )}

      <div style={{ flex: 1, overflowY: "auto", display: "flex", flexDirection: "column", gap: "8px" }}>
        {chains.length === 0 && !loading && (
          <div style={{ opacity: 0.5, fontSize: "0.9rem" }}>
            <p>No chains found. Run <code>cargo run -- init-db</code> to seed from <code>chains/</code>, or create one here.</p>
          </div>
        )}
        {chains.map((chain) => (
          <ChainRow
            key={chain.id}
            chain={chain}
            onEdit={() => { setEditChain(chain); setShowForm(true); }}
          />
        ))}
      </div>

      {showForm && (
        <ChainForm
          existing={editChain}
          onSaved={handleFormSaved}
          onCancel={handleFormClose}
        />
      )}
    </div>
  );
}

// ─── ChainRow ─────────────────────────────────────────────────────────────────

function ChainRow({
  chain,
  onEdit,
}: {
  chain: SchedulerChain;
  onEdit: () => void;
}) {
  const [expanded, setExpanded] = useState(false);

  const allPatterns = [
    chain.pattern ? chain.pattern : null,
    ...chain.patterns,
  ].filter(Boolean) as string[];

  return (
    <div style={{
      background: "var(--surface, #1e1e2e)",
      border: "1px solid var(--border, #313244)",
      borderLeft: `3px solid ${chain.no_evaluator ? "#fbbf24" : "var(--accent, #89b4fa)"}`,
      borderRadius: "8px",
      padding: "10px 14px",
    }}>
      <div style={{ display: "flex", alignItems: "flex-start", gap: "12px" }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          {/* Name + priority badge */}
          <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
            <span style={{ fontWeight: 600, fontSize: "0.95rem" }}>{chain.name || "(unnamed)"}</span>
            <span style={{
              fontSize: "0.7rem", padding: "1px 6px", borderRadius: "4px",
              border: "1px solid #cba6f7", color: "#cba6f7",
            }}>
              priority {chain.priority}
            </span>
            {chain.no_evaluator && (
              <span style={{
                fontSize: "0.7rem", padding: "1px 6px", borderRadius: "4px",
                border: "1px solid #fbbf24", color: "#fbbf24",
              }}>
                no evaluator
              </span>
            )}
            {chain.evaluation_rubric && (
              <span style={{
                fontSize: "0.7rem", padding: "1px 6px", borderRadius: "4px",
                border: "1px solid #4ade80", color: "#4ade80",
              }}>
                custom rubric
              </span>
            )}
          </div>

          {/* Description */}
          {chain.description && (
            <p style={{ margin: "4px 0 0", fontSize: "0.82rem", opacity: 0.7, wordBreak: "break-word" }}>
              {chain.description}
            </p>
          )}

          {/* Patterns */}
          {allPatterns.length > 0 && (
            <div style={{ display: "flex", flexWrap: "wrap", gap: "4px", marginTop: "6px" }}>
              {allPatterns.map((p, i) => (
                <code key={i} style={{
                  fontSize: "0.72rem", padding: "1px 6px", borderRadius: "4px",
                  background: "var(--surface2, #313244)",
                  border: "1px solid var(--border, #45475a)",
                  color: p === "" ? "#a6adc8" : "inherit",
                }}>
                  {p === "" ? "(empty — default)" : `"${p}"`}
                </code>
              ))}
            </div>
          )}

          {/* Footer */}
          <div style={{ display: "flex", gap: "14px", marginTop: "6px", fontSize: "0.78rem", opacity: 0.65, flexWrap: "wrap" }}>
            {chain.updated_at && <span>Updated: {fmtDatetime(chain.updated_at)}</span>}
            <button
              onClick={() => setExpanded((v) => !v)}
              style={{ background: "none", border: "none", padding: 0, cursor: "pointer", color: "var(--accent, #89b4fa)", fontSize: "0.78rem" }}
            >
              {chain.step_count} step{chain.step_count !== 1 ? "s" : ""} {expanded ? "▲" : "▼"}
            </button>
          </div>

          {expanded && (
            <pre style={{
              marginTop: "8px", fontSize: "0.75rem", background: "var(--surface2, #313244)",
              borderRadius: "6px", padding: "8px", overflow: "auto", maxHeight: "240px",
              whiteSpace: "pre-wrap", wordBreak: "break-all",
            }}>
              {(() => { try { return JSON.stringify(JSON.parse(chain.steps), null, 2); } catch { return chain.steps; } })()}
            </pre>
          )}
        </div>

        {/* Actions */}
        <div style={{ display: "flex", gap: "6px", flexShrink: 0 }}>
          <button
            onClick={onEdit}
            style={{ background: "none", border: "1px solid var(--border, #313244)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.78rem", color: "inherit" }}
          >
            Edit
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── ChainForm ────────────────────────────────────────────────────────────────

function ChainForm({
  existing,
  onSaved,
  onCancel,
}: {
  existing: SchedulerChain | null;
  onSaved: () => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [pattern, setPattern] = useState(existing?.pattern ?? "");
  const [patternsText, setPatternsText] = useState(
    (existing?.patterns ?? []).join("\n")
  );
  const [priority, setPriority] = useState(existing?.priority ?? 100);
  const [description, setDescription] = useState(existing?.description ?? "");
  const [noEvaluator, setNoEvaluator] = useState(existing?.no_evaluator ?? false);
  const [rubric, setRubric] = useState(existing?.evaluation_rubric ?? "");
  const toolGroups = useMergedToolGroups();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showRaw, setShowRaw] = useState(false);
  const [rawJson, setRawJson] = useState("");
  const [rawJsonError, setRawJsonError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const [steps, setSteps] = useState<StepDraft[]>(() => {
    if (existing) {
      const parsed = jsonToStepDrafts(existing.steps);
      if (parsed && parsed.length > 0) return parsed;
    }
    return [makeDefaultStep()];
  });

  useEffect(() => {
    if (showRaw) {
      setRawJson(stepDraftsToJson(steps));
      setRawJsonError(null);
    }
  }, [showRaw]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => { inputRef.current?.focus(); }, []);

  const applyRawJson = () => {
    const parsed = jsonToStepDrafts(rawJson);
    if (!parsed) {
      setRawJsonError("Invalid JSON or not an array of steps");
      return false;
    }
    setSteps(parsed);
    setRawJsonError(null);
    return true;
  };

  const handleToggleRaw = () => {
    if (showRaw) {
      if (!applyRawJson()) return;
      setShowRaw(false);
    } else {
      setShowRaw(true);
    }
  };

  const updateStep = (i: number, updated: StepDraft) => {
    setSteps(steps.map((s, idx) => idx === i ? updated : s));
  };

  const removeStep = (i: number) => {
    setSteps(steps.filter((_, idx) => idx !== i));
  };

  const addStep = () => {
    setSteps([...steps, makeDefaultStep()]);
  };

  const moveStep = (i: number, dir: -1 | 1) => {
    const j = i + dir;
    if (j < 0 || j >= steps.length) return;
    const next = [...steps];
    [next[i], next[j]] = [next[j], next[i]];
    setSteps(next);
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;
    if (showRaw && !applyRawJson()) return;

    const stepsJson = stepDraftsToJson(steps);
    const patternsArr = patternsText
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);

    setSaving(true);
    setError(null);
    try {
      const res = await apiRequest(`/mcp`, {
        method: "POST",
        body: JSON.stringify({
          jsonrpc: "2.0", id: 1, method: "tools/call",
          params: {
            name: "manage_chain",
            arguments: {
              action: "define",
              name: name.trim(),
              pattern: pattern,
              patterns: patternsArr,
              priority,
              description: description.trim() || null,
              no_evaluator: noEvaluator,
              evaluation_rubric: rubric.trim() || null,
              steps: stepsJson,
            },
          },
        }),
      });
      if (!res.ok) throw new Error(`${res.status}`);
      const data = await res.json();
      if (data?.result?.isError) {
        const msg = data.result.content?.[0]?.text ?? "Unknown error";
        throw new Error(msg);
      }
      onSaved();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const overlayStyle: React.CSSProperties = {
    position: "fixed", inset: 0, background: "rgba(0,0,0,0.6)", display: "flex",
    alignItems: "center", justifyContent: "center", zIndex: 50,
  };

  const cardStyle: React.CSSProperties = {
    background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #313244)",
    borderRadius: "10px", padding: "20px", width: "min(680px, 96vw)", maxHeight: "92vh",
    display: "flex", flexDirection: "column", gap: "12px", overflowY: "auto",
  };

  const labelStyle: React.CSSProperties = {
    display: "flex", flexDirection: "column", gap: "4px", fontSize: "0.85rem",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)", border: "1px solid var(--border, #45475a)",
    borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.9rem",
  };

  return (
    <div style={overlayStyle} onClick={(e) => e.target === e.currentTarget && onCancel()}>
      <div style={cardStyle}>
        <h3 style={{ margin: 0, fontSize: "1rem" }}>
          {existing ? `Edit Chain: ${existing.name}` : "New Routing Chain"}
        </h3>

        <form onSubmit={handleSubmit} style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          {/* Name */}
          <label style={labelStyle}>
            Name *
            <input
              ref={inputRef}
              style={inputStyle}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. meta-learn"
              required
              spellCheck={false}
            />
          </label>

          {/* Pattern + Priority row */}
          <div style={{ display: "flex", gap: "10px", flexWrap: "wrap" }}>
            <label style={{ ...labelStyle, flex: 2 }}>
              Primary pattern
              <input
                style={inputStyle}
                value={pattern}
                onChange={(e) => setPattern(e.target.value)}
                placeholder='e.g. "meta-learn:" — leave empty for catch-all default'
                spellCheck={false}
              />
            </label>
            <label style={{ ...labelStyle, flex: "0 0 100px" }}>
              Priority
              <input
                type="number"
                min={1}
                max={9999}
                style={{ ...inputStyle, width: "100%" }}
                value={priority}
                onChange={(e) => setPriority(Number(e.target.value))}
              />
            </label>
          </div>

          {/* Extra patterns */}
          <label style={labelStyle}>
            <span>
              Extra patterns{" "}
              <span style={{ opacity: 0.5, fontSize: "0.8rem" }}>— one per line, OR-matched</span>
            </span>
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: "56px", fontFamily: "monospace", fontSize: "0.85rem" }}
              value={patternsText}
              onChange={(e) => setPatternsText(e.target.value)}
              placeholder={"hypothesize root cause\nanalyze failure"}
              spellCheck={false}
            />
          </label>

          {/* Description */}
          <label style={labelStyle}>
            Description
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: "56px" }}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="What this chain does…"
            />
          </label>

          {/* Flags row */}
          <div style={{ display: "flex", gap: "20px", flexWrap: "wrap" }}>
            <label style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "0.85rem", cursor: "pointer" }}>
              <input
                type="checkbox"
                checked={noEvaluator}
                onChange={(e) => setNoEvaluator(e.target.checked)}
                style={{ accentColor: "var(--accent, #89b4fa)" }}
              />
              No evaluator
              <span style={{ opacity: 0.5, fontSize: "0.78rem" }}>(skip auto-appended score step)</span>
            </label>
          </div>

          {/* Evaluation rubric */}
          {!noEvaluator && (
            <label style={labelStyle}>
              <span>
                Evaluation rubric{" "}
                <span style={{ opacity: 0.5, fontSize: "0.8rem" }}>— overrides task success_criteria</span>
              </span>
              <textarea
                style={{ ...inputStyle, resize: "vertical", minHeight: "56px" }}
                value={rubric}
                onChange={(e) => setRubric(e.target.value)}
                placeholder="Leave empty to use task success_criteria"
              />
            </label>
          )}

          {/* Steps */}
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span style={{ fontSize: "0.85rem", fontWeight: 600 }}>
              Steps ({steps.length})
            </span>
            <span style={{ fontSize: "0.75rem", opacity: 0.5 }}>
              — vars: <code style={{ fontSize: "0.73rem" }}>{"{{goal}}"}</code>{" "}
              <code style={{ fontSize: "0.73rem" }}>{"{{task_id}}"}</code>{" "}
              <code style={{ fontSize: "0.73rem" }}>{"{{date}}"}</code>{" "}
              <code style={{ fontSize: "0.73rem" }}>{"{{file_slug}}"}</code>
            </span>
            <button
              type="button"
              onClick={handleToggleRaw}
              style={{ marginLeft: "auto", background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.75rem", color: showRaw ? "var(--accent, #89b4fa)" : "inherit" }}
            >
              {showRaw ? "← Builder" : "Raw JSON"}
            </button>
          </div>

          {showRaw && (
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              <textarea
                style={{ ...inputStyle, fontFamily: "monospace", resize: "vertical", minHeight: "180px", fontSize: "0.82rem" }}
                value={rawJson}
                onChange={(e) => { setRawJson(e.target.value); setRawJsonError(null); }}
                spellCheck={false}
              />
              {rawJsonError && (
                <span style={{ color: "#f87171", fontSize: "0.78rem" }}>{rawJsonError}</span>
              )}
            </div>
          )}

          {!showRaw && (
            <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              {steps.length === 0 && (
                <p style={{ opacity: 0.45, fontSize: "0.85rem", margin: 0 }}>No steps yet.</p>
              )}
              {steps.map((step, i) => (
                <StepCard
                  key={i}
                  step={step}
                  index={i}
                  total={steps.length}
                  onChange={(updated) => updateStep(i, updated)}
                  onRemove={() => removeStep(i)}
                  onMoveUp={() => moveStep(i, -1)}
                  onMoveDown={() => moveStep(i, 1)}
                  inputStyle={inputStyle}
                  toolGroups={toolGroups}
                />
              ))}
              <button
                type="button"
                onClick={addStep}
                style={{ alignSelf: "flex-start", background: "none", border: "1px dashed var(--accent, #89b4fa)", borderRadius: "6px", padding: "6px 16px", cursor: "pointer", fontSize: "0.85rem", color: "var(--accent, #89b4fa)" }}
              >
                + Add Step
              </button>
            </div>
          )}

          {error && <p style={{ color: "#f87171", margin: 0, fontSize: "0.83rem" }}>{error}</p>}

          <div style={{ display: "flex", justifyContent: "flex-end", gap: "8px", marginTop: "4px" }}>
            <button type="button" className="sidebar-btn" onClick={onCancel}>Cancel</button>
            <button type="submit" className="sidebar-btn active" disabled={saving}>
              {saving ? "Saving…" : existing ? "Save" : "Create"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

