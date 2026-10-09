/**
 * The model catalog as the pickers see it (`/api/models`): which models exist,
 * who serves them, what each costs and may do with prompts, and whether the
 * routing policy would let one run for a given kind of work right now.
 *
 * Shared by `<ModelPicker>` and the step editor's derived "Runs on", so a
 * choice the brain would refuse says so before it is saved.
 */

export type Purpose = "background" | "subagent" | "chat";
export type DataClass = "private" | "public";

/** One `catalog_models` row. `admission[purpose][class]` is null when allowed, else the refusal. */
export interface CatalogModel {
  name: string;
  provider: string;
  model: string;
  capabilities?: string;
  context_window?: number;
  admission?: Partial<Record<Purpose, Partial<Record<DataClass, string | null>>>>;
}

/** One `providers` row (the provider registry). */
export interface ProviderInfo {
  name: string;
  kind: string;
  billing: "local" | "free" | "paid";
  data_policy: "no_log" | "may_log" | "may_train";
  enabled: boolean;
  key_configured: boolean;
}

export interface ModelGroup {
  provider: string;
  info?: ProviderInfo;
  models: CatalogModel[];
}

/** The catalog row a stored value means: its model id or its name, ignoring case (as the brain matches). */
export function findModel(models: CatalogModel[], value: string): CatalogModel | undefined {
  const v = value.trim().toLowerCase();
  if (!v) return undefined;
  return models.find((m) => m.model.toLowerCase() === v) ?? models.find((m) => m.name.toLowerCase() === v);
}

/** Models grouped by provider: local first, then free, then paid, each by name. */
export function groupModels(models: CatalogModel[], providers: ProviderInfo[]): ModelGroup[] {
  const order = { local: 0, free: 1, paid: 2 } as const;
  const byName = new Map(providers.map((p) => [p.name, p]));
  const groups = new Map<string, ModelGroup>();
  for (const m of models) {
    const g = groups.get(m.provider) ?? { provider: m.provider, info: byName.get(m.provider), models: [] };
    g.models.push(m);
    groups.set(m.provider, g);
  }
  return [...groups.values()]
    .map((g) => ({ ...g, models: [...g.models].sort((a, b) => a.model.localeCompare(b.model)) }))
    .sort(
      (a, b) =>
        (order[a.info?.billing ?? "paid"] - order[b.info?.billing ?? "paid"]) ||
        a.provider.localeCompare(b.provider),
    );
}

/** Why the policy would refuse this model for this work right now; null when it would run. */
export function refusal(m: CatalogModel, purpose: Purpose, dataClass: DataClass): string | null {
  return m.admission?.[purpose]?.[dataClass] ?? null;
}

/**
 * The step `provider_hint` a model implies, as the brain derives it
 * (`model_router::provider_hint_for`): `ollama` for a local provider, else the
 * provider's own name.
 */
export function hintForModel(m: CatalogModel, providers: ProviderInfo[]): string {
  const p = providers.find((x) => x.name === m.provider);
  return !p || p.billing === "local" ? "ollama" : m.provider;
}

/** Short badges for a provider: billing and what it may do with prompts. */
export function providerBadges(info?: ProviderInfo): string[] {
  if (!info) return ["unknown provider"];
  const policy = { no_log: "no log", may_log: "may log", may_train: "may train" }[info.data_policy];
  const out: string[] = [info.billing, policy];
  if (!info.enabled) out.push("disabled");
  else if (!info.key_configured) out.push("no key");
  return out;
}
