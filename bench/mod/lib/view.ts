//! Pure merge of `/v1/workspaces`, `/v1/environments/{selected}` and the sessions service's
//! `/state` into what the pane renders. No fetch here (that's `register.tsx`'s job, same split
//! as `platform.ts`) — this file is `node --test`-able standalone and holds the status/doing
//! rules the brief pins down, so they have one definition instead of one per render branch.
//!
//! Field names below are copied from the real handlers, not the brief's guess:
//! `crates/workspaces/src/model.rs` `Workspace` (`state: WsState`, lowercase: creating/ready/
//! stopped/error/deleted — "ready" is the only "running" value, there is no separate state for
//! it) and `Environment` (`services: Service[]` with `ports: number[]`, `service_status:
//! ServiceStatus[]` with camelCase `interceptedBy` — confirmed by
//! `the_service_status_keys_are_camel_case` in `crates/workspaces/src/api/environments.rs`).

export type V1Workspace = { id: string; name: string; state: "creating" | "ready" | "stopped" | "error" | "deleted" };

export type V1Service = { name: string; ports: number[] };
export type V1ServiceStatus = { name: string; interceptedBy?: string };
export type V1Environment = { id: string; name: string; services: V1Service[]; service_status?: V1ServiceStatus[] };

// Matches bench/sessions/main.ts `Sessions.state()` (Task 2): `{ws: {lines, busy, queued, agents}}`.
export type SessionAgent = { id: string; label: string; status: string };
export type SessionWs = { lines: string[]; busy: boolean; queued: string[]; agents: SessionAgent[] };
export type SessionsState = Record<string, SessionWs>;

export type Row = {
  id: string;
  name: string;
  status: "idle" | "running" | "errored" | "stopped";
  doing: string;
  queued: number;
  intercepts: string[];
  agents: { id: string; label: string; status: string }[];
};

export type EnvRow = { id: string; name: string; services: { name: string; ports: number[]; interceptedBy?: string }[] };

function lastLine(lines: string[], prefix: string): string | undefined {
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i]!.startsWith(prefix)) return lines[i]!.slice(prefix.length);
  }
  return undefined;
}

function doingOf(lines: string[]): string {
  const doing = lastLine(lines, "t:") ?? lastLine(lines, "u:") ?? "";
  return doing.slice(0, 60);
}

function statusOf(ws: V1Workspace, session: SessionWs | undefined): Row["status"] {
  if (ws.state !== "ready") return "stopped";
  const lines = session?.lines ?? [];
  if (lastLine(lines, "s:")?.startsWith("(error)")) return "errored";
  if (session?.busy) return "running";
  return "idle";
}

export function buildView(workspaces: V1Workspace[], envs: V1Environment[], state: SessionsState): { workspaces: Row[]; environment: EnvRow | null } {
  const env = envs[0] ?? null;
  const rows = workspaces.map((ws) => {
    const session = state[ws.id];
    const intercepts = (env?.service_status ?? [])
      .filter((st) => st.interceptedBy === ws.id)
      .flatMap((st) => {
        const svc = env?.services.find((s) => s.name === st.name);
        return (svc?.ports ?? []).map((p) => `${st.name}:${p}`);
      });
    return {
      id: ws.id,
      name: ws.name,
      status: statusOf(ws, session),
      doing: doingOf(session?.lines ?? []),
      queued: session?.queued.length ?? 0,
      intercepts,
      agents: session?.agents.map((a) => ({ id: a.id, label: a.label, status: a.status })) ?? [],
    };
  });
  const environment: EnvRow | null = env && {
    id: env.id,
    name: env.name,
    services: env.services.map((svc) => ({
      name: svc.name,
      ports: svc.ports,
      interceptedBy: env.service_status?.find((st) => st.name === svc.name)?.interceptedBy,
    })),
  };
  return { workspaces: rows, environment };
}
