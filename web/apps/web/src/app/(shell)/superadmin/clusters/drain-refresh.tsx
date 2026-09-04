"use client";

import { AutoRefresh } from "@/components/app/auto-refresh";

/** Same shape as `RollTable`'s `anyRollingOut` — poll every 10s normally, 2s while a node on this
 *  page is mid-drain (Global Constraint: "2s while a roll or drain the page itself started is in
 *  progress"). A `decommissionStatus` starting with `"draining"` is exactly that condition. */
export function DrainRefresh({ anyDraining }: { anyDraining: boolean }) {
  return <AutoRefresh intervalMs={anyDraining ? 2_000 : 10_000} />;
}
