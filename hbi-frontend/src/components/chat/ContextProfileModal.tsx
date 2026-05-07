import { useEffect, useState, useCallback } from "react";
import { getBrainUrl, getApiKey } from "../../api/config";

// ── Types ─────────────────────────────────────────────────────────────────────

interface AgentSummary {
  name: string;
  description?: string;
  tools?: string[];
  tool_count?: number;
  model_preference?: string;
  provider_hint?: string;
}

interface AgentDetail {
  name: string;
  description: string;
  tools: string[];
  system_prompt: string;
  token_budget?: number;
  pre_load_query?: string;
  model_preference?: string;
  provider_hint?: string;
}

interface SkillGroup {
  name: string;
  tools: string[];
}

interface AvailableProvider {
  type: string;
  name: string;
}

interface CatalogModel {
  name: string;
  provider: string;
  model: string;
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
  activeProfile: string;
  profiles: AgentSummary[];
  onProfileChange: (name: string) => void;
  onProfilesChanged: () => void;
  disabled?: boolean;
}

type Mode = "list" | "edit" | "create";

const EMPTY_FORM: AgentDetail = {
  name: "",
  description: "",
  system_prompt: "",
  tools: [],
  token_budget: 8000,
  pre_load_query: "",
  model_preference: "",
  provider_hint: "",
};

// ── Helpers ───────────────────────────────────────────────────────────────────

function hdr(color: string) {
  return {
    padding: "14px 18px",
    borderBottom: "1px solid var(--border, #2a2f3e)",
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    background: color,
  } as React.CSSProperties;
}

function CloseBtn({ onClick }: { onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      style={{
        background: "none",
        border: "none",
        color: "var(--text-muted, #7a8099)",
        cursor: "pointer",
        fontSize: 18,
        lineHeight: 1,
        padding: "2px 6px",
        borderRadius: 4,
      }}
    >
      ✕
    </button>
  );
}

// ── Main component ────────────────────────────────────────────────────────────

