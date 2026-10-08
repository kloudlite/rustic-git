//! `Backend.space()`: the sidebar's data, read from the platform with the bench's own tool token.
//! Workspaces are the CURRENT USER's (`KL_OWNER`; the API already scopes the list to the caller,
//! the filter is belt and braces) in the space `KL_TEAM`; environments are every environment of
//! that team whoever made them. A clone's parent and task come from the bench's clone file
//! (`readClones`), because the platform lists a subagent clone as an ordinary workspace. Processes
//! and the changed-file count come from each READY workspace's `kl ide serve` (crates/ide), best
//! effort under a 5 s cap: a pod that does not answer leaves those fields out, never fails the view.
//! Nothing here formats a token into a string; errors carry the API's text or ours.
import { forgetClone, readClones } from "@kloudlite-tui/agent";
import { apiJson, podGet, podPost } from "@kloudlite-tui/tools";
import type { SpaceEnvironment, SpaceProcess, SpaceView, SpaceWorkspace } from "./index.ts";

const POD_CAP_MS = 5000;
const LOG_LINES = 20;

const capped = <T>(p: Promise<T>): Promise<T | undefined> =>
  Promise.race([p.catch(() => undefined), new Promise<undefined>((r) => setTimeout(r, POD_CAP_MS))]);

async function processes(ws: string): Promise<SpaceProcess[]> {
  const { processes } = await podPost<{ processes: Omit<SpaceProcess, "logs">[] }>(ws, "process_list", {}, POD_CAP_MS);
  return Promise.all(
    processes.map(async (p) => {
      let logs: string[] = [];
      if (p.state === "running") {
        // from offset 0: the ring is 4 MiB at most and a running process has no cursor of ours to resume
        const o = await capped(podPost<{ stdout?: string; stderr?: string }>(ws, "process_output", { id: p.id }, POD_CAP_MS));
        logs = `${o?.stdout ?? ""}${o?.stderr ?? ""}`.split("\n").filter(Boolean).slice(-LOG_LINES);
      }
      return { ...p, logs };
    }),
  );
}

async function enrich(w: SpaceWorkspace): Promise<void> {
  const [procs, ch] = await Promise.all([
    capped(processes(w.id)),
    capped(podGet<{ changes?: unknown[] }>(w.id, "/fs/changes", POD_CAP_MS)),
  ]);
  if (procs) w.processes = procs;
  if (Array.isArray(ch?.changes)) w.changes = ch.changes.length;
}

export async function space(): Promise<SpaceView> {
  const user = process.env.KL_OWNER ?? "";
  const team = process.env.KL_TEAM || undefined;
  const bench = process.env.KL_BENCH;
  const q = (k: string) => (team ? `?${k}=${encodeURIComponent(team)}` : "");
  let ws: any[], envs: any[], mine: any[] | undefined;
  try {
    [ws, envs, mine] = await Promise.all([
      apiJson<any[]>("GET", `/v1/workspaces${q("team")}`),
      apiJson<any[]>("GET", `/v1/environments${q("owner")}`),
      apiJson<any[]>("GET", "/v1/me/environments").catch(() => undefined),
    ]);
  } catch (e: any) {
    return { available: false, error: String(e?.message ?? e).slice(0, 200), user, workspaces: [], environments: [] };
  }

  const clones = readClones();
  const listed = new Set(ws.map((w) => w.id));
  // a clone that left the platform's list was deleted elsewhere
  for (const r of clones) if (!listed.has(r.id)) forgetClone(r.id);
  const rec = new Map(clones.map((r) => [r.id, r]));

  const workspaces: SpaceWorkspace[] = ws
    .filter((w) => w.id !== bench && (!user || w.owner === user))
    .map((w) => ({
      id: w.id,
      name: w.name,
      owner: w.owner,
      state: w.state,
      repo: w.repo ?? undefined,
      branch: w.branch ?? undefined,
      attached_environment: w.attached_environment ?? undefined,
      parent: rec.get(w.id)?.parent,
      task: rec.get(w.id)?.task,
    }));
  await Promise.all(workspaces.filter((w) => w.state === "ready").map(enrich));

  const environments: SpaceEnvironment[] = envs.map((e) => {
    const status = new Map<string, any>((e.service_status ?? []).map((s: any) => [s.name, s]));
    return {
      id: e.id,
      name: e.name,
      owner: e.owner,
      state: e.state,
      services: (e.services ?? []).map((s: any) => ({ name: s.name, ports: s.ports ?? [], interceptedBy: status.get(s.name)?.interceptedBy ?? undefined })),
    };
  });
  const follows = mine?.find((m) => m.team === team) ?? mine?.[0];
  return { available: true, user, workspaces, environments, connected: follows?.environment ?? undefined };
}
