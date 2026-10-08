//! Tools that start another session and wait for its answer: `workspace_ask` (main asks a
//! workspace's own session) and `subagent` (a throwaway session with the pod tools only).
//! `subagent` never works in the parent: it clones the parent workspace (a btrfs worktree, so cheap),
//! runs in the clone, commits there and pushes straight into the parent's checked-out branch over
//! SSH (the parent's home seed sets receive.denyCurrentBranch=updateInstead; the owner's platform
//! key is in the clone's pod). A moved parent branch gets ONE rebase round through the same child
//! session; a push that still fails keeps the clone so the commits are not lost. The clone is
//! deleted on every other exit. Needs the bench/main token: a workspace token cannot clone.
//! Delegated sessions use the CALLER's permission callback, so their cards land in the user's TUI.
//! The answer is the last assistant text seen before `agent_end`: Claude sessions emit
//! `agent_end` with an empty message list, so the events are tracked instead.
import { randomBytes } from "node:crypto";
import { api, podExec, type ExecResult, type ToolDef } from "@kloudlite-tui/tools";
import type { SessionHandle, SessionOpts } from "./index.ts";
import { forgetSessions } from "./forget.ts";

export type DelegateDeps = {
  /** Open sessions by key (LocalBackend's map). */
  live: Map<string, SessionHandle>;
  /** Keys mid-turn right now. */
  busy: Set<string>;
  open(key: string, opts: SessionOpts): Promise<SessionHandle>;
  /** Platform API (tools' `api`) and pod exec; injectable so tests run on fakes. */
  api?: (method: string, path: string, body?: unknown) => Promise<string>;
  exec?: (ws: string, cmd: string, timeoutMs?: number) => Promise<ExecResult>;
  /** Waits between polls; tests pass a no-op (the caps count slept time, not wall time). */
  sleep?: (ms: number) => Promise<void>;
  /** Drops a deleted workspace's bench sessions; tests pass a fake. */
  forget?: (ws: string) => Promise<void>;
};

const WS = "/home/kl/workspace";
const sq = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
const hex = () => randomBytes(4).toString("hex");
const tail = (s: string, n: number) => s.trim().split("\n").slice(-n).join("\n");
const MOVED = /non-fast-forward|fetch first/;

/** One commit of whatever the child left. The -c fallbacks apply only when the pod has no git
 * identity (the directory knows no name for the handle); otherwise the person's own identity wins. */
const commitCmd = (task: string) =>
  `cd ${WS} && git add -A && (git diff --cached --quiet || { n=$(git config user.name); e=$(git config user.email); ` +
  `git -c user.name="\${n:-kl subagent}" -c user.email="\${e:-subagent@kloudlite.local}" commit -q -m ${sq(task.split("\n")[0]!.slice(0, 72) || "subagent changes")}; })`;

const textOf = (m: any): string =>
  (Array.isArray(m?.content) ? m.content : [])
    .filter((b: any) => b?.type === "text")
    .map((b: any) => b.text)
    .join("\n")
    .trim();

/** Send, then resolve at the next `agent_end` with the last assistant text. */
function answer(h: SessionHandle, send: () => Promise<void>): Promise<string> {
  return new Promise((resolve, reject) => {
    let last = "";
    const off = h.subscribe((e: any) => {
      if (e.type === "message_end" && e.message?.role === "assistant") last = textOf(e.message) || last;
      if (e.type === "agent_end") {
        off();
        resolve(last || "(no answer)");
      }
    });
    send().catch((err) => {
      off();
      reject(err);
    });
  });
}

