// The engine's `cwd` is a key, not a path: the bench holds no source (ruling 2). Each key maps to
// one workspace's tool server; path confinement is the pod's own `paths::confine`.
export type Backend = (name: string, args: Record<string, unknown>) => Promise<unknown>;
const backends = new Map<string, Backend>();
export const setBackend = (cwd: string, b: Backend | undefined) => { b ? backends.set(cwd, b) : backends.delete(cwd); };

export class ToolError extends Error { status: number; constructor(status: number, message: string) { super(message); this.status = status; } }

export type ToolsAuth = { address: string; token?: string };

/**
 * `resolve(fresh)` gets the address+token pair, cached by the caller; a 401 means the keys beat
 * re-minted the token since the last resolve, so it is answered by resolving once more (`fresh`)
 * and retrying, not by failing the call.
 */
export const httpBackend = (resolve: (fresh?: boolean) => Promise<ToolsAuth>): Backend => async (name, args) => {
  const call = async (at: ToolsAuth) => {
    const r = await fetch(`http://${at.address}/tools/${name}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(at.token ? { authorization: `Bearer ${at.token}` } : {}) },
      body: JSON.stringify(args),
      signal: AbortSignal.timeout(600_000),
    });
    const v = (await r.json().catch(() => ({ error: `bad json from ${name}` }))) as { error?: string };
    return { r, v };
  };
  let at = await resolve();
  let { r, v } = await call(at);
  if (r.status === 401) {
    at = await resolve(true);
    ({ r, v } = await call(at));
  }
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
