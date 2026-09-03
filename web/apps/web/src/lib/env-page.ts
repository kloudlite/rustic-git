import { cache } from "react";
import { getEnvironment, listVolumes, volumeHistory } from "@/lib/api";
import type { ApiCommitRecord, ApiEnvironment, ApiService, ApiVolumeSummary } from "@/lib/api";

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
  return {
    id,
    env,
    volume,
    history,
    name: env?.name ?? volume?.display_name ?? id,
    services: env?.services ?? [],
  };
});
