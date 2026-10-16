import { useState } from "react";
import { useProfiles } from "../../api/queries";
import { answerBody, runAssist, type AssistRun } from "../../lib/assist";
import ModelPicker from "./ModelPicker";

/**
 * One widget for asking an agent to help with a form (HBI plan §9.4): a
 * prompt, an agent (the widget's natural default), a model (the agent's own
 * unless chosen), and a run button. The run is read-only, so nothing is
 * stored; the tools the agent actually ran are shown with the result, and
 * `onResult` puts the answer into the form.
 *
 * `buildTask` turns the person's prompt into the agent's whole task (the agent
 * sees nothing else). `onResult` gets the agent's own text and returns an
 * error to show when the answer can't be used (e.g. no step array in it);
 * `action` is the preset that ran, when one did.
 */
export default function AgentAssist({
  title,
  defaultAgent,
  placeholder,
  buildTask,
  onResult,
  runLabel = "Ask",
  note,
  actions,
}: {
  title: string;
  defaultAgent: string;
  placeholder: string;
  buildTask: (prompt: string, action?: string) => string;
  /** May be async (e.g. to check the draft, or ask again); the run stays busy until it settles. */
  onResult: (body: string, run: AssistRun, action: string | undefined, prompt: string) => string | null | Promise<string | null>;
  runLabel?: string;
  /** A line beside the run button, e.g. "Replaces current steps". */
  note?: string;
  /** Preset requests shown as buttons (each passes its key to `buildTask`); the prompt may then be empty. */
  actions?: { key: string; label: string }[];
}) {
  const [open, setOpen] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [agent, setAgent] = useState(defaultAgent);
  const [model, setModel] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [last, setLast] = useState<AssistRun | null>(null);
  const profiles = useProfiles().data ?? [];
  const spawnable = profiles.filter((p) => p.spawnable);
  const profile = profiles.find((p) => p.name === agent);
  const agentModel = profile?.model_preference?.trim();
  const dataClass = profile?.data_class === "public" ? "public" : "private";

  const run = async (action?: string) => {
    if (!action && !prompt.trim()) return;
    setBusy(true);
    setError(null);
    setLast(null);
    try {
      const r = await runAssist(agent, buildTask(prompt.trim(), action), model);
      setLast(r);
      if (!r.ok) throw new Error(`${r.agent || agent} ran but gave no answer`);
      const problem = await onResult(answerBody(r.answer), r, action, prompt.trim());
      if (problem) setError(problem);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const box: React.CSSProperties = { border: "1px solid var(--border, #45475a)", borderRadius: 8, overflow: "hidden" };
  const field: React.CSSProperties = {
    background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #45475a)", borderRadius: 6,
    padding: "4px 8px", color: "inherit", fontSize: "0.8rem",
  };

  return (
    <div style={box}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        style={{ width: "100%", background: open ? "var(--surface2, #313244)" : "none", border: "none", padding: "8px 12px", cursor: "pointer", display: "flex", gap: 8, color: "var(--accent, #89b4fa)", fontSize: "0.82rem", textAlign: "left" }}
      >
        <span>{open ? "▲" : "▼"}</span>
        <span>{title}</span>
      </button>
      {open && (
        <div style={{ padding: "10px 12px", display: "flex", flexDirection: "column", gap: 8, background: "var(--surface2, #313244)" }}>
          <textarea
            style={{ ...field, fontSize: "0.85rem", resize: "vertical", minHeight: 64, padding: "6px 10px" }}
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            placeholder={placeholder}
            spellCheck={false}
          />
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "flex-start", fontSize: "0.78rem" }}>
            <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
              <span style={{ opacity: 0.65 }}>Agent</span>
              <select style={field} value={agent} onChange={(e) => setAgent(e.target.value)}>
                {!spawnable.some((p) => p.name === agent) && <option value={agent}>{agent}</option>}
                {spawnable.map((p) => (
                  <option key={p.name} value={p.name}>{p.name}</option>
                ))}
              </select>
            </label>
            <label style={{ display: "flex", gap: 6, alignItems: "flex-start" }}>
              <span style={{ opacity: 0.65, paddingTop: 4 }}>Model</span>
              <ModelPicker
                value={model}
                onChange={setModel}
                purpose="subagent"
                dataClass={dataClass}
                defaultLabel={`agent default (${agentModel || "the chat model"})`}
                style={field}
              />
            </label>
          </div>
          {error && <span className="tone-text-red" style={{ fontSize: "0.78rem", whiteSpace: "pre-wrap" }}>{error}</span>}
          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
            <button type="button" className="sidebar-btn active" onClick={() => run()} disabled={busy || !prompt.trim()}>
              {busy ? "Working…" : runLabel}
            </button>
            {actions?.map((a) => (
              <button key={a.key} type="button" className="sidebar-btn" onClick={() => run(a.key)} disabled={busy}>
                {a.label}
              </button>
            ))}
            {note && <span style={{ fontSize: "0.75rem", opacity: 0.45 }}>{note}</span>}
          </div>
          {last && (
            <div style={{ fontSize: "0.75rem", opacity: 0.8 }} data-testid="assist-trace">
              <span style={{ opacity: 0.65 }}>{last.agent || agent} on {last.model || "?"} ran: </span>
              {last.trace.length === 0 ? (
                <span className="tone-text-yellow">no tools (the answer is its own, unchecked)</span>
              ) : (
                last.trace.map((t, i) => (
                  <span key={i}>
                    {i > 0 && " · "}
                    <span className={t.success ? "tone-text-green" : "tone-text-red"}>{t.success ? "✓" : "✗"}</span> {t.tool}
                  </span>
                ))
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
