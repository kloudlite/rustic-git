//! Tools that start another session: `workspace_ask` (main hands a workspace's own session a goal;
//! fire-and-forget, the answer comes back later as a `[from <ws>] ...` message prompted, or followed
//! up when busy, into the CALLER's session so main never blocks) and `subagent` (a throwaway session
//! with the workspace's own tools; a workspace session's tool only, it blocks and the session relies on
//! its answer). Main never gets `subagent`: it hands goals to workspaces with `workspace_ask`.
//! `subagent` never works in the parent: it clones the parent workspace (a btrfs worktree, so cheap),
//! runs in the clone, commits there and pushes straight into the parent's checked-out branch over
//! SSH (the parent's home seed sets receive.denyCurrentBranch=updateInstead; the owner's platform
//! key is in the clone's pod). A moved parent branch gets ONE rebase round through the same child
//! session; a push that still fails keeps the clone so the commits are not lost. The clone is
//! deleted on every other exit. Needs the bench/main token: a workspace token cannot clone.
//! Delegated sessions ask through `deps.permit`, which routes to whichever client is connected to the
//! CALLER's session now (clients.ts), but each request names the delegated session
//! (`PermissionRequest.session`), so the TUI shows the card in that workspace's view.
//! The answer is the last assistant text seen before `agent_end`: Claude sessions emit
//! `agent_end` with an empty message list, so the events are tracked instead. It only counts after
//! the asked user message was seen (a busy Claude ends its CURRENT run before the followUp runs).
//! A `workspace_ask` is saved to disk (asks.ts) while it is in flight and resent after a bench
//! restart (`resumeAsks`), at most twice.
import { randomBytes } from "node:crypto";
import { api, podExec, type ExecResult, type ToolDef } from "@kloudlite-tui/tools";
import type { PermissionRequest, Decision, SessionHandle, SessionOpts } from "./index.ts";
import { forgetSessions } from "./forget.ts";
import { asksDir, dropAsk, listAsks, saveAsk, type PendingAsk } from "./asks.ts";

export type DelegateDeps = {
  /** Open sessions by key (LocalBackend's map). */
  live: Map<string, SessionHandle>;
  /** Keys mid-turn right now. */
  busy: Set<string>;
  open(key: string, opts: SessionOpts): Promise<SessionHandle>;
  /** Ask the client connected to `key` (the caller's session) for a decision; waits for one. */
  permit(key: string, req: PermissionRequest, signal: AbortSignal): Promise<Decision>;
  /** Where pending asks live; tests pass a temp dir. */
  asks?: string;
  /** Platform API (tools' `api`) and pod exec; injectable so tests run on fakes. */
  api?: (method: string, path: string, body?: unknown) => Promise<string>;
  exec?: (ws: string, cmd: string, timeoutMs?: number) => Promise<ExecResult>;
  /** Waits between polls; tests pass a no-op (the caps count slept time, not wall time). */
  sleep?: (ms: number) => Promise<void>;
  /** Drops a deleted workspace's bench sessions; tests pass a fake. */
  forget?: (ws: string) => Promise<void>;
};

/** A workspace pod's source folder (crates/workspaces/src/k8s/mod.rs WORKSPACE_DIR). */
export const WORKSPACE_DIR = "/home/kl/workspace";
const WS = WORKSPACE_DIR;
const sq = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
const hex = () => randomBytes(4).toString("hex");
const tail = (s: string, n: number) => s.trim().split("\n").slice(-n).join("\n");
const MOVED = /non-fast-forward|fetch first/;

/** One commit of whatever the child left. The -c fallbacks apply only when the pod has no git
 * identity (the directory knows no name for the handle); otherwise the person's own identity wins. */
const commitCmd = (task: string) =>
  `cd ${WS} && git add -A && (git diff --cached --quiet || { n=$(git config user.name); e=$(git config user.email); ` +
  `git -c user.name="\${n:-kl subagent}" -c user.email="\${e:-subagent@kloudlite.local}" commit -q -m ${sq(task.split("\n")[0]!.slice(0, 72) || "subagent changes")}; })`;

/** `git init -b main` unless already a repo, then an empty first commit so there is a branch and a
 * HEAD to clone from and merge back onto. Same identity fallbacks as `commitCmd`. */
const startCmd =
  `cd ${WS} && (git rev-parse --git-dir >/dev/null 2>&1 || git init -q -b main) && n=$(git config user.name); e=$(git config user.email); ` +
  `git -c user.name="\${n:-kl subagent}" -c user.email="\${e:-subagent@kloudlite.local}" commit -q --allow-empty -m 'Start workspace'`;

