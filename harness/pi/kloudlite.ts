import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { TOOLS } from "./catalog.ts";

/**
 * The bench's hands on the platform: `/v1` as tools. Authentication is the
 * person's own: a short-lived platform token the desktop app mints and the
 * platform projects into this pod at `KL_TOOL_TOKEN_FILE`, re-read on every
 * call so a refresh lands without a restart and a revoked session stops at the
 * next call. Nothing here holds a credential of its own, and every write and
 * delete is named as such in the catalogue so a person can decide what the model may do.
 */
const SIGN_IN = "sign in on the Kloudlite desktop app";

function token(): { api: string; token: string } {
  const f = process.env.KL_TOOL_TOKEN_FILE;
  const api = process.env.KL_API_URL;
  let t = "";
  try {
    t = f ? fs.readFileSync(f, "utf8").trim() : "";
  } catch {
    /* unreadable is the same as absent */
  }
  if (!t || !api) throw new Error(SIGN_IN);
  return { api: api.replace(/\/$/, ""), token: t };
}

export async function call(method: string, p: string, body?: unknown): Promise<{ status: number; data: unknown }> {
  const c = token();
  const r = await fetch(`${c.api}${p}`, {
    method,
    redirect: "error",
    headers: { authorization: `Bearer ${c.token}`, ...(body !== undefined ? { "content-type": "application/json" } : {}) },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await r.text();
  if (r.status === 401) return { status: 401, data: `${SIGN_IN} (your desktop session ended or the bench was stopped)` };
  // The api answers JSON; a page of HTML means the request never reached it
  // (an unpublished route, a redirect to sign in) — say that, not the page.
  if (/^\s*<!doctype html|^\s*<html/i.test(text)) return { status: r.status >= 400 ? r.status : 502, data: `not an api answer for ${p} — the route is not published on ${c.api} (got a web page)` };
  let data: unknown = text;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    /* not json: the text is the answer (a 403's sentence among them) */
  }
  return { status: r.status, data };
}

const text = (v: unknown) => ({ content: [{ type: "text" as const, text: typeof v === "string" ? v : JSON.stringify(v, null, 2) }] });
const answer = async (method: string, p: string, body?: unknown) => {
  const { status, data } = await call(method, p, body);
  if (status >= 400) return { ...text(`${status}: ${typeof data === "string" ? data : JSON.stringify(data)}`), isError: true };
  return text(data ?? `${status} ok`);
};
/**
 * A service as `/v1` takes it. `command`, `env` and `mounts` have no serde default on the api
 * side, so an omitted one is a 422 rather than an empty list — `service()` fills them in.
 */
const SERVICE = Type.Object({
  name: Type.String(),
  image: Type.String(),
  command: Type.Optional(Type.Array(Type.String(), { description: "overrides the image's entrypoint command" })),
  env: Type.Optional(Type.Record(Type.String(), Type.String(), { description: "environment variables" })),
  mounts: Type.Optional(Type.Array(Type.Object({ folder: Type.String({ description: "one safe segment of the environment's own volume" }), path: Type.String({ description: "where it is mounted in the container" }) }))),
  ports: Type.Optional(Type.Array(Type.Number(), { description: "container ports siblings reach it on by name" })),
});
const service = (s: Record<string, any>) => ({ name: s.name, image: s.image, command: s.command ?? [], env: s.env ?? {}, mounts: s.mounts ?? [], ports: s.ports ?? [] });

const q = (o: Record<string, string | undefined>) => {
  const s = new URLSearchParams(Object.entries(o).filter(([, v]) => v) as [string, string][]).toString();
  return s ? `?${s}` : "";
};

/**
 * What the model is: the Kloudlite harness, and nothing else. This REPLACES pi's
 * own system prompt rather than appending to it (owner, 2026-09-17): a bench
 * session has no filesystem and no shell of its own, so a coding-agent prompt
 * about local files, the CLI it happens to be built on, or paths under
 * /opt/harness describes a machine it cannot touch and invites it to go looking.
 * Its tools are the whole world it sees — which is also why it is told not to try them out: a
 * model with no filesystem answers "what can you do?" by calling something, and every kl_* write
 * lands on the person's real workspaces.
 */
/**
 * The caveman compression rules, vendored beside this file. Read once at load: the extension dir is
 * wherever the harness tree was installed, and a missing file is a prompt without the style rather
 * than an extension that will not start.
 */
function caveman(): string[] {
  try {
    const at = path.join(path.dirname(fileURLToPath(import.meta.url)), "caveman.md");
    const body = fs.readFileSync(at, "utf8").trim();
    return body ? ["Speak in the caveman style below. Chat text only; code, files and commits stay normal prose.", body] : [];
  } catch {
    return [];
  }
}
const CAVEMAN = caveman();

export function identity(hands: string): string {
  return [
    "You are the Kloudlite harness: the person's bench on the Kloudlite platform.",
    hands,
    "Those tools are the only way you can see or change anything. Never try to reach the platform another way, and never guess at what a tool would have told you.",
    // A bench model asked what tools it had ran kl_environment_service_rm to find out (2026-09-17,
    // harmless only because that service did not exist). Each tool's description ends with its
    // effect — [read], [write], [destroy] — so the rule can be stated in those terms.
    "Never call a tool whose effect is write or destroy unless the person asked for that change in this conversation. When asked what you can do, answer from kl_capabilities and describe the tools by name; do not run them to find out.",
    "When nothing you have does what was asked, say so and stop. Never go behind the tools for it — not the harness's own files, not the kl binary, not your session logs, not a token and a hand-made request. There is nothing there for you, and looking is refused.",
    // The owner, 2026-09-17: "I should see things very clearly." A model that narrates its plan
    // buries the one line that matters — the id, the port, the error.
    // The desktop draws every kl_* result as a card (spec §8); saying the same fields again is
    // the same screen twice, and the one line the person wanted is then buried in the middle.
    "The person sees every tool result rendered; never repeat its fields. Your text is one line: what happened, or what you need.",
    "Answer short. Lead with the result in one line. Then only the facts the person needs, one line each — ids, paths, ports, errors verbatim. Never restate the request, the plan, or the text of an ask you sent. No headings, tables, emoji, or closing offers unless asked. When you queued an ask, say so in one line and stop.",
    ...CAVEMAN,
  ].join("\n\n");
}

/** pi's `before_agent_start` hook hands back the system prompt for the turn; returning our own replaces it. */
export function tellItWhereItStands(pi: ExtensionAPI, hands: string): void {
  const prompt = identity(hands);
  pi.on("before_agent_start", async () => ({ systemPrompt: prompt }));
}

/**
 * The hands a bench session has. Two rules the owner set (2026-09-17), both here
 * because the model cannot read them anywhere else: what it IS is a machine of
 * its own, so "install X" with nothing named is about itself; and another
 * workspace is ASKED, never driven — the request is queued into that
 * workspace's own session, which has its own hands and its own tab.
 */
export const BENCH_HANDS = [
  "You are yourself a workspace on the platform: a machine with its own files, its own shell, its own packages and its own environment. read, write, edit, bash, grep, find and ls all run THERE — in your own workspace, never on the machine this conversation runs on, and never in any other workspace. \"Install X\", \"add a package\", \"switch the environment\" with no workspace named mean YOURS — kl_pkg_list, kl_pkg_add, kl_pkg_rm, kl_pkg_update, kl_env_current, kl_env_switch, kl_env_clear.",
  "You never change another workspace yourself. To have work done in one, use kl_workspace_ask with the workspace id and the request in plain words: it is queued into that workspace's own session, which does the work there and answers back to you. Say that you asked, and go on; the answer arrives as a message.",
  "A new component — a backend, a service, a separate project — gets its OWN workspace (kl_workspace_create, then kl_workspace_ask), unless the person names an existing workspace to put it in. Never add an unrelated component to a workspace because the code you touched last lives there.",
  "To see how a workspace is getting on, use kl_workspace_progress.",
  "The platform itself — workspaces, environments, volumes, quota, regions, requests — is reached only through the kl_* tools.",
].join("\n\n");

/**
 * Registering one tool from the catalogue: the description a person reads is the
 * description the model gets, and a call that hands work to a workspace or an
 * environment is published as an exchange, which is what the workspace's own
 * queue and the session's queue both read.
 */
export function makeReg(pi: ExtensionAPI) {
  const spec = (name: string) => TOOLS.find((t) => t.name === name)!;
  const names: string[] = [];
  const reg = <P extends Parameters<typeof Type.Object>[0]>(name: string, params: P, run: (a: Record<string, any>, signal?: AbortSignal) => Promise<{ content: { type: "text"; text: string }[]; isError?: boolean }>) => {
    const s = spec(name);
    names.push(name);
    pi.registerTool({
      name,
      label: name,
      description: `${s.summary} [${s.effect}]`,
      parameters: Type.Object(params),
      async execute(toolCallId, a, signal, _update, ctx) {
        const args = a as Record<string, any>;
        // A call that hands work to a workspace or environment is an exchange:
        // harness-bench records it in the bench's one log, where the session's
        // queue and the workspace's queue both read it.
        const target = /^kl_(workspace|environment)_/.test(name) ? String(args.workspace ?? args.id ?? args.name ?? "") : "";
        const publish = (v: unknown) => target && ctx?.ui?.setWidget("harness:exchange", [JSON.stringify(v)]);
        const id = `x-${toolCallId}`;
        publish({ id, workspace: target, dir: "out", text: `${name} ${JSON.stringify(args)}`, state: "sent" });
        const r = await run(args, signal);
        publish({ id: `${id}-in`, workspace: target, dir: "in", text: r.content.map((c) => c.text).join("").slice(0, 2000), state: r.isError ? "failed" : "done", ref: id });
        publish({ id, state: r.isError ? "failed" : "done" });
        return r;
      },
    });
  };
  // What this session can do, for `kl_capabilities` to read back: a bench session and a workspace
  // session register different sets, and answering with the other's would be a list of lies.
  return Object.assign(reg, { names });
}

/**
 * The answer to "what can you do here?" — which a model with no matching tool otherwise goes
 * looking for, in the extension's own source or in the `kl` binary (the fleet, 2026-09-17).
 * Registered last, so it names everything else this session holds.
 */
export function capabilities(reg: ReturnType<typeof makeReg>) {
  reg("kl_capabilities", {}, async () => {
    const mine = TOOLS.filter((t) => reg.names.includes(t.name));
    const groups = ["workspace", "environment", "platform"] as const;
    const lines = groups.flatMap((g) => {
      const rows = mine.filter((t) => t.group === g);
      return rows.length ? [`${g}:`, ...rows.map((t) => `  ${t.name} [${t.effect}] — ${t.summary}`)] : [];
    });
    return text(
      [
        "this machine (its own files and shell, nowhere else):",
        "  read, write, edit, bash (background: true for a long-running one), process, grep, find, ls [write where they change a file]",
        ...lines,
        "anything not listed is not something you can do — say so.",
      ].join("\n"),
    );
  });
}

/** Where harness-bench listens for its own extension; a test points this elsewhere. */
const BENCH_URL = () => process.env.KL_BENCH_URL ?? "http://127.0.0.1:7789";
const benchCall = async (method: string, p: string, body?: unknown): Promise<{ ok: boolean; data: any }> => {
  const r = await fetch(`${BENCH_URL()}${p}`, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { ok: r.ok, data: await r.json().catch(() => ({ error: `the bench answered ${r.status}` })) };
};

/**
 * How a workspace is getting on, without going to look. A model with no tool for this grepped the
 * bench's own `.bench/workspaces/*\/thread.jsonl` off disk (the fleet, 2026-09-17) — refused now,
 * so here is the answer instead: what has been asked of it and what its session has been saying.
 * Both modes have it: a workspace session may have asked something of another one too.
 */
export function progressTool(reg: ReturnType<typeof makeReg>) {
  reg("kl_workspace_progress", { workspace: Type.String({ description: "workspace id" }) }, async (a) => {
    const id = encodeURIComponent(a.workspace);
    const [x, m] = await Promise.all([benchCall("GET", `/exchanges?workspace=${id}`), benchCall("GET", `/workspaces/${id}/messages?limit=10`)]);
    if (!x.ok || !m.ok) return { ...text(String((x.ok ? m.data : x.data)?.error ?? "the bench could not be asked"), true), isError: true };
    const asks = (x.data as { dir: string; state: string; text: string }[]).filter((e) => e.dir === "out").map((e) => `  ${e.state}: ${String(e.text).replace(/^\[ask \S+ from [^\]]*\] /, "").slice(0, 160)}`);
    const said = ((m.data as { messages?: Record<string, any>[] }).messages ?? []).slice(-10).flatMap((r) => {
      const c = r.content;
      if (r.role === "user") return [`  asked: ${(typeof c === "string" ? c : (c ?? []).map((b: any) => b.text ?? "").join("")).slice(0, 160)}`];
      if (r.role !== "assistant") return [];
      // A tool call is what it is DOING; the prose is what it thinks about it. Both, briefly.
      return (c as any[] ?? []).map((b) => (b.type === "toolCall" ? `  ran ${b.name}` : b.text ? `  said: ${String(b.text).slice(0, 160)}` : "")).filter(Boolean);
    });
    return text([`asked of ${a.workspace}:`, ...(asks.length ? asks : ["  nothing outstanding"]), `its session, latest last:`, ...(said.length ? said : ["  nothing yet"])].join("\n"));
  });
}

/**
 * The machine this session IS: the bench's own workspace, or, in a workspace
 * session, that workspace. Packages are read-modify-write against `/v1` because
 * PATCH takes the WHOLE list — "add nats" has to keep what is already there.
 */
export function ownTools(pi: ExtensionAPI, own: string, space: string | undefined, reg = makeReg(pi)) {
  const packages = async (): Promise<string[]> => {
    const { status, data } = await call("GET", `/v1/workspaces/${encodeURIComponent(own)}`);
    if (status >= 400) throw new Error(`${status}: ${typeof data === "string" ? data : JSON.stringify(data)}`);
    return ((data as { packages?: string[] } | null)?.packages ?? []).slice();
  };
  // A pin is `attr@version`: matching on the attr alone is what lets "remove nodejs" take `nodejs@20`.
  const attr = (e: string) => e.split("@")[0];
  const setPackages = async (next: string[]) => answer("PATCH", `/v1/workspaces/${encodeURIComponent(own)}`, { packages: next });
  const P = Type.Array(Type.String(), { description: "packages: attr or attr@version" });
  reg("kl_pkg_list", {}, async () => text(await packages()));
  reg("kl_pkg_add", { packages: P }, async (a) => {
    const have = await packages();
    // A re-pin replaces the entry it pins rather than sitting beside it.
    const next = [...have.filter((e) => !a.packages.some((x: string) => attr(x) === attr(e))), ...a.packages];
    return setPackages(next);
  });
  reg("kl_pkg_rm", { packages: P }, async (a) => {
    const have = await packages();
    const next = have.filter((e) => !a.packages.some((x: string) => attr(x) === attr(e)));
    if (next.length === have.length) return { ...text(`none of ${a.packages.join(", ")} is installed here`), isError: true };
    return setPackages(next);
  });
  reg("kl_pkg_update", {}, () => answer("POST", `/v1/workspaces/${encodeURIComponent(own)}/packages/update`));
  if (!space) return;
  // A person's space follows ONE environment; this machine and every workspace in it resolve its services by bare name.
  reg("kl_env_current", {}, () => answer("GET", "/v1/me/environments"));
  reg("kl_env_switch", { environment: Type.String({ description: "environment id owned by this space" }) }, (a) => answer("PUT", `/v1/me/environments/${encodeURIComponent(space)}`, { environment: a.environment }));
  reg("kl_env_clear", {}, () => answer("DELETE", `/v1/me/environments/${encodeURIComponent(space)}`));
}

/**
 * The space's environment. Every session that lives in a space may manage it — the bench and every
 * workspace session alike (owner, 2026-09-17): the person debugging in a workspace is the person
 * who needs a service added or its traffic pointed at them, and making them walk back to the bench
 * for it is the same "no tool for this" that sent a model reading extension source.
 *
 * Creating, stopping, cloning, restoring and deleting an environment stay with the bench: those
 * are about the space, not about the work in front of one workspace.
 */
function environmentTools(reg: ReturnType<typeof makeReg>) {
  const S = (d: string) => Type.String({ description: d });
  const O = <T>(t: T) => Type.Optional(t as any);
  reg("kl_environments", { owner: O(S("owner slug to list for")) }, (a) => answer("GET", `/v1/environments${q({ owner: a.owner })}`));
  reg("kl_environment", { id: S("environment id") }, (a) => answer("GET", `/v1/environments/${a.id}`));
  reg(
    "kl_intercept",
    {
      id: S("environment id"),
      service: S("service name in the environment"),
      workspace: O(S("workspace id to deliver to; absent = clear the intercept")),
      ports: O(Type.Array(Type.Object({ from: Type.Number(), to: Type.Number() }), { description: "port remaps: service port → workspace port" })),
    },
    (a) => (a.workspace ? answer("POST", `/v1/environments/${a.id}/intercepts`, { service: a.service, workspace: a.workspace, ports: a.ports }) : answer("DELETE", `/v1/environments/${a.id}/intercepts/${a.service}`)),
  );
  // PATCH takes the WHOLE list, so both of these read the environment first and pass every service
  // they are not changing through VERBATIM. A tool that took "the list" and rebuilt each row from
  // a narrower schema would silently drop a service's command, env or mounts — the model cannot
  // see what it did not ask for. Removals take their StatefulSet with them; bytes stay on the volume.
  const services = async (id: string): Promise<Record<string, any>[]> => {
    const { status, data } = await call("GET", `/v1/environments/${encodeURIComponent(id)}`);
    if (status >= 400) throw new Error(`${status}: ${typeof data === "string" ? data : JSON.stringify(data)}`);
    return ((data as { services?: Record<string, any>[] } | null)?.services ?? []).slice();
  };
  const withServices = (id: string, next: Record<string, any>[]) => answer("PATCH", `/v1/environments/${encodeURIComponent(id)}`, { services: next });
  reg("kl_environment_service_add", { id: S("environment id"), service: SERVICE }, async (a) => {
    const have = await services(a.id);
    const one = service(a.service);
    return withServices(a.id, [...have.filter((s) => s.name !== one.name), one]);
  });
  reg("kl_environment_service_rm", { id: S("environment id"), name: S("the service to remove") }, async (a) => {
    const have = await services(a.id);
    const next = have.filter((s) => s.name !== a.name);
    if (next.length === have.length) return { ...text(`${a.id} has no service ${a.name}`), isError: true };
    return withServices(a.id, next);
  });
}

export function tools(pi: ExtensionAPI) {
  const reg = makeReg(pi);
  const S = (d: string) => Type.String({ description: d });
  const O = <T>(t: T) => Type.Optional(t as any);

  // workspaces. Another workspace is ASKED, never driven: the request goes to the bench's own
  // server, which queues it into that workspace's session — the one place with hands there.
  reg("kl_workspace_ask", { workspace: S("workspace id"), request: S("what to do there, in plain words") }, async (a) => {
    const r = await fetch(`${BENCH_URL()}/workspaces/${encodeURIComponent(a.workspace)}/ask`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ text: a.request, from: process.env.KL_SESSION }),
    });
    const d = (await r.json().catch(() => ({}))) as { error?: string };
    if (!r.ok) return { ...text(d.error ?? `the bench answered ${r.status}`), isError: true };
    return text(`queued in ${a.workspace}'s session; its reply arrives here`);
  });
  reg("kl_workspaces", { team: O(S("team slug; absent = personal")) }, (a) => answer("GET", `/v1/workspaces${q({ team: a.team })}`));
  reg("kl_workspace", { id: S("workspace id") }, (a) => answer("GET", `/v1/workspaces/${a.id}`));
  reg(
    "kl_workspace_create",
    {
      name: S("workspace name"),
      region: S("region id"),
      team: O(S("team slug; absent = personal")),
      repo: O(S("repository to seed from, e.g. kloudlite/rustic-git")),
      branch: O(S("branch to check out")),
      quota_gb: O(Type.Number({ description: "disk quota in GB (default 20)" })),
      packages: O(Type.Array(Type.String(), { description: "packages: attr or attr@version" })),
    },
    (a) => answer("POST", "/v1/workspaces", { name: a.name, region: a.region, team: a.team, repo: a.repo, branch: a.branch, quota_gb: a.quota_gb ?? 20, packages: a.packages }),
  );
  reg("kl_workspace_start", { id: S("workspace id") }, (a) => answer("POST", `/v1/workspaces/${a.id}/start`));
  reg("kl_workspace_stop", { id: S("workspace id") }, (a) => answer("POST", `/v1/workspaces/${a.id}/stop`));
  reg("kl_workspace_push", { id: S("workspace id"), message: O(S("what this snapshot is")) }, (a) => answer("POST", `/v1/workspaces/${a.id}/push`, { message: a.message }));
  reg("kl_workspace_clone", { id: S("source workspace id"), name: S("name for the clone") }, (a) => answer("POST", `/v1/workspaces/${a.id}/clone`, { name: a.name }));
  reg("kl_workspace_packages_update", { id: S("workspace id") }, (a) => answer("POST", `/v1/workspaces/${a.id}/packages/update`));
  reg(
    "kl_workspace_restore",
    { name: S("name for the restored workspace"), snapshot_id: S("snapshot id to restore"), image: O(S("override the snapshot's image")), packages: O(Type.Array(Type.String(), { description: "override the snapshot's packages" })), quota_gb: O(Type.Number({ description: "override the snapshot's disk quota" })) },
    (a) => answer("POST", "/v1/workspaces/restore", { name: a.name, snapshot_id: a.snapshot_id, image: a.image, packages: a.packages, quota_gb: a.quota_gb }),
  );
  reg("kl_workspace_delete", { id: S("workspace id") }, (a) => answer("DELETE", `/v1/workspaces/${a.id}`));

  // environments
  environmentTools(reg);
  reg(
    "kl_environment_create",
    { name: S("environment name"), region: S("region id"), owner: O(S("team slug; absent = personal")), services: Type.Array(SERVICE, { description: "services to run" }) },
    (a) => answer("POST", "/v1/environments", { name: a.name, region: a.region, owner: a.owner, services: a.services.map(service) }),
  );
  reg("kl_environment_start", { id: S("environment id") }, (a) => answer("POST", `/v1/environments/${a.id}/start`));
  reg("kl_environment_stop", { id: S("environment id") }, (a) => answer("POST", `/v1/environments/${a.id}/stop`));
  reg("kl_environment_push", { id: S("environment id"), message: O(S("what this snapshot is")) }, (a) => answer("POST", `/v1/environments/${a.id}/push`, { message: a.message }));
  reg("kl_environment_clone", { id: S("source environment id"), name: S("name for the clone") }, (a) => answer("POST", `/v1/environments/${a.id}/clone`, { name: a.name }));
  reg(
    "kl_environment_restore",
    { name: S("name for the restored environment"), snapshot_id: S("snapshot id to restore"), owner: O(S("team slug; absent = personal")), region: O(S("region to run in")), services: O(Type.Array(SERVICE, { description: "override the services the snapshot froze" })) },
    (a) => answer("POST", "/v1/environments/restore", { name: a.name, snapshot_id: a.snapshot_id, owner: a.owner, region: a.region, services: a.services?.map(service) }),
  );
  reg("kl_environment_restore_in_place", { id: S("environment id"), snapshot_id: S("snapshot id of this environment's own volume") }, (a) => answer("POST", `/v1/environments/${a.id}/restore-in-place`, { snapshot_id: a.snapshot_id }));
  reg("kl_environment_delete", { id: S("environment id") }, (a) => answer("DELETE", `/v1/environments/${a.id}`));

  // platform
  reg("kl_regions", {}, () => answer("GET", "/v1/regions"));
  reg("kl_quota", {}, () => answer("GET", "/v1/quota"));
  reg("kl_volumes", { name: O(S("a volume name, for its history")) }, (a) => answer("GET", a.name ? `/v1/volumes/${a.name}/history` : "/v1/volumes"));
  reg("kl_builder", {}, () => answer("GET", "/v1/builders/me"));
  reg("kl_volume_history", { name: S("volume name") }, (a) => answer("GET", `/v1/volumes/${a.name}/history`));
  reg("kl_volume_delete", { name: S("volume name; it must be detached") }, (a) => answer("DELETE", `/v1/volumes/${a.name}`));
  reg("kl_requests", {}, () => answer("GET", "/v1/requests"));
  // Anything that has to be GRANTED is a request; one pending per owner per kind.
  reg(
    "kl_request_create",
    {
      kind: Type.Union([Type.Literal("quota"), Type.Literal("access"), Type.Literal("region"), Type.Literal("other")], { description: "what is being asked for" }),
      reason: O(S("why, in the person's words")),
      owner: O(S("team slug; absent = your own")),
      quota: O(Type.Object({ workspaces: O(Type.Number()), environments: O(Type.Number()), snapshots: O(Type.Number()), diskGb: O(Type.Number()), cpu: O(Type.Number()), memoryGb: O(Type.Number()) }, { description: "kind=quota: the ceilings asked for" })),
      access: O(Type.Object({ team: Type.String(), role: Type.String() }, { description: "kind=access: team and role" })),
      region: O(Type.Object({ region: Type.String() }, { description: "kind=region: the region asked for" })),
      other: O(Type.Object({ title: Type.String(), body: Type.String() }, { description: "kind=other: what is being asked" })),
    },
    (a) => answer("POST", "/v1/requests", { kind: a.kind, reason: a.reason, owner: a.owner, quota: a.quota, access: a.access, region: a.region, other: a.other }),
  );
  // Claims only, unverified: the api is what verifies; the token itself never reaches the model.
  reg("kl_whoami", {}, async () => {
    const claims = JSON.parse(Buffer.from(token().token.split(".")[1] ?? "", "base64url").toString() || "{}") as { sub?: string; team?: string; exp?: number };
    return text({ username: claims.sub, team: claims.team, expires_at: claims.exp ? new Date(claims.exp * 1000).toISOString() : undefined });
  });
  progressTool(reg);
  capabilities(reg);
}

