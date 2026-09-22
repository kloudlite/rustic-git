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

export class Platform {
  private api: string;
  private token: string;
  private team?: string;

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

  async tools(ws: string) { return ((await this.call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools`)) as { address: string }).address; }
  async name(ws: string) { return ((await this.call("GET", `/v1/workspaces/${encodeURIComponent(ws)}`)) as { name: string }).name; }
  async clone(ws: string, name: string) { return ((await this.call("POST", `/v1/workspaces/${encodeURIComponent(ws)}/clone`, { name })) as { id: string }).id; }
  async remove(ws: string) { await this.call("DELETE", `/v1/workspaces/${encodeURIComponent(ws)}`); }
}
