//! What the sidebar draws, and `fromSpace`, which maps the backend's `SpaceView` (the platform's
//! real workspaces, environments and ide-server processes) onto it. The demo data lives in
//! ./fixtures.ts for tests only.
import type { SpaceView } from "@kloudlite-tui/backend";

export type WorkspaceStatus = "running" | "attached" | "stopped" | "cloning";

export type Workspace = {
  id: string;
  name: string;
  /** User that owns (and can attach to) this workspace. */
  owner: string;
  status: WorkspaceStatus;
  /**
   * Parent workspace id — set only on ephemeral workspaces. The hierarchy is
   * exactly three levels: main session › workspaces › ephemeral workspaces
   * (spun off a workspace to test something, like a git worktree). The flat
   * list stays in tree order, so a workspace's ephemerals follow it directly.
   */
  parent?: string;
  /** Ephemeral only: what the agent in it is doing. */
  task?: string;
  /** Clone progress, e.g. "42%" — only meaningful while status is "cloning". */
  progress?: string;
  /** Ports the workspace exposes. */
  ports: number[];
  /** Long-running processes inside the workspace (dev servers, watchers). */
  processes?: Process[];
  /** Uncommitted files in the workspace's checkout. */
  changes?: number;
  repo: string;
  branch: string;
};

export type ProcessStatus = "running" | "starting" | "exited" | "crashed";

/** A process the workspace runs — what the backend is actually doing. */
export type Process = {
  id: string;
  name: string;
  command: string;
  status: ProcessStatus;
  /** RFC 3339, from the pod's process list. */
  startedAt?: string;
  /** Exit code, for a process that stopped. */
  code?: number;
  /** Recent output, oldest first; stderr lines carry `err`. */
  logs: { text: string; err?: true }[];
};

export type Service = {
  name: string;
  port: number;
  /** Protocol shown with the port; defaults to tcp. */
  proto?: "http" | "tcp";
  /** Workspace currently intercepting this service's traffic, if any. */
  interceptedBy?: string;
};

/**
 * An environment is somewhere a working session can plug in: its services, and
 * nothing else. **Workspaces do not belong to an environment** — they belong to
 * the working session, which carries them whole when it connects elsewhere, so
 * another environment's workspaces can never appear in this session's list.
 */
export type Environment = {
  id: string;
  name: string;
  /** User (or team) that owns this environment. */
  owner: string;
  services: Service[];
  /**
   * The restore point the environment is sitting on. The agent takes and
   * switches these; the UI only ever shows which one is in effect, never a
   * list of them.
   */
  snapshot?: string;
};

/**
 * The workspace an ephemeral one should hang off: a workspace parents itself,
 * an ephemeral hands its own parent over, so nothing nests deeper than one.
 */
export function parentFor(workspaces: Workspace[], w: Workspace): string {
  return w.parent && workspaces.some((p) => p.id === w.parent) ? w.parent : w.id;
}

/** The workspace, preceded by its parent when it is an ephemeral one. */
export function wsPath(workspaces: Workspace[], w: Workspace): string[] {
  const parent = w.parent ? workspaces.find((p) => p.id === w.parent) : undefined;
  return parent ? [parent.name, w.name] : [w.name];
}

/** Display label: own environments by name, others as owner/name. */
export function envLabel(e: Environment, user: string): string {
  return e.owner === user ? e.name : `${e.owner}/${e.name}`;
}

const base = (w: string) => w.split("/").pop() ?? w;
const RUNNERS = new Set(["npm", "pnpm", "yarn", "bun"]);
const SHELLS = new Set(["sh", "bash", "zsh"]);

/** The pod's `process_list` sends whole epoch seconds (crates/ide/src/procs.rs `now_rfc3339`, misnamed),
 * which Date.parse reads as NaN; accept both so an RFC 3339 fix there needs no change here. */
export function isoStart(s: string | undefined): string | undefined {
  return s && /^\d+$/.test(s) ? new Date(Number(s) * 1000).toISOString() : s;
}

/** A short label for a process from its command line: `npm run dev`, `vite`, `node server.js`. */
export function procName(cmd: string): string {
  let c = cmd.trim();
  for (;;) {
    const n = c.replace(/^cd\s+\S+\s*&&\s*/, "").replace(/^[A-Za-z_][A-Za-z0-9_]*=\S*\s+/, "");
    if (n === c) break;
    c = n;
  }
  const [first = "", ...rest] = c.split(/\s+/).filter(Boolean);
  const head = base(first);
  let out: string;
  if (RUNNERS.has(head) && rest[0] === "run" && rest[1]) out = `${head} run ${rest[1]}`;
  else if ((head === "npx" || head === "bunx") && rest[0]) out = rest[0];
  else if (SHELLS.has(head) && (rest[0] === "-c" || rest[0] === "-lc")) {
    const script = rest.slice(1).join(" ").replace(/^(["'])(.*)\1$/, "$2");
    return procName(script);
  } else out = [head, rest.find((a) => !a.startsWith("-"))].filter(Boolean).join(" ");
  return out.length > 32 ? `${out.slice(0, 31)}…` : out;
}

/**
 * The backend's view as sidebar rows. Tree order is what Sidebar relies on: each workspace, then
 * its clones. ponytail: the platform has no clone progress and `error` is shown as stopped; add a
 * state when the sidebar learns to colour failures.
 */
export function fromSpace(v: SpaceView): { workspaces: Workspace[]; environments: Environment[]; envIndex: number } {
  const name = new Map(v.workspaces.map((w) => [w.id, w.name]));
  const row = (w: SpaceView["workspaces"][number]): Workspace => ({
    id: w.id,
    name: w.name,
    owner: w.owner,
    status: w.attached_environment ? "attached" : w.state === "stopped" || w.state === "error" ? "stopped" : w.state === "creating" ? "cloning" : "running",
    parent: w.parent,
    task: w.task,
    ports: [],
    changes: w.changes,
    repo: w.repo ?? "",
    branch: w.branch ?? "",
    processes: w.processes?.map((p) => ({
      id: p.id,
      // a process in another tree says whose it is; main's needs no label
      name: (p.tree && p.tree !== "main" ? `${p.tree} · ` : "") + (procName(p.cmd) || p.id),
      startedAt: isoStart(p.started_at),
      command: p.cmd,
      status: p.state === "running" ? "running" : p.failed || (p.exit_code ?? 0) !== 0 ? "crashed" : "exited",
      code: p.exit_code ?? undefined,
      logs: p.logs,
    })),
  });
  const by = new Map(v.workspaces.map((w) => [w.id, w]));
  // the tree is three levels (workspace › task), so a clone of a clone hangs off the root
  const rootOf = (w: SpaceView["workspaces"][number]): string => {
    for (let i = 0; w.parent && by.has(w.parent) && i < 16; i++) w = by.get(w.parent)!;
    return w.id;
  };
  const top = v.workspaces.filter((w) => rootOf(w) === w.id);
  const workspaces = top.flatMap((w) => [
    row({ ...w, parent: undefined }),
    ...v.workspaces.filter((c) => c !== w && rootOf(c) === w.id).map((c) => row({ ...c, parent: w.id })),
  ]);
  const environments: Environment[] = v.environments.map((e) => ({
    id: e.id,
    name: e.name,
    owner: e.owner,
    services: e.services.map((s) => ({ name: s.name, port: s.ports[0] ?? 0, interceptedBy: s.interceptedBy && (name.get(s.interceptedBy) ?? s.interceptedBy) })),
    snapshot: e.snapshot,
  }));
  return { workspaces, environments, envIndex: Math.max(0, environments.findIndex((e) => e.id === v.connected)) };
}