/**
 * Two modes, one file. A WORKSPACE session already has hands on its own files
 * (`workspace-tools.ts`); all it gains here is the machine's own packages, so
 * "install ripgrep" inside a workspace is that workspace's, not a platform call
 * about somebody else. A BENCH session gets the platform, and asks.
 */
export default function (pi: ExtensionAPI) {
  // A `btw` fork runs `--no-tools` over a copy of a session's transcript. It registers nothing —
  // it is loaded ONLY so it is told what it is, like every other session (owner, 2026-09-17).
  if (process.env.KL_FORK === "1") return tellItWhereItStands(pi, "You answer one question about this bench's work, from the transcript you were forked from. You have no tools: you can change nothing, and you cannot look anything up — answer from what is in front of you, or say it is not there.");
  // The mode is which machine's session this is: a workspace names it, the bench is its own
  // (`KL_WORKSPACE_ID`, whose tool server `workspace-tools.ts` is pointed at by address).
  const inWorkspace = process.env.KL_TOOLS_WORKSPACE;
  if (inWorkspace) {
    const reg = makeReg(pi);
    ownTools(pi, inWorkspace, process.env.KL_TEAM, reg);
    environmentTools(reg);
    progressTool(reg);
    return capabilities(reg);
  }
  tools(pi);
  const own = process.env.KL_WORKSPACE_ID;
  if (own) ownTools(pi, own, process.env.KL_TEAM);
  tellItWhereItStands(pi, BENCH_HANDS);
}
