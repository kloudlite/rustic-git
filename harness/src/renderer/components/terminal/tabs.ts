import type { Machine } from "../../model";

/** One terminal, opened against a scope the person picked. */
export type TermTab = { id: string; label: string; scope: string; banner: string };

let seq = 0;

/**
 * A scope is the bench — the person's own machine in the team, where the
 * agents run — or one of their workspaces, reached through its tool server.
 * A stopped workspace is listed but not openable: there is no pod to attach
 * to, and the workspace page is where it is started.
 */
export function scopesOf(machine: Machine) {
  return [
    { id: "bench", label: "bench", sub: "your machine in the team", kind: "bench" as const },
    ...machine.workspaces.map((w) => ({
      id: w.id,
      label: w.name,
      sub: w.state === "stopped" ? "stopped" : w.branch,
      kind: "workspace" as const,
      disabled: w.state === "stopped",
    })),
  ];
}

export function makeTab(machine: Machine, team: string, scopeId: string): TermTab {
  const ws = machine.workspaces.find((w) => w.id === scopeId);
  const name = ws?.name ?? "bench";
  const dim = ws ? "the workspace is the working directory" : "your machine in the team; workspaces resolve by their tool servers";

  return { id: `t${++seq}`, label: name, scope: scopeId, banner: `kloudlite shell · ${name} · ${team}\r\n\x1b[2m${dim}\x1b[0m` };
}
