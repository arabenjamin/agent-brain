import { useActiveModels } from "../../api/queries";
import { findModel, hintForModel } from "../../lib/models";
import ModelPicker from "../ui/ModelPicker";
import { PROVIDER_CHOICES, providerLabel, type StepDraft, type ToolGroup } from "./model";

// ─── StepCard ─────────────────────────────────────────────────────────────────

export default function StepCard({
  step,
  index,
  total,
  onChange,
  onRemove,
  onMoveUp,
  onMoveDown,
  inputStyle,
  toolGroups,
}: {
  step: StepDraft;
  index: number;
  total: number;
  onChange: (updated: StepDraft) => void;
  onRemove: () => void;
  onMoveUp: () => void;
  onMoveDown: () => void;
  inputStyle: React.CSSProperties;
  toolGroups: ToolGroup[];
}) {
  const knownTools = toolGroups.flatMap((g) => g.tools);
  const update = (patch: Partial<StepDraft>) => onChange({ ...step, ...patch });
  const models = useActiveModels().data;
  const activeModel = models?.active_model;
  const pinned = step.model ? findModel(models?.catalog_models ?? [], step.model) : undefined;
  const dataClass = step.extra.data_class === "public" ? "public" : "private";

  // A pinned model decides where the step runs; write the hint the brain
  // derives from it, so the saved step can't contradict its model.
  const setModel = (model: string) => {
    const row = model ? findModel(models?.catalog_models ?? [], model) : undefined;
    update(row ? { model, provider_hint: hintForModel(row, models?.providers ?? []) } : { model });
  };

  const setArgKey = (i: number, key: string) => {
    const args = step.args.map((a, idx) => idx === i ? { ...a, key } : a);
    update({ args });
  };

  const setArgValue = (i: number, value: string) => {
    const args = step.args.map((a, idx) => idx === i ? { ...a, value } : a);
    update({ args });
  };

  const removeArg = (i: number) => {
    update({ args: step.args.filter((_, idx) => idx !== i) });
  };

  const addArg = () => {
    update({ args: [...step.args, { key: "", value: "" }] });
  };

  const cardStyle: React.CSSProperties = {
    background: "var(--surface2, #313244)",
    border: "1px solid var(--border, #45475a)",
    borderRadius: "8px",
    padding: "12px",
    display: "flex",
    flexDirection: "column",
    gap: "8px",
  };

  const rowStyle: React.CSSProperties = {
    display: "flex",
    alignItems: "center",
    gap: "8px",
  };

  const smallSelectStyle: React.CSSProperties = {
    ...inputStyle,
    padding: "3px 6px",
    fontSize: "0.8rem",
  };

  return (
    <div style={cardStyle}>
      {/* Step header */}
      <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
        <span style={{ fontSize: "0.75rem", opacity: 0.55, fontWeight: 600, minWidth: "48px" }}>
          Step {index + 1}
        </span>
        <div style={{ display: "flex", gap: "4px", marginLeft: "auto" }}>
          <button
            type="button"
            onClick={onMoveUp}
            disabled={index === 0}
            title="Move up"
            style={{ background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "1px 6px", cursor: index === 0 ? "default" : "pointer", fontSize: "0.72rem", color: "inherit", opacity: index === 0 ? 0.3 : 0.7 }}
          >
            ▲
          </button>
          <button
            type="button"
            onClick={onMoveDown}
            disabled={index === total - 1}
            title="Move down"
            style={{ background: "none", border: "1px solid var(--border, #45475a)", borderRadius: "4px", padding: "1px 6px", cursor: index === total - 1 ? "default" : "pointer", fontSize: "0.72rem", color: "inherit", opacity: index === total - 1 ? 0.3 : 0.7 }}
          >
            ▼
          </button>
          <button
            type="button"
            onClick={onRemove}
            title="Remove step"
            style={{ background: "none", border: "1px solid #f87171", borderRadius: "4px", padding: "1px 8px", cursor: "pointer", fontSize: "0.72rem", color: "#f87171" }}
          >
            Remove
          </button>
        </div>
      </div>

      {/* Tool dropdown */}
      <div style={rowStyle}>
        <span style={{ fontSize: "0.82rem", opacity: 0.75, minWidth: "60px" }}>Tool</span>
        <select
          style={{ ...inputStyle, flex: 1 }}
          value={knownTools.includes(step.tool_name) ? step.tool_name : "__custom__"}
          onChange={(e) => {
            // Selecting the custom sentinel must clear tool_name so the custom-name
            // input (gated on tool_name ∉ known tools) appears and the user can type.
            // Previously this branch was a no-op, so the dropdown could never leave
            // the known-tool set — custom tools were unreachable.
            update({ tool_name: e.target.value === "__custom__" ? "" : e.target.value });
          }}
        >
          {toolGroups.map((g) => (
            <optgroup key={g.group} label={g.group}>
              {g.tools.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </optgroup>
          ))}
          <optgroup label="Custom">
            <option value="__custom__">— custom (type below) —</option>
          </optgroup>
        </select>
      </div>

      {/* Custom tool name input — shown when tool not in list */}
      {!knownTools.includes(step.tool_name) && (
        <div style={rowStyle}>
          <span style={{ fontSize: "0.82rem", opacity: 0.75, minWidth: "60px" }}>Name</span>
          <input
            style={{ ...inputStyle, flex: 1 }}
            value={step.tool_name}
            onChange={(e) => update({ tool_name: e.target.value })}
            placeholder="custom_tool_name"
            spellCheck={false}
          />
        </div>
      )}

      {/* Arguments */}
      <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
        <span style={{ fontSize: "0.78rem", opacity: 0.6 }}>
          Arguments{" "}
          <span style={{ opacity: 0.5 }}>
            — use <code style={{ fontSize: "0.75rem" }}>{"{{goal}}"}</code>, <code style={{ fontSize: "0.75rem" }}>{"{{task_id}}"}</code>, <code style={{ fontSize: "0.75rem" }}>{"{{date}}"}</code>. Objects/arrays/numbers are entered as JSON (<code style={{ fontSize: "0.75rem" }}>{"{\"limit\":\"40\"}"}</code>, <code style={{ fontSize: "0.75rem" }}>{"[\"a\",\"b\"]"}</code>, <code style={{ fontSize: "0.75rem" }}>15000</code>); quote a value to keep it a string.
          </span>
        </span>
        {step.args.map((arg, i) => (
          <div key={i} style={{ display: "flex", gap: "6px", alignItems: "center" }}>
            <input
              style={{ ...inputStyle, width: "120px", flex: "0 0 120px", fontSize: "0.82rem" }}
              value={arg.key}
              onChange={(e) => setArgKey(i, e.target.value)}
              placeholder="key"
              spellCheck={false}
            />
            <span style={{ opacity: 0.4, fontSize: "0.8rem" }}>=</span>
            <input
              style={{ ...inputStyle, flex: 1, fontSize: "0.82rem" }}
              value={arg.value}
              onChange={(e) => setArgValue(i, e.target.value)}
              placeholder="value"
              spellCheck={false}
            />
            <button
              type="button"
              onClick={() => removeArg(i)}
              style={{ background: "none", border: "none", cursor: "pointer", color: "#f87171", fontSize: "0.9rem", padding: "0 4px", flexShrink: 0 }}
              title="Remove argument"
            >
              ×
            </button>
          </div>
        ))}
        <button
          type="button"
          onClick={addArg}
          style={{ alignSelf: "flex-start", background: "none", border: "1px dashed var(--border, #45475a)", borderRadius: "4px", padding: "2px 10px", cursor: "pointer", fontSize: "0.78rem", color: "var(--accent, #89b4fa)", marginTop: "2px" }}
        >
          + Add argument
        </button>
      </div>

      {/* Fields this editor doesn't edit, kept as they are on save. */}
      {Object.keys(step.extra).length > 0 && (
        <div style={{ fontSize: "0.75rem", opacity: 0.65 }} title="Kept as they are when you save; edit them in Raw JSON">
          Also kept:{" "}
          {Object.entries(step.extra).map(([k, v], i) => (
            <span key={k}>
              {i > 0 && " · "}
              <code style={{ fontSize: "0.73rem" }}>{k}</code>={extraValue(v)}
            </span>
          ))}
        </div>
      )}

      {/* Provider + Priority row */}
      <div style={{ display: "flex", gap: "8px", flexWrap: "wrap", alignItems: "center" }}>
        <div style={{ display: "flex", alignItems: "flex-start", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65, paddingTop: 3 }}>Model</span>
          <ModelPicker
            value={step.model}
            onChange={setModel}
            purpose="background"
            dataClass={dataClass}
            defaultLabel="none: “Runs on” decides"
            style={smallSelectStyle}
            title="Pin this step to one model. The routing policy still judges it; a model it refuses fails the step with the reason."
          />
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65 }}>Runs on</span>
          {pinned ? (
            <span style={{ fontSize: "0.8rem" }} title="Derived from the pinned model">
              {pinned.provider} <span style={{ opacity: 0.55 }}>(from the model)</span>
            </span>
          ) : (
          <select
            style={smallSelectStyle}
            value={step.provider_hint}
            onChange={(e) => update({ provider_hint: e.target.value })}
            title="Without a pinned model, only local vs cloud changes where a step runs."
          >
            {/* Keep a value the list doesn't offer (ollama-cloud, anthropic,
                gemini), labelled with what it really does. */}
            {((PROVIDER_CHOICES as readonly string[]).includes(step.provider_hint)
              ? [...PROVIDER_CHOICES]
              : [...PROVIDER_CHOICES, step.provider_hint]
            ).map((p) => (
              <option key={p || "cloud"} value={p}>{providerLabel(p, activeModel)}</option>
            ))}
          </select>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span style={{ fontSize: "0.78rem", opacity: 0.65 }}>Priority</span>
          <select
            style={smallSelectStyle}
            value={step.priority}
            onChange={(e) => update({ priority: Number(e.target.value) })}
          >
            <option value={0}>0 — low</option>
            <option value={1}>1 — normal</option>
            <option value={2}>2 — high</option>
            <option value={3}>3 — critical</option>
          </select>
        </div>
      </div>
    </div>
  );
}

function extraValue(v: unknown): string {
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > 60 ? `${s.slice(0, 59)}…` : s;
}
