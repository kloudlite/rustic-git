import type { Machine } from "../../model";

/**
 * One terminal. `owner` is the session tab it was opened from — the drawer shows only the active
 * tab's terminals — and `session` names it within that tab. It is a LABEL now, not a handle: the
 * shell lives in the pod's `shell` sidecar for exactly as long as the socket does (spec §2.3), so
 * nothing on another device reattaches it.
 */
export type TermTab = { id: string; label: string; scope: string; owner: string; session: string; banner: string; at: number };

let seq = 0;

/** Kept for the label's shape: lowercase, dashes, nothing that could be read as an argument. */
export const SESSION_RE = /^[a-z0-9][a-z0-9-]{0,47}$/;

/**
 * A tab id is arbitrary (a workspace id, an email-shaped thread id); a shell's label is not.
 * Lowercase, anything else a dash, no run of dashes and no dash at either end.
 */
export function slug(ownerTabId: string): string {
  const s = ownerTabId.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40).replace(/-+$/g, "");
  return s || "tab";
}

export function sessionName(ownerTabId: string, n: number): string {
  return `kl-${slug(ownerTabId)}-${n}`;
}

/** Every `kl-<slug>-<n>` in a listing belongs to this tab; the rest is somebody else's. */
export function sessionsOfTab(names: string[], ownerTabId: string): string[] {
  const pre = `kl-${slug(ownerTabId)}-`;
  return names.filter((n) => n.startsWith(pre) && /^\d+$/.test(n.slice(pre.length))).sort((a, b) => Number(a.slice(pre.length)) - Number(b.slice(pre.length)));
}

/** The `<n>` in `kl-<slug>-<n>`, or 0 for a name that is not this tab's. */
export function sessionIndex(name: string, ownerTabId: string): number {
  const pre = `kl-${slug(ownerTabId)}-`;
  return name.startsWith(pre) && /^\d+$/.test(name.slice(pre.length)) ? Number(name.slice(pre.length)) : 0;
}

/** The first index this tab is not already using. A tab is a socket now: nothing else holds one. */
export function nextIndex(taken: string[], ownerTabId: string): number {
  const pre = `kl-${slug(ownerTabId)}-`;
  const used = new Set(sessionsOfTab(taken, ownerTabId).map((n) => Number(n.slice(pre.length))));
  let n = 1;
  while (used.has(n)) n++;
  return n;
}

/**
 * A scope is not picked any more: it IS the tab. A workspace tab (or one of
 * its ephemerals) opens in that workspace, every other tab on the bench.
 */
export function scopeOfTab(machine: Machine, ownerTabId: string): string {
  const w = machine.workspaces.find((x) => x.id === ownerTabId || x.ephemerals?.some((e) => e.id === ownerTabId));
  return w && w.state !== "stopped" ? w.id : "bench";
}

export function makeTab(machine: Machine, team: string, ownerTabId: string, scopeId: string, n: number): TermTab {
  const ws = machine.workspaces.find((w) => w.id === scopeId);
  const name = ws?.name ?? "bench";
  // What this shell IS, said once: the person's home in that pod, with none of the code in it.
  const dim = ws ? "the person's home in this workspace's pod — the code is not mounted here" : "the person's home in the bench pod";

  return {
    id: `t${++seq}`,
    label: name,
    scope: scopeId,
    owner: ownerTabId,
    session: sessionName(ownerTabId, n),
    at: Date.now(),
    banner: `kloudlite shell · ${name} · ${team}\r\n\x1b[2m${dim}\x1b[0m`,
  };
}
