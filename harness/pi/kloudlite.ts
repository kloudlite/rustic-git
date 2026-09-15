import fs from "node:fs";
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
const q = (o: Record<string, string | undefined>) => {
  const s = new URLSearchParams(Object.entries(o).filter(([, v]) => v) as [string, string][]).toString();
  return s ? `?${s}` : "";
};

export default function (pi: ExtensionAPI) {
  const spec = (name: string) => TOOLS.find((t) => t.name === name)!;
  const reg = <P extends Parameters<typeof Type.Object>[0]>(name: string, params: P, run: (a: Record<string, any>) => Promise<{ content: { type: "text"; text: string }[]; isError?: boolean }>) => {
    const s = spec(name);
    pi.registerTool({
      name,
      label: name,
      description: `${s.summary} [${s.effect}]`,
      parameters: Type.Object(params),
      async execute(toolCallId, a, _signal, _update, ctx) {
        const args = a as Record<string, any>;
        // A call that hands work to a workspace or environment is an exchange:
        // harness-bench records it in the bench's one log, where the session's
        // queue and the workspace's queue both read it.
        const target = /^kl_(workspace|environment)_/.test(name) ? String(args.id ?? args.name ?? "") : "";
        const publish = (v: unknown) => target && ctx?.ui?.setWidget("harness:exchange", [JSON.stringify(v)]);
        const id = `x-${toolCallId}`;
        publish({ id, workspace: target, dir: "out", text: `${name} ${JSON.stringify(args)}`, state: "sent" });
        const r = await run(args);
        publish({ id: `${id}-in`, workspace: target, dir: "in", text: r.content.map((c) => c.text).join("").slice(0, 2000), state: r.isError ? "failed" : "done", ref: id });
        publish({ id, state: r.isError ? "failed" : "done" });
        return r;
      },
    });
  };
  const S = (d: string) => Type.String({ description: d });
  const O = (t: ReturnType<typeof Type.String>) => Type.Optional(t);

  // workspaces
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
  reg("kl_workspace_packages", { id: S("workspace id"), packages: Type.Array(Type.String(), { description: "the whole list: attr or attr@version" }) }, (a) => answer("PATCH", `/v1/workspaces/${a.id}`, { packages: a.packages }));
  reg("kl_workspace_delete", { id: S("workspace id") }, (a) => answer("DELETE", `/v1/workspaces/${a.id}`));

  // environments
  // A person's space (one per team, plus personal = their handle) follows one environment; every
  // workspace and the bench in it resolve its services by bare name.
  reg("kl_my_environment", {}, () => answer("GET", "/v1/me/environments"));
  reg("kl_my_environment_set", { team: S("team slug, or your handle for your personal space"), environment: S("environment id owned by that team") }, (a) => answer("PUT", `/v1/me/environments/${a.team}`, { environment: a.environment }));
  reg("kl_my_environment_clear", { team: S("team slug, or your handle for your personal space") }, (a) => answer("DELETE", `/v1/me/environments/${a.team}`));
  reg("kl_environments", { owner: O(S("owner slug to list for")) }, (a) => answer("GET", `/v1/environments${q({ owner: a.owner })}`));
  reg("kl_environment", { id: S("environment id") }, (a) => answer("GET", `/v1/environments/${a.id}`));
  reg(
    "kl_environment_create",
    {
      name: S("environment name"),
      region: S("region id"),
      owner: O(S("team slug; absent = personal")),
      services: Type.Array(Type.Object({ name: Type.String(), image: Type.String(), ports: O(Type.Array(Type.Number())) }), { description: "services to run" }),
    },
    (a) => answer("POST", "/v1/environments", { name: a.name, region: a.region, owner: a.owner, services: a.services }),
  );
  reg("kl_environment_start", { id: S("environment id") }, (a) => answer("POST", `/v1/environments/${a.id}/start`));
  reg("kl_environment_stop", { id: S("environment id") }, (a) => answer("POST", `/v1/environments/${a.id}/stop`));
  reg("kl_environment_push", { id: S("environment id"), message: O(S("what this snapshot is")) }, (a) => answer("POST", `/v1/environments/${a.id}/push`, { message: a.message }));
  reg("kl_environment_clone", { id: S("source environment id"), name: S("name for the clone") }, (a) => answer("POST", `/v1/environments/${a.id}/clone`, { name: a.name }));
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
  reg("kl_environment_delete", { id: S("environment id") }, (a) => answer("DELETE", `/v1/environments/${a.id}`));

  // platform
  reg("kl_regions", {}, () => answer("GET", "/v1/regions"));
  reg("kl_quota", {}, () => answer("GET", "/v1/quota"));
  reg("kl_volumes", { name: O(S("a volume name, for its history")) }, (a) => answer("GET", a.name ? `/v1/volumes/${a.name}/history` : "/v1/volumes"));
  reg("kl_builder", {}, () => answer("GET", "/v1/builders/me"));
  // Claims only, unverified: the api is what verifies; the token itself never reaches the model.
  reg("kl_whoami", {}, async () => {
    const claims = JSON.parse(Buffer.from(token().token.split(".")[1] ?? "", "base64url").toString() || "{}") as { sub?: string; team?: string; exp?: number };
    return text({ username: claims.sub, team: claims.team, expires_at: claims.exp ? new Date(claims.exp * 1000).toISOString() : undefined });
  });
}
