import { useActiveModels } from "../../api/queries";
import { findModel, groupModels, providerBadges, refusal, type DataClass, type Purpose } from "../../lib/models";

/**
 * One model chooser for everywhere an agent or a step runs (HBI plan §9.4):
 * the catalog grouped by provider, each provider with its billing and
 * data-policy badges, each model marked when the routing policy would refuse
 * it for this work right now (a private-work rule, a spend cap, a cooldown).
 *
 * `value` is a catalog model id, or "" for the default, which `defaultLabel`
 * names (e.g. "agent default (gemma4-e4b)"). A stored value the catalog does
 * not have is kept and shown as unknown, never silently replaced.
 */
export default function ModelPicker({
  value,
  onChange,
  purpose,
  dataClass = "private",
  defaultLabel,
  style,
  className,
  title,
}: {
  value: string;
  onChange: (model: string) => void;
  purpose: Purpose;
  dataClass?: DataClass;
  /** The "" option's label. */
  defaultLabel: string;
  style?: React.CSSProperties;
  className?: string;
  title?: string;
}) {
  const info = useActiveModels().data;
  const models = info?.catalog_models ?? [];
  const groups = groupModels(models, info?.providers ?? []);
  const chosen = value ? findModel(models, value) : undefined;
  const why = chosen ? refusal(chosen, purpose, dataClass) : null;
  const unknown = !!value && models.length > 0 && !chosen;

  return (
    <span style={{ display: "inline-flex", flexDirection: "column", gap: 2, minWidth: 0 }}>
      <select
        value={chosen?.model ?? value}
        onChange={(e) => onChange(e.target.value)}
        style={style}
        className={className}
        title={title}
        aria-label="Model"
      >
        <option value="">{defaultLabel}</option>
        {unknown && <option value={value}>{value} — not in the catalog</option>}
        {groups.map((g) => (
          <optgroup key={g.provider} label={`${g.provider} · ${providerBadges(g.info).join(" · ")}`}>
            {g.models.map((m) => {
              const r = refusal(m, purpose, dataClass);
              return (
                <option key={`${g.provider}/${m.model}`} value={m.model} title={r ?? undefined}>
                  {m.model}
                  {r ? " — refused here" : ""}
                </option>
              );
            })}
          </optgroup>
        ))}
      </select>
      {why && (
        <span className="tone-text-yellow" style={{ fontSize: "0.72rem" }}>
          Would be refused for {dataClass} {purpose} work: {why}
        </span>
      )}
      {unknown && (
        <span className="tone-text-red" style={{ fontSize: "0.72rem" }}>
          {value} is not in the model catalog; this would fail every run.
        </span>
      )}
    </span>
  );
}
