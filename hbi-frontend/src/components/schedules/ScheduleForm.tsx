import { useEffect, useRef, useState } from "react";
import { apiRequest } from "../../api/client";
import { jsonToStepDrafts, makeDefaultStep, stepDraftsToJson, stUrl, type ScheduledTask, type StepDraft } from "./model";
import StepCard from "./StepCard";
import { useMergedToolGroups } from "./useToolGroups";

// ─── ScheduledTaskForm ────────────────────────────────────────────────────────

export default function ScheduledTaskForm({
  existing,
  onSaved,
  onCancel,
  embedded = false,
}: {
  existing: ScheduledTask | null;
  /** Called with the saved schedule's id (a new one's id comes from the response). */
  onSaved: (id: string | null) => void;
  onCancel: () => void;
  /** Render in place (inside the drawer) instead of as a modal overlay. */
  embedded?: boolean;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [enabled, setEnabled] = useState(existing?.enabled ?? true);
  const [intervalUnit, setIntervalUnit] = useState<"seconds" | "minutes" | "hours" | "days">("days");
  const [intervalValue, setIntervalValue] = useState<number>(1);
  const toolGroups = useMergedToolGroups();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showRaw, setShowRaw] = useState(false);
  const [rawJson, setRawJson] = useState("");
  const [rawJsonError, setRawJsonError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // AI assist state
  const [showAiAssist, setShowAiAssist] = useState(false);
  const [aiPrompt, setAiPrompt] = useState("");
  const [aiLoading, setAiLoading] = useState(false);
  const [aiError, setAiError] = useState<string | null>(null);

  // Step builder state
  const [steps, setSteps] = useState<StepDraft[]>(() => {
    if (existing) {
      const parsed = jsonToStepDrafts(existing.steps);
      if (parsed && parsed.length > 0) return parsed;
    }
    return [makeDefaultStep()];
  });

  // Sync interval_seconds ↔ unit/value
  useEffect(() => {
    const secs = existing?.interval_seconds ?? 86400;
    if (secs % 86400 === 0) { setIntervalUnit("days"); setIntervalValue(secs / 86400); }
    else if (secs % 3600 === 0) { setIntervalUnit("hours"); setIntervalValue(secs / 3600); }
    else if (secs % 60 === 0) { setIntervalUnit("minutes"); setIntervalValue(secs / 60); }
    else { setIntervalUnit("seconds"); setIntervalValue(secs); }
  }, [existing?.interval_seconds]);

  // Sync raw JSON when toggling to raw view
  useEffect(() => {
    if (showRaw) {
      setRawJson(stepDraftsToJson(steps));
      setRawJsonError(null);
    }
  }, [showRaw]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => { inputRef.current?.focus(); }, []);

  const computedIntervalSeconds = () => {
    const multipliers = { seconds: 1, minutes: 60, hours: 3600, days: 86400 };
    return Math.max(1, Math.round(intervalValue * multipliers[intervalUnit]));
  };

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
      // Switching back to builder — try to apply raw JSON
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

  const handleAiGenerate = async () => {
    if (!aiPrompt.trim()) return;
    setAiLoading(true);
    setAiError(null);

    const allTools = toolGroups.flatMap((g) => g.tools).join(", ");
    // Delegate to the `schedule-author` spawnable agent — the brain's own
    // scheduler specialist. It already knows the lint rules (provider_hint at the
    // step top level, {{_prev}} handoffs, valid note_types, source_context) and
    // the spawnable-agent roster, so it drafts a correct chain rather than us
    // re-encoding those rules here. The agent sees NO other context, so `task`
    // must be fully self-contained.
    const task =
      `Draft a scheduled-task step chain for this request:\n"${aiPrompt.trim()}"\n\n` +
      `Tools available to a scheduled step: ${allTools}\n\n` +
      `Rules:\n` +
      `- Each step is an object: tool_name (string), arguments (object), provider_hint ("ollama" for local), priority (0-3).\n` +
      `- provider_hint is a TOP-LEVEL field of the step, never inside arguments (omitting it routes to cloud).\n` +
      `- A step that consumes the previous step's output must reference {{_prev}} in its arguments; a reason step needs context:"{{_prev}}".\n` +
      `- A store_note step needs a source_context and a valid note_type.\n` +
      `- For an open-ended sub-task (research/build/review), use a run_agent step (agent + task).\n` +
      `- Template vars: {{goal}}, {{task_id}}, {{date}}, {{_prev}}.\n\n` +
      `Output ONLY the final JSON array of steps inside a \`\`\`json code block.`;

    try {
      const res = await apiRequest(`/mcp`, {
        method: "POST",
        body: JSON.stringify({
          jsonrpc: "2.0", id: 1, method: "tools/call",
          params: { name: "run_agent", arguments: { agent: "schedule-author", task } },
        }),
      });
      if (!res.ok) throw new Error(`${res.status}`);
      const data = await res.json();
      const rawText: string = data?.result?.content?.[0]?.text ?? "";
      if (data?.result?.isError) {
        throw new Error(rawText || "The scheduler agent returned an error");
      }

      // run_agent wraps the agent's findings as {"answer": "...", "ok": ...,
      // "tool_trace": [...]}. Pull `answer` out first, otherwise the greedy
      // array match below could span both the steps array and tool_trace.
      let findings = rawText;
      try {
        const outer = JSON.parse(rawText);
        if (outer && typeof outer.answer === "string") findings = outer.answer;
      } catch { /* not wrapped JSON — treat the whole text as findings */ }

      // Prefer a fenced ```json block; fall back to the first bare [...] array.
      const fenced = findings.match(/```(?:json)?\s*(\[[\s\S]*?\])\s*```/);
      const bare = findings.match(/\[[\s\S]*\]/);
      const arrayText = fenced ? fenced[1] : bare ? bare[0] : null;
      if (!arrayText) throw new Error(`No step array in the agent's reply:\n${findings.slice(0, 400)}`);

      const parsed = jsonToStepDrafts(arrayText);
      if (!parsed || parsed.length === 0) throw new Error("Could not parse the step array the agent returned");

      setSteps(parsed);
      setShowAiAssist(false);
      setAiPrompt("");
    } catch (e) {
      setAiError(String(e));
    } finally {
      setAiLoading(false);
    }
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;

    // If raw editor is open, try to apply it first
    if (showRaw && !applyRawJson()) return;

    const stepsJson = stepDraftsToJson(steps);

    setSaving(true);
    setError(null);

    const body: Record<string, unknown> = {
      name: name.trim(),
      description: description.trim() || null,
      enabled,
      interval_seconds: computedIntervalSeconds(),
      steps: stepsJson,
    };

    try {
      const res = existing
        ? await apiRequest(stUrl(`/${existing.id}`), { method: "PUT", body: JSON.stringify(body) })
        : await apiRequest(stUrl(), { method: "POST", body: JSON.stringify(body) });

      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error((data as { error?: string }).error ?? `${res.status}`);
      const saved = data as { id?: string; scheduled_task?: { id?: string } };
      onSaved(existing?.id ?? saved.id ?? saved.scheduled_task?.id ?? null);
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

  const embeddedStyle: React.CSSProperties = { display: "flex", flexDirection: "column", gap: "12px" };

  const labelStyle: React.CSSProperties = {
    display: "flex", flexDirection: "column", gap: "4px", fontSize: "0.85rem",
  };

  const inputStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)", border: "1px solid var(--border, #45475a)",
    borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.9rem",
  };

  return (
    <div style={embedded ? undefined : overlayStyle} onClick={embedded ? undefined : (e) => e.target === e.currentTarget && onCancel()}>
      <div style={embedded ? embeddedStyle : cardStyle}>
        {!embedded && (
          <h3 style={{ margin: 0, fontSize: "1rem" }}>
            {existing ? `Edit: ${existing.name}` : "New Scheduled Task"}
          </h3>
        )}

        <form onSubmit={handleSubmit} style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <label style={labelStyle}>
            Name *
            <input
              ref={inputRef}
              style={inputStyle}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Daily news briefing"
              required
            />
          </label>

          <label style={labelStyle}>
            Description
            <textarea
              style={{ ...inputStyle, resize: "vertical", minHeight: "56px" }}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="What this task does…"
            />
          </label>

          {/* Interval */}
          <div style={labelStyle}>
            <span>Interval *</span>
            <div style={{ display: "flex", gap: "8px" }}>
              <input
                type="number"
                min={1}
                style={{ ...inputStyle, width: "80px" }}
                value={intervalValue}
                onChange={(e) => setIntervalValue(Math.max(1, Number(e.target.value)))}
              />
              <select
                style={{ ...inputStyle, flex: 1 }}
                value={intervalUnit}
                onChange={(e) => setIntervalUnit(e.target.value as typeof intervalUnit)}
              >
                <option value="seconds">seconds</option>
                <option value="minutes">minutes</option>
                <option value="hours">hours</option>
                <option value="days">days</option>
              </select>
              <span style={{ fontSize: "0.8rem", opacity: 0.6, alignSelf: "center", whiteSpace: "nowrap" }}>
                = {computedIntervalSeconds()}s
              </span>
            </div>
          </div>

          {/* Enabled */}
          <label style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "0.85rem", cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
              style={{ accentColor: "var(--accent, #89b4fa)" }}
            />
            Enabled
          </label>

          {/* AI Assist */}
          <div style={{ border: "1px solid var(--border, #45475a)", borderRadius: "8px", overflow: "hidden" }}>
            <button
              type="button"
              onClick={() => setShowAiAssist((v) => !v)}
              style={{ width: "100%", background: showAiAssist ? "var(--surface2, #313244)" : "none", border: "none", padding: "8px 12px", cursor: "pointer", display: "flex", alignItems: "center", gap: "8px", color: "var(--accent, #89b4fa)", fontSize: "0.82rem", textAlign: "left" }}
            >
              <span>{showAiAssist ? "▲" : "▼"}</span>
              <span>Ask the scheduler agent to design steps</span>
            </button>

            {showAiAssist && (
              <div style={{ padding: "10px 12px", display: "flex", flexDirection: "column", gap: "8px", background: "var(--surface2, #313244)" }}>
                <textarea
                  style={{ background: "var(--surface, #1e1e2e)", border: "1px solid var(--border, #45475a)", borderRadius: "6px", padding: "6px 10px", color: "inherit", fontSize: "0.85rem", resize: "vertical", minHeight: "64px" }}
                  value={aiPrompt}
                  onChange={(e) => setAiPrompt(e.target.value)}
                  placeholder="Describe what you want this task to do, e.g. 'Every day, search for AI news and store a summary note'. The scheduler agent (schedule-author) drafts lint-clean steps — this may take a few seconds."
                  spellCheck={false}
                />
                {aiError && <span style={{ color: "#f87171", fontSize: "0.78rem" }}>{aiError}</span>}
                <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                  <button
                    type="button"
                    className="sidebar-btn active"
                    onClick={handleAiGenerate}
                    disabled={aiLoading || !aiPrompt.trim()}
                  >
                    {aiLoading ? "Generating…" : "Generate Steps"}
                  </button>
                  <span style={{ fontSize: "0.75rem", opacity: 0.45 }}>
                    Replaces current steps
                  </span>
                </div>
              </div>
            )}
          </div>

          {/* Steps header */}
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span style={{ fontSize: "0.85rem", fontWeight: 600 }}>
              Steps ({steps.length})
            </span>
            <button
              type="button"
              onClick={handleToggleRaw}
              style={{ marginLeft: "auto", background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "2px 8px", cursor: "pointer", fontSize: "0.75rem", color: showRaw ? "var(--accent, #89b4fa)" : "inherit" }}
            >
              {showRaw ? "← Builder" : "Raw JSON"}
            </button>
          </div>

          {/* Raw JSON editor */}
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
              <span style={{ fontSize: "0.75rem", opacity: 0.5 }}>
                Fields: <code style={{ fontSize: "0.73rem" }}>tool_name</code>, <code style={{ fontSize: "0.73rem" }}>arguments</code>, <code style={{ fontSize: "0.73rem" }}>provider_hint</code>, <code style={{ fontSize: "0.73rem" }}>model</code>, <code style={{ fontSize: "0.73rem" }}>priority</code>, <code style={{ fontSize: "0.73rem" }}>max_attempts</code>
              </span>
            </div>
          )}

          {/* Step cards */}
          {!showRaw && (
            <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              {steps.length === 0 && (
                <p style={{ opacity: 0.45, fontSize: "0.85rem", margin: 0 }}>No steps yet. Add one below.</p>
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