export default function ContextProfileModal({
  isOpen,
  onClose,
  activeProfile,
  profiles,
  onProfileChange,
  onProfilesChanged,
  disabled,
}: Props) {
  const [mode, setMode] = useState<Mode>("list");
  const [form, setForm] = useState<AgentDetail>(EMPTY_FORM);
  const [skillGroups, setSkillGroups] = useState<SkillGroup[]>([]);
  const [availableProviders, setAvailableProviders] = useState<AvailableProvider[]>([]);
  const [catalogModels, setCatalogModels] = useState<CatalogModel[]>([]);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [fetchingEdit, setFetchingEdit] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Reset to list mode when modal opens/closes
  useEffect(() => {
    if (isOpen) {
      setMode("list");
      setError(null);
    }
  }, [isOpen]);

  // Fetch skills and models when the modal opens
  useEffect(() => {
    if (!isOpen) return;
    const h = { Authorization: `Bearer ${getApiKey()}` };
    fetch(`${getBrainUrl()}/api/skills`, { headers: h })
      .then((r) => r.json())
      .then((data: { skills?: SkillGroup[] }) => setSkillGroups(data.skills ?? []))
      .catch(() => {});
    fetch(`${getBrainUrl()}/api/models`, { headers: h })
      .then((r) => r.json())
      .then((data: { available_providers?: AvailableProvider[]; catalog_models?: CatalogModel[] }) => {
        setAvailableProviders(data.available_providers ?? []);
        setCatalogModels(data.catalog_models ?? []);
      })
      .catch(() => {});
  }, [isOpen]);

  // Escape key to close or go back
  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (mode !== "list") setMode("list");
        else onClose();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [isOpen, mode, onClose]);

  const startCreate = useCallback(() => {
    setForm(EMPTY_FORM);
    setError(null);
    setMode("create");
  }, []);

  const startEdit = useCallback(async (name: string) => {
    setFetchingEdit(true);
    setError(null);
    try {
      const res = await fetch(`${getBrainUrl()}/api/contexts/${name}`, {
        headers: { Authorization: `Bearer ${getApiKey()}` },
      });
      const data = (await res.json()) as AgentDetail;
      setForm({
        name: data.name ?? name,
        description: data.description ?? "",
        system_prompt: data.system_prompt ?? "",
        tools: data.tools ?? [],
        token_budget: data.token_budget ?? 8000,
        pre_load_query: data.pre_load_query ?? "",
        model_preference: data.model_preference ?? "",
        provider_hint: data.provider_hint ?? "",
      });
      setMode("edit");
    } catch {
      setError("Failed to load agent details");
    } finally {
      setFetchingEdit(false);
    }
  }, []);

  const handleSave = useCallback(async () => {
    if (!form.name.trim()) {
      setError("Name is required");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const payload: AgentDetail = {
        ...form,
        name: form.name.trim(),
        // omit empty optional strings so they serialize as null in YAML
        pre_load_query: form.pre_load_query?.trim() || undefined,
        model_preference: form.model_preference?.trim() || undefined,
        provider_hint: form.provider_hint?.trim() || undefined,
      };
      const res = await fetch(`${getBrainUrl()}/api/contexts`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${getApiKey()}`,
        },
        body: JSON.stringify(payload),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error((body as { error?: string }).error ?? `HTTP ${res.status}`);
      }
      onProfilesChanged();
      setMode("list");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  }, [form, onProfilesChanged]);

  const handleDelete = useCallback(
    async (name: string) => {
      if (!confirm(`Delete agent "${name}"? This cannot be undone.`)) return;
      setDeleting(name);
      setError(null);
      try {
        await fetch(`${getBrainUrl()}/api/contexts/${name}`, {
          method: "DELETE",
          headers: { Authorization: `Bearer ${getApiKey()}` },
        });
        if (activeProfile === name) onProfileChange("general");
        onProfilesChanged();
      } catch {
        setError(`Failed to delete "${name}"`);
      } finally {
        setDeleting(null);
      }
    },
    [activeProfile, onProfileChange, onProfilesChanged]
  );

  const toggleTool = useCallback((tool: string) => {
    setForm((prev) => ({
      ...prev,
      tools: prev.tools.includes(tool)
        ? prev.tools.filter((t) => t !== tool)
        : [...prev.tools, tool],
    }));
  }, []);

  const allToolsFlat = skillGroups.flatMap((s) => s.tools);
  const allSelected =
    allToolsFlat.length > 0 && allToolsFlat.every((t) => form.tools.includes(t));

  const toggleAllTools = useCallback(() => {
    setForm((prev) => ({
      ...prev,
      tools: allSelected ? [] : [...allToolsFlat],
    }));
  }, [allSelected, allToolsFlat]);

  if (!isOpen) return null;

  // ── Shared overlay ───────────────────────────────────────────────────────

  const isEditOrCreate = mode === "edit" || mode === "create";

  return (
    <div
      onClick={() => { if (mode === "list") onClose(); }}
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 200,
        background: "rgba(0,0,0,0.55)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        backdropFilter: "blur(2px)",
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          background: "var(--bg-panel, #13161f)",
          border: "1px solid var(--border, #2a2f3e)",
          borderRadius: 10,
          width: isEditOrCreate ? "min(680px, 95vw)" : "min(500px, 92vw)",
          maxHeight: "85vh",
          display: "flex",
          flexDirection: "column",
          overflow: "hidden",
          boxShadow: "0 0 48px rgba(0,0,0,0.6)",
          fontFamily: "'JetBrains Mono','Fira Code',monospace",
          transition: "width 0.15s",
        }}
      >
        {mode === "list" && (
          <ListPane
            profiles={profiles}
            activeProfile={activeProfile}
            disabled={!!disabled}
            deleting={deleting}
            fetchingEdit={fetchingEdit}
            error={error}
            onSelect={(name) => { onProfileChange(name); onClose(); }}
            onEdit={startEdit}
            onDelete={handleDelete}
            onCreate={startCreate}
            onClose={onClose}
          />
        )}

        {(mode === "edit" || mode === "create") && (
          <EditPane
            mode={mode}
            form={form}
            skillGroups={skillGroups}
            availableProviders={availableProviders}
            catalogModels={catalogModels}
            allSelected={allSelected}
            saving={saving}
            error={error}
            onFormChange={setForm}
            onToggleTool={toggleTool}
            onToggleAll={toggleAllTools}
            onSave={handleSave}
            onBack={() => setMode("list")}
          />
        )}
      </div>
    </div>
  );
}

// ── List pane ─────────────────────────────────────────────────────────────────

function ListPane({
  profiles,
  activeProfile,
  disabled,
  deleting,
  fetchingEdit,
  error,
  onSelect,
  onEdit,
  onDelete,
  onCreate,
  onClose,
}: {
  profiles: AgentSummary[];
  activeProfile: string;
  disabled: boolean;
  deleting: string | null;
  fetchingEdit: boolean;
  error: string | null;
  onSelect: (name: string) => void;
  onEdit: (name: string) => void;
  onDelete: (name: string) => void;
  onCreate: () => void;
  onClose: () => void;
}) {
  return (
    <>
      <div style={hdr("transparent")}>
        <div style={{ fontSize: 13, fontWeight: 700, color: "var(--text, #d4d8e8)" }}>
          🤖 Agents
        </div>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <button
            onClick={onCreate}
            disabled={disabled}
            style={{
              background: "rgba(79,142,247,0.12)",
              border: "1px solid rgba(79,142,247,0.3)",
              borderRadius: 5,
              color: "var(--accent, #4f8ef7)",
              cursor: disabled ? "default" : "pointer",
              fontSize: 11,
              padding: "3px 10px",
              fontFamily: "inherit",
            }}
          >
            + New Agent
          </button>
          <CloseBtn onClick={onClose} />
        </div>
      </div>

      <div style={{ overflow: "auto", padding: "12px 14px", display: "flex", flexDirection: "column", gap: 8 }}>
        <p style={{ margin: "0 0 6px", fontSize: 11, color: "var(--text-muted, #7a8099)" }}>
          Select the active agent — defines which tools the LLM can call and its persona.
        </p>

        {error && (
          <div style={{ fontSize: 11, color: "#f87171", padding: "6px 10px", background: "rgba(248,113,113,0.1)", borderRadius: 5 }}>
            {error}
          </div>
        )}

        {(profiles.length > 0 ? profiles : [{ name: "general" }]).map((p) => {
          const isActive = p.name === activeProfile;
          const isBusy = deleting === p.name || fetchingEdit;
          const toolCount = p.tools?.length ?? p.tool_count ?? 0;

          return (
            <div
              key={p.name}
              style={{
                display: "flex",
                alignItems: "flex-start",
                gap: 8,
                padding: "10px 12px",
                borderRadius: 6,
                border: isActive
                  ? "1px solid var(--accent, #4f8ef7)"
                  : "1px solid var(--border, #2a2f3e)",
                background: isActive ? "rgba(79,142,247,0.07)" : "var(--bg-card, #1a1e2a)",
              }}
            >
              {/* Main click area — select */}
              <button
                disabled={disabled || isBusy}
                onClick={() => onSelect(p.name)}
                style={{
                  flex: 1,
                  background: "none",
                  border: "none",
                  cursor: disabled ? "default" : "pointer",
                  textAlign: "left",
                  fontFamily: "inherit",
                  padding: 0,
                  opacity: isBusy ? 0.5 : 1,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: isActive ? "var(--accent, #4f8ef7)" : "var(--text, #d4d8e8)" }}>
                    {p.name}
                  </span>
                  <span style={{ fontSize: 9, color: "var(--text-muted, #7a8099)" }}>
                    {toolCount} tools
                  </span>
                  {p.model_preference && (
                    <span style={{ fontSize: 9, color: "#a78bfa", opacity: 0.8 }}>
                      {p.model_preference}
                    </span>
                  )}
                  {isActive && (
                    <span style={{
                      fontSize: 9, fontWeight: 700, padding: "1px 7px", borderRadius: 10,
                      background: "rgba(79,142,247,0.2)", color: "var(--accent, #4f8ef7)",
                      border: "1px solid rgba(79,142,247,0.3)", letterSpacing: "0.05em", textTransform: "uppercase",
                    }}>
                      active
                    </span>
                  )}
                </div>
                {p.description && (
                  <span style={{ fontSize: 10, color: "var(--text-muted, #7a8099)", lineHeight: 1.5, marginTop: 2, display: "block" }}>
                    {p.description}
                  </span>
                )}
              </button>

              {/* Action buttons */}
              <div style={{ display: "flex", gap: 4, flexShrink: 0, paddingTop: 1 }}>
                <ActionBtn
                  label="Edit"
                  disabled={disabled || isBusy}
                  onClick={() => onEdit(p.name)}
                  color="rgba(79,142,247,0.7)"
                />
                {p.name !== "general" && (
                  <ActionBtn
                    label={deleting === p.name ? "…" : "Delete"}
                    disabled={disabled || isBusy}
                    onClick={() => onDelete(p.name)}
                    color="rgba(248,113,113,0.7)"
                  />
                )}
              </div>
            </div>
          );
        })}
      </div>

      <div style={{
        padding: "8px 18px",
        borderTop: "1px solid var(--border, #2a2f3e)",
        fontSize: 10,
        color: "var(--text-muted, #4a5068)",
        textAlign: "right",
      }}>
        click a name to activate · Esc to close
      </div>
    </>
  );
}

// ── Edit / Create pane ────────────────────────────────────────────────────────

function EditPane({
  mode,
  form,
  skillGroups,
  availableProviders,
  catalogModels,
  allSelected,
  saving,
  error,
  onFormChange,
  onToggleTool,
  onToggleAll,
  onSave,
  onBack,
}: {
  mode: "edit" | "create";
  form: AgentDetail;
  skillGroups: SkillGroup[];
  availableProviders: AvailableProvider[];
  catalogModels: CatalogModel[];
  allSelected: boolean;
  saving: boolean;
  error: string | null;
  onFormChange: (f: AgentDetail) => void;
  onToggleTool: (t: string) => void;
  onToggleAll: () => void;
  onSave: () => void;
  onBack: () => void;
}) {
  const title = mode === "create" ? "New Agent" : `Edit · ${form.name}`;

  const field = (
    label: string,
    key: keyof AgentDetail,
    opts: { type?: string; rows?: number; placeholder?: string; disabled?: boolean } = {}
  ) => {
    const value = (form[key] ?? "") as string | number;
    const isTextarea = !!opts.rows;
    const Tag = isTextarea ? "textarea" : "input";
    return (
      <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
        <span style={{ fontSize: 10, color: "var(--text-muted, #7a8099)", textTransform: "uppercase", letterSpacing: "0.07em" }}>
          {label}
        </span>
        <Tag
          disabled={opts.disabled}
          type={opts.type ?? "text"}
          rows={opts.rows}
          placeholder={opts.placeholder}
          value={String(value)}
          onChange={(e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) =>
            onFormChange({ ...form, [key]: opts.type === "number" ? Number(e.target.value) : e.target.value })
          }
          style={{
            background: "var(--bg-card, #1a1e2a)",
            border: "1px solid var(--border, #2a2f3e)",
            borderRadius: 5,
            color: opts.disabled ? "var(--text-muted, #7a8099)" : "var(--text, #d4d8e8)",
            fontFamily: "inherit",
            fontSize: 11,
            padding: "6px 9px",
            resize: isTextarea ? "vertical" : undefined,
            minHeight: isTextarea ? (opts.rows ?? 3) * 20 : undefined,
            opacity: opts.disabled ? 0.6 : 1,
          }}
        />
      </label>
    );
  };

  return (
    <>
      <div style={hdr("transparent")}>
        <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
          <button
            onClick={onBack}
            style={{
              background: "none", border: "none",
              color: "var(--text-muted, #7a8099)", cursor: "pointer",
              fontSize: 14, padding: "2px 6px", borderRadius: 4, fontFamily: "inherit",
            }}
          >
            ← Back
          </button>
          <span style={{ fontSize: 13, fontWeight: 700, color: "var(--text, #d4d8e8)" }}>
            🤖 {title}
          </span>
        </div>
        <button
          onClick={onSave}
          disabled={saving}
          style={{
            background: saving ? "rgba(79,142,247,0.3)" : "rgba(79,142,247,0.15)",
            border: "1px solid rgba(79,142,247,0.4)",
            borderRadius: 5,
            color: "var(--accent, #4f8ef7)",
            cursor: saving ? "default" : "pointer",
            fontSize: 11,
            fontWeight: 600,
            padding: "4px 14px",
            fontFamily: "inherit",
          }}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </div>

      <div style={{ overflow: "auto", padding: "14px 16px", display: "flex", flexDirection: "column", gap: 12 }}>
        {error && (
          <div style={{ fontSize: 11, color: "#f87171", padding: "6px 10px", background: "rgba(248,113,113,0.1)", borderRadius: 5 }}>
            {error}
          </div>
        )}

        {field("Name *", "name", { disabled: mode === "edit", placeholder: "e.g. researcher" })}
       
        {field("Description", "description", { rows: 2, placeholder: "What this agent does" })}
        
        {field("System Prompt", "system_prompt", {
          rows: 5,
          placeholder: "You are an agent that…",
        })}

        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 10 }}>
          <SelectField
            label="Model Preference"
            value={form.model_preference ?? ""}
            onChange={(v) => onFormChange({ ...form, model_preference: v })}
            options={[
              { value: "", label: "— inherit —" },
              ...catalogModels.map((m) => ({ value: m.model, label: m.model })),
            ]}
          />
          <SelectField
            label="Provider Hint"
            value={form.provider_hint ?? ""}
            onChange={(v) => onFormChange({ ...form, provider_hint: v })}
            options={[
              { value: "", label: "— inherit —" },
              ...availableProviders.map((p) => ({ value: p.type, label: p.name })),
            ]}
          />
          {field("Token Budget", "token_budget", { type: "number", placeholder: "8000" })}
        </div>

        {field("Pre-load Query (Cypher)", "pre_load_query", {
          rows: 2,
          placeholder: "MATCH (n:Note) WHERE … RETURN n.content AS content LIMIT 10",
        })}

        {/* Tool selector */}
        <div>
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 6 }}>
            <span style={{ fontSize: 10, color: "var(--text-muted, #7a8099)", textTransform: "uppercase", letterSpacing: "0.07em" }}>
              Allowed Tools ({form.tools.length} selected)
            </span>
            <button
              onClick={onToggleAll}
              style={{
                background: "none", border: "none",
                color: "var(--accent, #4f8ef7)", cursor: "pointer",
                fontSize: 10, padding: 0, fontFamily: "inherit",
              }}
            >
              {allSelected ? "Deselect all" : "Select all"}
            </button>
          </div>
          <div style={{
            display: "flex", flexDirection: "column", gap: 8,
            maxHeight: 220, overflow: "auto",
            background: "var(--bg-card, #1a1e2a)",
            border: "1px solid var(--border, #2a2f3e)",
            borderRadius: 5, padding: "10px 12px",
          }}>
            {skillGroups.length === 0 && (
              <span style={{ fontSize: 10, color: "var(--text-muted, #7a8099)" }}>Loading tools…</span>
            )}
            {skillGroups.map((skill) => (
              <div key={skill.name}>
                <div style={{ fontSize: 10, fontWeight: 600, color: "var(--text-muted, #7a8099)", marginBottom: 4, textTransform: "uppercase", letterSpacing: "0.06em" }}>
                  {skill.name}
                </div>
                <div style={{ display: "flex", flexWrap: "wrap", gap: "4px 8px" }}>
                  {skill.tools.map((tool) => {
                    const checked = form.tools.includes(tool);
                    return (
                      <label
                        key={tool}
                        style={{
                          display: "flex", alignItems: "center", gap: 4,
                          cursor: "pointer", fontSize: 11,
                          color: checked ? "var(--text, #d4d8e8)" : "var(--text-muted, #7a8099)",
                        }}
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => onToggleTool(tool)}
                          style={{ cursor: "pointer", accentColor: "var(--accent, #4f8ef7)" }}
                        />
                        {tool}
                      </label>
                    );
                  })}
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>

      <div style={{
        padding: "8px 18px",
        borderTop: "1px solid var(--border, #2a2f3e)",
        fontSize: 10,
        color: "var(--text-muted, #4a5068)",
        textAlign: "right",
      }}>
        Esc to go back without saving
      </div>
    </>
  );
}

// ── Select field ─────────────────────────────────────────────────────────────

function SelectField({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
}) {
  return (
    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
      <span style={{ fontSize: 10, color: "var(--text-muted, #7a8099)", textTransform: "uppercase", letterSpacing: "0.07em" }}>
        {label}
      </span>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        style={{
          background: "var(--bg-card, #1a1e2a)",
          border: "1px solid var(--border, #2a2f3e)",
          borderRadius: 5,
          color: value ? "var(--text, #d4d8e8)" : "var(--text-muted, #7a8099)",
          fontFamily: "inherit",
          fontSize: 11,
          padding: "6px 9px",
          cursor: "pointer",
          appearance: "none",
          backgroundImage: `url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath d='M0 0l5 6 5-6z' fill='%237a8099'/%3E%3C/svg%3E")`,
          backgroundRepeat: "no-repeat",
          backgroundPosition: "right 8px center",
          paddingRight: 24,
        }}
      >
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </label>
  );
}

// ── Shared small button ───────────────────────────────────────────────────────

function ActionBtn({
  label,
  disabled,
  onClick,
  color,
}: {
  label: string;
  disabled: boolean;
  onClick: () => void;
  color: string;
}) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      style={{
        background: "none",
        border: `1px solid ${color}`,
        borderRadius: 4,
        color,
        cursor: disabled ? "default" : "pointer",
        fontSize: 10,
        padding: "2px 8px",
        fontFamily: "inherit",
        opacity: disabled ? 0.5 : 1,
      }}
    >
      {label}
    </button>
  );
}
