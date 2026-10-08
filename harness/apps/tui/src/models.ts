import type { CatalogModel, ModelRef } from "@kloudlite-tui/backend";
import { backend, hello } from "./hello.ts";

export type { ModelRef };

/** Every provider's models, as the backend knows them; refreshed from the live lists. */
export let catalog: CatalogModel[] = hello().catalog;

/**
 * Every provider is dynamic — pi's bundled JSON is only an offline seed, and a seed goes stale.
 * The backend fetches each configured provider's live list and persists it.
 */
export async function refreshCatalog(): Promise<CatalogModel[]> {
  catalog = await backend().models.refresh();
  return catalog;
}

/** The backend resolved it (saved choice, else an anthropic opus, else the first model). */
export const DEFAULT_MODEL: ModelRef = hello().defaultModel;

/** provider id → auth info; re-resolved after a login. */
export async function loadProviderAuth(): Promise<Map<string, { ok: boolean; envKey?: string }>> {
  const all = await backend().auth.providers();
  return new Map(all.map((a) => [a.provider, { ok: a.ok, envKey: a.envKeys[0] }]));
}

export function findModel(ref: ModelRef): CatalogModel | undefined {
  return catalog.find((m) => m.provider === ref.provider && m.id === ref.id);
}

export function modelLabel(ref: ModelRef): string {
  return findModel(ref)?.name ?? ref.id;
}
