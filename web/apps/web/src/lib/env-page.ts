import { cache } from "react";
import { getEnvironment, listVolumes, volumeHistory } from "@/lib/api";
import type { ApiCommitRecord, ApiEnvironment, ApiService, ApiVolumeSummary } from "@/lib/api";

/** What a push wrote into a snapshot record's free-form `state` — see
 *  `crates/workspaces/src/upstream.rs::Provenance`. Once the environment is deleted this is the
 *  only thing left that can say what the snapshot was OF, which is why an archived page reads its
 *  name and its services from here rather than from an object that no longer exists. */
export type Provenance = { kind?: string; name?: string; services?: ApiService[] };

export function provenanceOf(state: unknown): Provenance {
  return state && typeof state === "object" ? (state as Provenance) : {};
}

export type EnvPage = {
  id: string;
  /** `null` for an ARCHIVED environment: the object is gone, its snapshots are not. */
  env: ApiEnvironment | null;
  volume: ApiVolumeSummary | null;
  history: ApiCommitRecord[];
  /** The snapshot the environment last landed on, if a restore has named one. */
  name: string;
  /** What the environment is RUNNING. Empty for an archived one — it runs nothing, and the
   *  services a snapshot recorded are the restore's business, not this page's. */
  services: ApiService[];
};

/** One read of everything the environment page and its layout both need.
 *
 *  `cache` so the layout's header counts and the page's own body are ONE round trip per render:
 *  a layout and its child render in the same pass, and asking the api twice for the same
 *  environment is how a two-tab header ends up disagreeing with the body under it.
 *
 *  An archived id is not an error here. `getEnvironment` 404s for one, and everything the page
 *  shows then comes from the volume's own records — which is the whole point of an archived row. */
export const loadEnvPage = cache(async function loadEnvPage(
  token: string,
  owner: string,
  id: string,
): Promise<EnvPage | null> {
  const [envRes, historyRes] = await Promise.all([getEnvironment(token, id), volumeHistory(token, id)]);
  const env = envRes.ok ? envRes.value : null;
  const history = historyRes.ok ? historyRes.value : [];
  // Neither a live environment nor a single snapshot: there is nothing here to show, and a
  // fabricated empty page would be indistinguishable from one whose data failed to load.
  if (!env && history.length === 0) return null;

  let volume: ApiVolumeSummary | null = null;
  if (!env) {
    const vols = await listVolumes(token, "environment", owner);
    volume = vols.ok ? (vols.value.find((v) => v.name === id) ?? null) : null;
  }
  const newest = provenanceOf(history[0]?.state);
  return {
    id,
    env,
    volume,
    history,
    name: env?.name ?? volume?.display_name ?? newest.name ?? id,
    services: env?.services ?? [],
  };
});
