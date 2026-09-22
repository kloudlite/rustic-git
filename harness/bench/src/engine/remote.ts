// The engine's `cwd` is a key, not a path: the bench holds no source (ruling 2). Each key maps to
// one workspace's tool server; path confinement is the pod's own `paths::confine`.
export type Backend = (name: string, args: Record<string, unknown>) => Promise<unknown>;
const backends = new Map<string, Backend>();
export const setBackend = (cwd: string, b: Backend | undefined) => { b ? backends.set(cwd, b) : backends.delete(cwd); };

export class ToolError extends Error { status: number; constructor(status: number, message: string) { super(message); this.status = status; } }

export const httpBackend = (address: string): Backend => async (name, args) => {
  const r = await fetch(`http://${address}/tools/${name}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(args), signal: AbortSignal.timeout(600_000) });
  const v = (await r.json().catch(() => ({ error: `bad json from ${name}` }))) as { error?: string };
  if (!r.ok) throw new ToolError(r.status, v.error ?? `${name}: ${r.status}`);
  return v;
};

export async function remote(cwd: string, name: string, args: Record<string, unknown>) {
  const b = backends.get(cwd);
  if (!b) throw new Error(`no tool backend for ${cwd}`);
  return b(name, args);
}

// A tool result is always a string to the model; a failed call is a result too, never an exception (tools.ts's own rule).
export const text = (v: unknown): string => {
  if (typeof v === "string") return v;
  const o = v as Record<string, unknown>;
  if (o && typeof o.stdout === "string") return `${o.stdout}${o.stderr ? `\n${o.stderr}` : ""}${o.exit_code ? `\nexit ${o.exit_code}` : ""}`;
  if (o && typeof o.content === "string") return o.content;
  return JSON.stringify(v);
};
export const tryRemote = (cwd: string, name: string, args: Record<string, unknown>) => remote(cwd, name, args).catch((e) => `error: ${(e as Error).message}`);
