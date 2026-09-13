import { Type } from "typebox";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { call } from "./kloudlite.ts";

/**
 * A workspace session's hands. pi runs in the bench pod; these seven tools run
 * in the workspace, as calls to its tool server (`kl ide serve`, crates/ide).
 * The names and parameters are pi's own, so the model sees the tools it knows;
 * the work is the tool server's. The address comes only from /v1, which answers
 * the workspace's owner and nobody else, and is asked again after a connection
 * error because a restarted pod has a new IP. Nothing here is a bench tool:
 * a workspace session starts with `--tools` naming exactly these.
 */
export const WORKSPACE_TOOLS = "read,write,edit,bash,grep,find,ls";
const MAX_EXEC_MS = 600_000;
const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

export type IdeCall = { tool: string; args: Record<string, unknown> };
type Result = { content: { type: "text"; text: string }[]; isError: boolean };
const text = (t: string, isError = false): Result => ({ content: [{ type: "text", text: t || "(no output)" }], isError });

export function toIde(name: string, p: Record<string, any>): IdeCall {
  switch (name) {
    case "read":
      return { tool: "read", args: { path: p.path, offset: p.offset, limit: p.limit } };
    case "write":
      return { tool: "write", args: { path: p.path, content: p.content } };
    case "edit":
      return { tool: "edit", args: { path: p.path, edits: (p.edits ?? []).map((e: { oldText: string; newText: string }) => ({ old: e.oldText, new: e.newText })) } };
    case "bash":
      return { tool: "exec", args: { cmd: p.command, timeout_ms: Math.min(MAX_EXEC_MS, (p.timeout ?? 120) * 1000) } };
    case "grep":
      return { tool: "grep", args: { pattern: p.literal ? escapeRe(p.pattern) : p.pattern, cwd: p.path, glob: p.glob, ignore_case: p.ignoreCase, context: p.context, max: p.limit } };
    case "find":
      return { tool: "glob", args: { pattern: p.pattern, cwd: p.path } };
    case "ls":
      return { tool: "exec", args: { cmd: ["ls", "-1Ap", p.path ?? "."], head: p.limit ?? 500 } };
    default:
      throw new Error(`no workspace tool ${name}`);
  }
}

export function fromIde(name: string, status: number, body: any, limit?: number): Result {
  // The tool server's own refusal is the whole answer: never the body around it.
  if (status >= 400) return text(String(body?.error ?? `the tool server answered ${status}`), true);
  switch (name) {
    case "read":
      return body.binary ? text(`${body.path}: binary, ${body.size} bytes`) : text(body.content + (body.truncated ? `\n[${body.total_lines} lines in all; page with offset]` : ""));
    case "write":
      return text(`wrote ${body.bytes} bytes to ${body.path}`);
    case "edit":
      return text(`applied ${body.applied} edit(s) to ${body.path}`);
    case "bash":
    case "ls": {
      const out = [body.stdout, body.stderr].filter(Boolean).join("\n").trim();
      if (body.timed_out) return text(`${out}\n[timed out]`.trim(), true);
      return body.exit_code === 0 ? text(out) : text(`${out}\n[exit ${body.exit_code}]`.trim(), true);
    }
    case "grep":
      return text((body.matches ?? []).map((m: { path: string; line: number; text: string }) => `${m.path}:${m.line}: ${m.text}`).join("\n") + (body.truncated ? "\n[truncated]" : ""));
    case "find":
      return text((body.paths ?? []).slice(0, limit ?? 1000).join("\n"));
    default:
      return text(JSON.stringify(body));
  }
}

export class ToolServer {
  private workspace: string;
  private resolve: (ws: string) => Promise<string>;
  private address?: string;
  constructor(workspace: string, resolve: (ws: string) => Promise<string>) {
    this.workspace = workspace;
    this.resolve = resolve;
  }
  async call(c: IdeCall, signal?: AbortSignal): Promise<{ status: number; body: any }> {
    for (let attempt = 0; ; attempt++) {
      this.address ??= await this.resolve(this.workspace);
      const at = this.address;
      try {
        const r = await fetch(`http://${at}/tools/${c.tool}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(c.args), signal });
        return { status: r.status, body: await r.json().catch(() => ({ error: `the tool server answered ${r.status} without JSON` })) };
      } catch (e) {
        if (signal?.aborted) throw e;
        this.address = undefined;
        if (attempt > 0) throw new Error(`workspace ${this.workspace} did not answer at ${at}: ${(e as Error).message}`);
      }
    }
  }
}

export async function resolveFromApi(ws: string): Promise<string> {
  // A laptop points every workspace session at one tool server, such as the local end of `kl-connect ws ide`.
  if (process.env.KL_TOOLS_ADDRESS) return process.env.KL_TOOLS_ADDRESS;
  const team = process.env.KL_TEAM ? `?team=${encodeURIComponent(process.env.KL_TEAM)}` : "";
  const r = await call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools${team}`);
  const d = r.data as { address?: string; error?: string } | string | null;
  if (r.status === 200 && d && typeof d === "object" && d.address) return d.address;
  throw new Error(d && typeof d === "object" && d.error ? d.error : `workspace ${ws}: ${typeof d === "string" ? d : r.status}`);
}

export default function (pi: ExtensionAPI) {
  const ws = process.env.KL_TOOLS_WORKSPACE;
  if (!ws) return;
  const server = new ToolServer(ws, resolveFromApi);
  const reg = (name: string, label: string, description: string, parameters: ReturnType<typeof Type.Object>) =>
    pi.registerTool({
      name,
      label,
      description: `${description} Runs in workspace ${ws}.`,
      parameters,
      async execute(_toolCallId, params, signal) {
        const p = params as Record<string, any>;
        try {
          const r = await server.call(toIde(name, p), signal);
          return fromIde(name, r.status, r.body, p.limit);
        } catch (e) {
          return text((e as Error).message, true);
        }
      },
    });
  reg("read", "Read", "Read a text file with line numbers. offset (1-based line) and limit page it.", Type.Object({ path: Type.String(), offset: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("write", "Write", "Create or overwrite a file; parent directories are created.", Type.Object({ path: Type.String(), content: Type.String() }));
  reg("edit", "Edit", "Exact replacements in one file, all or nothing. Each oldText must occur exactly once.", Type.Object({ path: Type.String(), edits: Type.Array(Type.Object({ oldText: Type.String(), newText: Type.String() })) }));
  reg("bash", "Bash", "Run a shell command in the workspace dir and return its output.", Type.Object({ command: Type.String(), timeout: Type.Optional(Type.Number({ description: "Seconds, at most 600" })) }));
  reg("grep", "Grep", "Regex search, gitignore-aware. path is a directory.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), glob: Type.Optional(Type.String()), ignoreCase: Type.Optional(Type.Boolean()), literal: Type.Optional(Type.Boolean()), context: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("find", "Find", "Files matching a glob, gitignore-aware, newest first.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
  reg("ls", "List", "List a directory; directories end in /.", Type.Object({ path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
}
