import type { Machine } from "../../model";

/** One terminal, opened against a scope the person picked. */
export type TermTab = { id: string; label: string; scope: string; banner: string };

let seq = 0;

/**
 * A scope is the machine itself — where it runs, with the connected environment
 * reachable and no working copy — or one workspace. An agent's copy is never a
 * scope: nobody works in one but the agent.
 */
export function scopesOf(machine: Machine) {
  return [
    { id: "machine", label: "machine", sub: "no working copy", kind: "machine" as const },
    ...machine.workspaces.map((w) => ({ id: w.id, label: w.name, sub: w.branch, kind: "workspace" as const })),
  ];
}

export function makeTab(machine: Machine, env: string, scopeId: string): TermTab {
  const ws = machine.workspaces.find((w) => w.id === scopeId);
  const banner = ws
    ? `kloudlite shell · ${ws.name} · ${env}\r\n\x1b[2mthe workspace is the working directory; services resolve by name\x1b[0m`
    : `kloudlite shell · ${machine.owner.split("@")[0]} · ${env}\r\n\x1b[2mmachine scope: no working copy, the environment is reachable\x1b[0m`;

  return { id: `t${++seq}`, label: ws?.name ?? "machine", scope: scopeId, banner };
}
