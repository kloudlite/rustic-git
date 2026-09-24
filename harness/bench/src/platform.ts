// The bench's only /v1 client. Ruling 8 names an operation executor as the mutation layer; none
// exists in the tree yet, so every clone and delete funnels through here and a later executor
// replaces this file, not its callers.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export class PlatformError extends Error {
  status: number;
  body: string;
  constructor(status: number, body: string) {
    super(`platform ${status}: ${body.slice(0, 200)}`);
    this.status = status;
    this.body = body;
  }
}

const ADDR_RE = /^([A-Za-z0-9.-]+:\d{1,5}|\[[0-9a-fA-F:]+\]:\d{1,5})$/;
function validAddress(addr: string, source: string): string {
  if (!ADDR_RE.test(addr)) throw new Error(`${source} is not a host:port address: ${addr}`);
  return addr;
}

export type ToolsAuth = { address: string; token?: string };

export class Platform {
  private api: string;
  private token: string;
  private team?: string;
  private toolsCache = new Map<string, ToolsAuth>();

  constructor(api: string, token: string, team?: string) {
    this.api = api;
    this.token = token;
    this.team = team;
  }

  static fromEnv(): Platform {
    const dir = process.env.KL_CONFIG_DIR ?? path.join(os.homedir(), ".config", "kl-connect");
    const c = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8")) as { api?: string; token: string };
    return new Platform(c.api ?? "https://dev.kloudlite.io", c.token, process.env.KL_TEAM);
  }

  private async call(method: string, p: string, body?: unknown): Promise<unknown> {
    const url = `${this.api}${p}${this.team ? `?team=${encodeURIComponent(this.team)}` : ""}`;
    const r = await fetch(url, { method, headers: { authorization: `Bearer ${this.token}`, "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    if (!r.ok) throw new PlatformError(r.status, text);
    if (text.trimStart().startsWith("<")) throw new PlatformError(r.status, "html page, not the api"); // a login redirect answers 200 with HTML
    return text ? JSON.parse(text) : undefined;
  }

  /** Status-carrying variant for callers (the operations executor's adapters) that need the code, not just a thrown error. */
  async raw(method: string, p: string, body?: unknown): Promise<{ status: number; data: unknown }> {
    try {
      return { status: 200, data: await this.call(method, p, body) };
    } catch (e) {
      if (e instanceof PlatformError) return { status: e.status, data: e.body };
      throw e;
    }
  }

  /**
   * `{address, token}` for a workspace, cached. `fresh` re-asks `/v1` — a 401 at the tool server
   * means the keys beat re-minted the token, answered by resolving once more, not by failing the call.
   */
  async tools(ws: string, fresh = false): Promise<ToolsAuth> {
    if (!fresh) {
      const had = this.toolsCache.get(ws);
      if (had) return had;
    }
    // A laptop points every workspace session at one tool server, such as the local end of
    // `kl-connect ws ide`; it carries its own token in the env when it needs one.
    let at: ToolsAuth;
    if (process.env.KL_TOOLS_ADDRESS) {
      at = { address: validAddress(process.env.KL_TOOLS_ADDRESS, "KL_TOOLS_ADDRESS"), ...(process.env.KL_TOOLS_TOKEN ? { token: process.env.KL_TOOLS_TOKEN } : {}) };
    } else {
      const d = (await this.call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools`)) as { address: string; token?: string };
      at = { address: validAddress(d.address, "workspace address"), ...(typeof d.token === "string" && d.token ? { token: d.token } : {}) };
    }
    this.toolsCache.set(ws, at);
    return at;
  }
  async name(ws: string) { return ((await this.call("GET", `/v1/workspaces/${encodeURIComponent(ws)}`)) as { name: string }).name; }
  async clone(ws: string, name: string) { return ((await this.call("POST", `/v1/workspaces/${encodeURIComponent(ws)}/clone`, { name })) as { id: string }).id; }
  async remove(ws: string) { await this.call("DELETE", `/v1/workspaces/${encodeURIComponent(ws)}`); }
}