const textOf = (m: any): string =>
  (typeof m?.content === "string" ? [{ type: "text", text: m.content }] : Array.isArray(m?.content) ? m.content : [])
    .filter((b: any) => b?.type === "text")
    .map((b: any) => b.text)
    .join("\n")
    .trim();

/** Send `text`, then resolve at the `agent_end` of the run that answered IT. Armed only by the user
 * message carrying `text`: a busy Claude emits `agent_end` for its CURRENT run first and only then
 * runs a followUp, so an earlier end would hand back the wrong turn's text. A disposed session
 * (`session_closed`) never ends a turn, so it resolves too. */
function answer(h: SessionHandle, text: string, send: () => Promise<void>): Promise<string> {
  return new Promise((resolve, reject) => {
    let last = "";
    let armed = false;
    const want = text.trim();
    const off = h.subscribe((e: any) => {
      if (e.type === "session_closed") {
        off();
        resolve("(session closed before answering)");
      } else if (!armed) {
        armed = e.type === "message_end" && e.message?.role === "user" && textOf(e.message).includes(want);
      } else if (e.type === "message_end" && e.message?.role === "assistant") last = textOf(e.message) || last;
      else if (e.type === "agent_end") {
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

async function runInClone(P: string, task: string, deps: DelegateDeps, opts: (key: string, e?: Partial<SessionOpts>) => SessionOpts): Promise<string> {
  // api throws on a non-2xx answer; this flow branches on the status text ("error 409: … already being cut")
  const call = (m: string, path: string, body?: unknown) =>
    (deps.api ?? api)(m, path, body).catch((e: any) => `error ${e?.message ?? e}`);
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
    if (!r.startsWith("error") && !r.startsWith("platform tools unavailable")) {
      await forget(id);
    }
  };

  const HEAD = `cd ${WS} && git rev-parse --abbrev-ref HEAD && git rev-parse HEAD`;
  let base = await exec(P, HEAD);
  // A workspace made without a repo has no git yet, or an unborn branch: start one here rather
  // than refuse (its pod start does the same since then; this covers the workspaces made before)
  if (base.code === 128 || /not a git repository/.test(base.stderr)) {
    const s = await exec(P, startCmd);
    if (s.code !== 0) return `error: could not start a git repo in ${WS} of ${P}: ${tail(s.stderr, 5)}`;
    base = await exec(P, HEAD);
  }
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
    made = await call("POST", `${p(P)}/clone`, { name: `sub-${hex()}`, task: task.split("\n")[0]!.slice(0, 72) });
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

    const sk = `${C}:agent-${hex()}`;
    h = await deps.open(sk, opts(sk, { fresh: true }));
    const prompt = `${task}\n\nYou are in your own clone of the workspace; your code is in ${WS} on branch ${branch}. You own how it is built. Commit your work there and do not push; the platform pushes it to the workspace with git when you finish. Your last message is your report to the workspace session: what you did, how you checked it, anything it must decide. Work only in ${WS}: never reach another workspace, pod or session.`;
    const reply = await answer(h, prompt, () => h!.prompt(prompt));

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
      const rebase = `The parent's branch moved. Run \`git pull --rebase ${remote} ${branch}\` in ${WS}, resolve any conflict, commit, and reply done.`;
      await answer(h, rebase, () => h!.prompt(rebase));
      await commit();
      r = await push();
    }
    if (r.code !== 0) {
      settled = true;
      return `${reply}\n\npush failed: ${tail(r.stderr, 5)}; clone ${C} kept with the commits. Report this failure as it is; do not work around it by fetching, copying or asking another workspace to pull.`;
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

/** What a workspace opened for an ask gets: the caller's model and settings, none of the TUI's own
 * tools, and cards routed to whoever is connected to the CALLER's session (named for the workspace). */
function askOpts(deps: DelegateDeps, callerKey: string, a: PendingAsk, key: string): SessionOpts {
  return {
    model: a.model,
    thinkingLevel: a.thinkingLevel,
    autoCompact: a.autoCompact,
    codemode: a.codemode,
    tools: [],
    permission: (req, s) => deps.permit(callerKey, { ...req, session: req.session ?? key }, s),
  };
}

/** Hand `reply` to the caller's session: a view of the live one, or (after a bench restart, when
 * nobody has it open) a freshly opened one. Prompt when idle, followUp when busy; main can start a
 * turn between the check and the call, so a refused prompt retries as a followUp instead of losing
 * the reply. Not awaited: a prompt lasts the caller's whole turn. */
async function deliver(deps: DelegateDeps, a: PendingAsk, reply: string): Promise<void> {
  try {
    const c = await deps.open(a.callerKey, { model: a.model, thinkingLevel: a.thinkingLevel, autoCompact: a.autoCompact, codemode: a.codemode, tools: [] });
    void (deps.busy.has(a.callerKey) ? c.followUp(reply) : c.prompt(reply))
      .catch(() => c.followUp(reply))
      .catch((e) => console.error("workspace_ask reply lost", a.callerKey, a.key, e))
      .finally(() => void c.dispose());
  } catch (e) {
    console.error("workspace_ask reply lost", a.callerKey, a.key, e);
  }
}

/** Run one ask to its delivery. Saved BEFORE the workspace is opened and dropped right before the
 * reply is delivered: a restart between the drop and the caller's session file loses one reply
 * (accepted); a restart before it resends the ask. */
export async function dispatchAsk(deps: DelegateDeps, a: PendingAsk): Promise<void> {
  const dir = deps.asks ?? asksDir();
  saveAsk(dir, a);
  let h: SessionHandle | undefined;
  let reply: string;
  try {
    h = await deps.open(a.key, askOpts(deps, a.callerKey, a, a.key));
    const s = h;
    reply = `[from ${a.key}] ${await answer(s, a.text, () => (deps.busy.has(a.key) ? s.followUp(a.text) : s.prompt(a.text)))}`;
  } catch (err) {
    reply = `[from ${a.key}] failed: ${err instanceof Error ? err.message : String(err)}`;
  }
  dropAsk(dir, a.id);
  await deliver(deps, a, reply);
  // our own view goes only after its answer was delivered
  if (h) await h.dispose().catch(() => {});
}

/** Boot: resend what a restart interrupted. `tries` counts resends; the third restart reports the
 * loss to the caller instead. Returns the in-flight dispatches (the daemon does not await them). */
export function resumeAsks(deps: DelegateDeps): Promise<void>[] {
  const dir = deps.asks ?? asksDir();
  return listAsks(dir).map(async (a) => {
    if (a.tries >= 2) {
      dropAsk(dir, a.id);
      return deliver(deps, a, `[from ${a.key}] failed: lost in ${a.tries + 1} bench restarts; ask again if it still matters`);
    }
    const text = a.text.startsWith("[resent after restart] ") ? a.text : `[resent after restart] ${a.text}`;
    return dispatchAsk(deps, { ...a, tries: a.tries + 1, text });
  });
}

export function delegateTools(kind: "main" | "workspace", ws: string | undefined, deps: DelegateDeps, caller: SessionOpts, callerKey = "main"): ToolDef[] {
  // same model, same gate; nothing of the TUI's own tools goes along
  // the request carries the delegated session's key so the TUI files the card under that workspace
  const opts = (key: string, extra: Partial<SessionOpts> = {}): SessionOpts => ({
    ...caller,
    tools: [],
    permission: (req, s) => deps.permit(callerKey, { ...req, session: req.session ?? key }, s),
    ...extra,
  });

  // ponytail: a bench restart mid-subagent leaves its `sub-*` clone (it may hold the work) and the
  // caller sees "Tool call was interrupted"; upgrade = persist the clone id and report it on boot.
  const subagent: ToolDef = {
    name: "subagent",
    description:
      "Hand planned work (a feature, a refactor, a multi-step fix) to a throwaway subagent in its own clone of this workspace. Commit your own work first. The task must stand alone: the subagent has none of your conversation. Blocks until it ends; the platform then pushes its commits into your checked-out branch with git and deletes the clone. Returns its answer plus `pushed <sha> ...`, `no code changes`, or `push failed ...` (report that as it is).",
    inputSchema: {
      type: "object",
      properties: { task: { type: "string" } },
      required: ["task"],
    },
    async run(input: { task: string }) {
      return runInClone(ws!, input.task, deps, opts);
    },
  };
  if (kind === "workspace") return [subagent];

  const ask: ToolDef = {
    name: "workspace_ask",
    description:
      "Hand a workspace's own session a goal: the person's words plus context only you have (environment, decisions, facts from another workspace's answer), never paths, libraries or steps. Returns at once; the workspace's answer arrives later as a `[from <ws>] ...` message. Never wait or poll for it. The only way main gets work done in a workspace.",
    inputSchema: { type: "object", properties: { workspace: { type: "string" }, request: { type: "string" } }, required: ["workspace", "request"] },
    async run(input: { workspace: string; request: string }) {
      const key = input.workspace;
      // Not awaited: main must stay free for the person while the workspace works.
      void dispatchAsk(deps, {
        id: hex(),
        callerKey,
        key,
        text: `[from main session] ${input.request}`,
        tries: 0,
        model: caller.model,
        codemode: caller.codemode,
        thinkingLevel: caller.thinkingLevel,
        autoCompact: caller.autoCompact,
      });
      return `sent to ${key}; its session is working on it. Its answer will arrive here as a message from ${key}; do not wait or poll for it.`;
    },
  };
  return [ask];
}