async function runInClone(P: string, task: string, deps: DelegateDeps, opts: (e?: Partial<SessionOpts>) => SessionOpts): Promise<string> {
  const call = deps.api ?? api;
  const exec = deps.exec ?? podExec;
  const sleep = deps.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  const p = (id: string) => `/v1/workspaces/${encodeURIComponent(id)}`;
  /** Poll `f` every 2 s until it yields a value; null once `capMs` of waiting is spent. */
  const until = async <T>(f: () => Promise<T | undefined>, capMs: number): Promise<T | null> => {
    for (let waited = 0; ; waited += 2000) {
      const v = await f();
      if (v !== undefined) return v;
      if (waited >= capMs) return null;
      await sleep(2000);
    }
  };
  const forget = deps.forget ?? ((id: string) => forgetSessions(id, deps.live));
  // the subagent's own session history goes with its clone, once the platform took the delete
  const del = async (id: string) => {
    await h?.dispose(); // before its history goes
    h = undefined;
    const r = await call("DELETE", p(id)).catch(() => "error");
    if (!r.startsWith("error") && !r.startsWith("platform tools unavailable")) await forget(id);
  };

  const base = await exec(P, `cd ${WS} && git rev-parse --abbrev-ref HEAD && git rev-parse HEAD`);
  const [branch, baseSha] = base.stdout.trim().split("\n");
  if (base.code !== 0 && base.code !== 128 && !/not a git repository/.test(base.stderr)) return `error: ${base.stderr.trim()}`;
  if (base.code !== 0 || !branch || branch === "HEAD" || !baseSha) return `error: subagent needs a git branch checked out in ${WS} of ${P}`;

  const parent = await call("GET", `${p(P)}/tools`);
  if (parent.startsWith("error") || parent.startsWith("platform tools unavailable")) return parent;
  const ip = String(JSON.parse(parent).address ?? "").replace(/:\d+$/, "");
  const remote = `ssh://kl@${ip}${WS}`;

  // one cut per source at a time: a second clone of the same parent 409s until the first's cut lands
  let made = "";
  const created = await until(async () => {
    made = await call("POST", `${p(P)}/clone`, { name: `sub-${hex()}` });
    return made.startsWith("error 409:") && made.includes("already being cut") ? undefined : made;
  }, 60_000);
  if (created === null || created.startsWith("error") || created.startsWith("platform tools unavailable")) return created ?? made;
  const C = JSON.parse(created).id as string;

  let settled = false; // clone deleted, or deliberately kept
  let h: SessionHandle | undefined;
  try {
    const ready = await until(async () => {
      try {
        const w = await call("GET", p(C));
        if (w.startsWith("error") || JSON.parse(w).state !== "ready") return undefined;
        const t = await call("GET", `${p(C)}/tools`);
        if (t.startsWith("error")) return undefined;
        const j = JSON.parse(t);
        return j.address && j.token ? true : undefined;
      } catch {
        return undefined;
      }
    }, 180_000);
    if (!ready) {
      await del(C);
      settled = true;
      return `error: clone ${C} of ${P} did not become ready in 180s; deleted`;
    }

    h = await deps.open(`${C}:agent-${hex()}`, opts({ fresh: true }));
    const prompt = `${task}\n\nYou are in your own clone of the workspace; your code is in ${WS} on branch ${branch}. Commit your work there; do not push, the platform pushes it.`;
    const reply = await answer(h, () => h!.prompt(prompt));

    const commit = async () => {
      const r = await exec(C, commitCmd(task));
      if (r.code !== 0) throw new Error(`commit failed in clone ${C}: ${tail(r.stderr, 5)}`);
      return (await exec(C, `cd ${WS} && git rev-parse HEAD`)).stdout.trim();
    };
    const head = await commit();
    if (head === baseSha) {
      await del(C);
      settled = true;
      return `${reply}\n\nno code changes`;
    }
    // listed before any rebase so the parent's own newer commits do not pad it
    const files = (await exec(C, `cd ${WS} && git diff --name-only ${baseSha}..HEAD`)).stdout.trim();
    const push = () => exec(C, `cd ${WS} && git push ${sq(remote)} HEAD:${sq(branch)}`);

    let r = await push();
    if (r.code !== 0 && MOVED.test(r.stderr)) {
      await answer(h, () => h!.prompt(`The parent's branch moved. Run \`git pull --rebase ${remote} ${branch}\` in ${WS}, resolve any conflict, commit, and reply done.`));
      await commit();
      r = await push();
    }
    if (r.code !== 0) {
      settled = true;
      return `${reply}\n\npush failed: ${tail(r.stderr, 5)}; clone ${C} kept with the commits`;
    }
    const sha = (await exec(C, `cd ${WS} && git rev-parse --short HEAD`)).stdout.trim();
    await del(C);
    settled = true;
    return `${reply}\n\npushed ${sha} to ${branch} in ${P}:\n${files}`;
  } catch (e) {
    if (!settled) await del(C);
    throw e;
  } finally {
    await h?.dispose();
  }
}

export function delegateTools(kind: "main" | "workspace", ws: string | undefined, deps: DelegateDeps, caller: SessionOpts): ToolDef[] {
  // same model, same gate; nothing of the TUI's own tools goes along
  const opts = (extra: Partial<SessionOpts> = {}): SessionOpts => ({ ...caller, tools: [], ...extra });

  const subagent: ToolDef = {
    name: "subagent",
    description:
      "Run a self-contained code task (search, read, edit, run) in a fresh throwaway session inside a workspace and get its final answer; use it to keep bulk exploration out of your own context.",
    inputSchema: {
      type: "object",
      properties: { ...(kind === "main" ? { workspace: { type: "string" } } : {}), task: { type: "string" } },
      required: [...(kind === "main" ? ["workspace"] : []), "task"],
    },
    async run(input: { workspace?: string; task: string }) {
      const target = kind === "workspace" ? ws! : input.workspace;
      if (!target) return "error: workspace is required";
      return runInClone(target, input.task, deps, opts);
    },
  };
  if (kind === "workspace") return [subagent];

  const ask: ToolDef = {
    name: "workspace_ask",
    description:
      "Ask a workspace's own session to do something inside that workspace (edit code, run commands, answer about its files) and get its answer; the main session has no filesystem of its own.",
    inputSchema: { type: "object", properties: { workspace: { type: "string" }, request: { type: "string" } }, required: ["workspace", "request"] },
    async run(input: { workspace: string; request: string }) {
      const key = input.workspace;
      const text = `[from main session] ${input.request}`;
      const existing = deps.live.get(key);
      const h = existing ?? (await deps.open(key, opts()));
      try {
        return await answer(h, () => (existing && deps.busy.has(key) ? h.followUp(text) : h.prompt(text)));
      } finally {
        if (!existing) await h.dispose();
      }
    },
  };
  return [ask, subagent];
}
