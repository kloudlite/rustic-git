import type { Machine } from "../../model";

/**
 * One terminal. `owner` is the session tab it was opened from — the drawer
 * shows only the active tab's terminals — and `session` is its tmux session
 * inside the scope, so the same tab on another device reattaches the same
 * shells rather than forking new ones.
 */
export type TermTab = { id: string; label: string; scope: string; owner: string; session: string; banner: string; at: number };

let seq = 0;

/** The tool server's own rule (`[a-z0-9-]{1,48}`, no leading dash), so a name is refused here and never reaches tmux's argv. */
export const SESSION_RE = /^[a-z0-9][a-z0-9-]{0,47}$/;

/**
 * A tab id is arbitrary (a workspace id, an email-shaped thread id); a tmux
 * session name is not. Lowercase, anything else a dash, no run of dashes and
 * no dash at either end — and short enough that `kl-<slug>-<n>` still fits 48.
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

/** The first index this tab is not already using, live tabs and listed sessions alike. */
export function nextIndex(taken: string[], ownerTabId: string): number {
  const pre = `kl-${slug(ownerTabId)}-`;
  const used = new Set(sessionsOfTab(taken, ownerTabId).map((n) => Number(n.slice(pre.length))));
  let n = 1;
  while (used.has(n)) n++;
  return n;
}

/** A tab younger than this is spared the reconcile: its session may not be listed yet. */
export const YOUNG_MS = 10_000;

/**
 * The tabs and the scope's tmux sessions are one list, in both directions
 * (owner, 2026-09-17: "close the tab here, the session there should close and
 * vice versa"). Pure so the ordering rules are testable: a session nobody holds
 * gets a tab, a tab whose session is gone goes, and a session is never held
 * twice. `live` is the whole listing; only this tab's names are its business.
 */
export function reconcile(tabs: TermTab[], live: string[], ownerTabId: string, now: number): { add: string[]; remove: string[] } {
  const mine = sessionsOfTab(live, ownerTabId);
  const here = tabs.filter((t) => t.owner === ownerTabId);
  const held = new Set<string>();
  const remove: string[] = [];
  for (const t of here) {
    // A duplicate is dropped whatever the listing says: one tab per session.
    if (held.has(t.session)) remove.push(t.id);
    else if (!mine.includes(t.session) && now - t.at >= YOUNG_MS) remove.push(t.id);
    else held.add(t.session);
  }
  return { add: mine.filter((n) => !held.has(n)), remove };
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
  const dim = ws ? "the workspace is the working directory" : "your machine in the team; workspaces resolve by their tool servers";

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
