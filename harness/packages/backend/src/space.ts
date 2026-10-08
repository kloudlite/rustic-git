//! `Backend.space()`: the sidebar's data, read from the platform with the bench's own tool token.
//! Workspaces are the CURRENT USER's (`KL_OWNER`; the API already scopes the list to the caller,
//! the filter is belt and braces) in the space `KL_TEAM`; environments are every environment of
//! that team whoever made them. A clone's parent and task (`clone_of`, `task`) are on the
//! platform's workspace doc, so every row comes from the API; nothing here is kept on the bench's
//! disk apart from the log cursors below, which die with the process. Processes
//! and the changed-file count come from each READY workspace's `kl ide serve` (crates/ide), best
//! effort under a 5 s cap: a pod that does not answer leaves those fields out, never fails the view.
//! Nothing here formats a token into a string; errors carry the API's text or ours.
import { apiJson, podGet, podPost } from "@kloudlite-tui/tools";
import type { SpaceEnvironment, SpaceProcess, SpaceView, SpaceWorkspace } from "./index.ts";

const POD_CAP_MS = 5000;
const LOG_LINES = 20;

const capped = <T>(p: Promise<T>): Promise<T | undefined> =>
  Promise.race([p.catch(() => undefined), new Promise<undefined>((r) => setTimeout(r, POD_CAP_MS))]);

/** Per running process: where the last read stopped and the tail so far, so a beat reads only
 * what is new instead of the whole 4 MiB ring. Keyed "ws/procId"; pruned when the process leaves
 * the list. */
const cursors = new Map<string, { next: number; next_err: number; lines: string[] }>();

async function processes(ws: string): Promise<SpaceProcess[]> {
  const { processes } = await podPost<{ processes: Omit<SpaceProcess, "logs">[] }>(ws, "process_list", {}, POD_CAP_MS);
  const live = new Set(processes.map((p) => `${ws}/${p.id}`));
  for (const k of cursors.keys()) if (k.startsWith(`${ws}/`) && !live.has(k)) cursors.delete(k);
  return Promise.all(
    processes.map(async (p) => {
      let logs: string[] = [];
      if (p.state === "running") {
        const key = `${ws}/${p.id}`;
        const c = cursors.get(key) ?? { next: 0, next_err: 0, lines: [] };
        const o = await capped(
          podPost<{ stdout?: string; stderr?: string; next?: number; next_err?: number }>(
            ws, "process_output", { id: p.id, since: c.next, since_err: c.next_err }, POD_CAP_MS,
          ),
        );
        if (o) {
          // each stream split on its own: joined first, stdout's last partial line would fuse with stderr's first
          const add = (t?: string) => (t ?? "").split("\n").filter(Boolean);
          c.lines = [...c.lines, ...add(o.stdout), ...add(o.stderr)].slice(-LOG_LINES);
          c.next = o.next ?? c.next;
          c.next_err = o.next_err ?? c.next_err;
          cursors.set(key, c);
        }
        logs = c.lines;
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
      parent: w.clone_of ?? undefined,
      task: w.task ?? undefined,
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
