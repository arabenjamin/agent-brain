import { describe, expect, it } from "vitest";
import { findModel, groupModels, hintForModel, providerBadges, refusal, type CatalogModel, type ProviderInfo } from "./models";

const providers: ProviderInfo[] = [
  { name: "anthropic", kind: "anthropic", billing: "paid", data_policy: "no_log", enabled: true, key_configured: true },
  { name: "ollama", kind: "ollama", billing: "local", data_policy: "no_log", enabled: true, key_configured: true },
  { name: "ollama-cloud", kind: "openai_compat", billing: "free", data_policy: "may_train", enabled: true, key_configured: false },
];
const models: CatalogModel[] = [
  { name: "claude-haiku-4-5", provider: "anthropic", model: "claude-haiku-4-5-20251001" },
  { name: "gpt-oss:120b-cloud", provider: "ollama-cloud", model: "gpt-oss:120b-cloud",
    admission: { background: { private: "private work only reaches no_log providers", public: null } } },
  { name: "gemma4:latest", provider: "ollama", model: "gemma4:latest" },
];

describe("model catalog", () => {
  it("matches a stored value exactly, by model id or name", () => {
    expect(findModel(models, "CLAUDE-HAIKU-4-5")?.model).toBe("claude-haiku-4-5-20251001");
    expect(findModel(models, "gemma4")).toBeUndefined();
    expect(findModel(models, " ")).toBeUndefined();
  });

  it("groups local first, then free, then paid", () => {
    expect(groupModels(models, providers).map((g) => g.provider)).toEqual(["ollama", "ollama-cloud", "anthropic"]);
  });

  it("reads the policy's refusal for the work at hand", () => {
    const cloud = models[1];
    expect(refusal(cloud, "background", "private")).toContain("no_log");
    expect(refusal(cloud, "background", "public")).toBeNull();
    expect(refusal(cloud, "chat", "private")).toBeNull();
  });

  it("derives the hint the brain derives", () => {
    expect(hintForModel(models[2], providers)).toBe("ollama");
    expect(hintForModel(models[1], providers)).toBe("ollama-cloud");
    expect(hintForModel({ name: "x", provider: "nobody", model: "x" }, providers)).toBe("ollama");
  });

  it("badges billing, data policy and a missing key", () => {
    expect(providerBadges(providers[2])).toEqual(["free", "may train", "no key"]);
    expect(providerBadges(undefined)).toEqual(["unknown provider"]);
  });
});
