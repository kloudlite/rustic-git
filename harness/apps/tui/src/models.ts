import { listModels, models, providerAuth, readSettings, type ModelRef } from "@kloudlite-tui/agent";

export type { ModelRef };

/** Full pi-ai catalog: every provider, every model. */
export const catalog = listModels();

/**
 * Every provider is dynamic — the JSON bundled in pi's dist is only an offline
 * seed, and a seed goes stale (it shipped with `deepseek-v4-flash`, an id the
 * API no longer serves). `refresh()` fetches each *configured* provider's live
 * list and persists it, so the next launch starts correct; unconfigured,
 * unknown and static providers are skipped, and provider errors come back in
 * the result rather than rejecting.
 */
export async function refreshCatalog(): Promise<{ provider: string; id: string; name: string }[]> {
  await models.refresh();
  return listModels();
}

export const DEFAULT_MODEL: ModelRef = (() => {
  // An explicitly chosen model wins outright — it is not validated against the
  // catalog, because this runs at import, before `refreshCatalog()`. On a cold
  // start the only models known are pi's bundled seed, so validating here
  // discarded any model the seed predates (a saved `deepseek-flash` silently
  // became an anthropic fallback on first launch). A model the provider has
  // since dropped surfaces as a request error, which beats losing the choice.
  const saved = readSettings().defaultModel;
  if (saved) return saved;
  const anthropic = catalog.find(
    (m) => m.provider === "anthropic" && /opus/.test(m.id),
  );
  return anthropic ?? catalog[0] ?? { provider: "anthropic", id: "claude-opus-5" };
})();

/** provider id → auth info; re-resolved after a login. */
export async function loadProviderAuth(): Promise<Map<string, { ok: boolean; envKey?: string }>> {
  const all = await providerAuth();
  return new Map(all.map((a) => [a.provider, { ok: a.ok, envKey: a.envKeys[0] }]));
}

export function modelLabel(ref: ModelRef): string {
  const m = models.getModel(ref.provider, ref.id);
  return m?.name ?? ref.id;
}
