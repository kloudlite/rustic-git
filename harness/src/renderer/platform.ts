import type { ApiEnvironment, ApiSnapshot, ApiWorkspace } from "../connect/platform";
import type { Environment, ServiceState, Snapshot, Workspace } from "./model";

/** The /v1 shapes main hands over, as the panels' own types. Nothing here invents a value: what
    the API has no field for (queue, agents, files, changes, a port's protocol) stays empty. */

export const LOADING = "loading…";

/** ipcRenderer wraps a main-side throw as "Error invoking remote method 'x': Error: msg". */
export const ipcError = (e: unknown) => (e instanceof Error ? e.message : String(e)).replace(/^Error invoking remote method '[^']*': (\w*Error: )?/, "");

export function toWorkspace(w: ApiWorkspace): Workspace {
  return {
    id: w.id,
    name: w.name,
    repo: w.repo ?? "",
    branch: w.branch ?? "",
    state: w.state === "ready" ? "running" : "stopped",
    packages: w.packages.map((p) => {
      const at = p.lastIndexOf("@");
      return at > 0 ? { name: p.slice(0, at), version: p.slice(at + 1), pinned: true } : { name: p, version: "" };
    }),
    queue: [],
    ephemerals: [],
    files: [],
    changes: [],
  };
}

export function toEnvironment(e: ApiEnvironment, team: string): Environment {
  return {
    id: e.id,
    name: e.name,
    owner: e.owner === team ? "team" : "you",
    region: e.region,
    // `vol/{owner}/{id}`: only set once something was pushed, and its last segment names the history.
    volume: e.volume?.split("/").pop(),
    services: e.services.map((s) => {
      const st = e.serviceStatus.find((x) => x.name === s.name);
      const state: ServiceState = e.state === "stopped" ? "stopped" : e.state === "error" ? "failed" : st?.ready ? "running" : "starting";
      const ic = st?.interceptedBy ? e.intercepts.find((i) => i.service === s.name && i.workspace === st.interceptedBy) : undefined;
      return {
        name: s.name,
        image: s.image,
        state,
        ...(st?.message ? { note: st.message } : {}),
        ports: s.ports.map((port) => ({
          port,
          protocol: "tcp" as const,
          ...(ic ? { intercept: { workspace: ic.workspace, port: ic.ports.find((p) => p.service === port)?.workspace ?? port } } : {}),
        })),
      };
    }),
  };
}

export function toSnapshot(s: ApiSnapshot, environment: string): Snapshot {
  return {
    id: s.id,
    name: s.message || s.id,
    environment,
    at: s.createdAt ? new Date(s.createdAt).toLocaleString() : "",
    by: "",
    services: s.services ?? 0,
    ...(s.phase !== "ready" ? { note: s.phase } : {}),
  };
}
