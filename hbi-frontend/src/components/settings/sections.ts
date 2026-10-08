/** Settings sections: the page's nav and Ctrl-K both read this list. */
export interface SettingsSection {
  id: string;
  label: string;
  /** Words Ctrl-K and the filter box match. */
  keywords: string;
}

export const SETTINGS_SECTIONS: SettingsSection[] = [
  { id: "connection", label: "Connection", keywords: "brain url api key server reconnect" },
  { id: "agents", label: "Agents & profiles", keywords: "context profiles tools allowlist system prompt model preference" },
  { id: "models", label: "Models & routing", keywords: "providers llm budget spend cap privacy data policy cooldown rate limit" },
  { id: "scheduler", label: "Scheduler", keywords: "pause start stop tick interval sleep idle error budget" },
  { id: "about", label: "About", keywords: "version commit build deployed" },
];
