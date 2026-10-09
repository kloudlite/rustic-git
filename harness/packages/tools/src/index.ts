/**
 * A tool the agent can call. There are no built-in tools — the harness ships
 * an empty registry and you register whatever set you want. Provider-agnostic:
 * `inputSchema` is plain JSON Schema.
 */
export interface ToolDef<I = any> {
  name: string;
  description: string;
  /** JSON Schema for the input. Be prescriptive about *when* to call it. */
  inputSchema: Record<string, unknown>;
  run(input: I): Promise<string>;
}

/** Tools registered for a session. Empty by default. */
export class Registry {
  #tools = new Map<string, ToolDef<any>>();

  add(...defs: ToolDef<any>[]): this {
    for (const def of defs) {
      if (this.#tools.has(def.name)) {
        throw new Error(`duplicate tool: ${def.name}`);
      }
      this.#tools.set(def.name, def);
    }
    return this;
  }

  names(): string[] {
    return [...this.#tools.keys()];
  }

  all(): ToolDef<any>[] {
    return [...this.#tools.values()];
  }

  get(name: string): ToolDef<any> {
    const def = this.#tools.get(name);
    if (!def) throw new Error(`unknown tool: ${name}`);
    return def;
  }
}
export { webFetch, webSearch } from "./web.ts";
export { api, apiJson, platformTools } from "./platform.ts";
export { podTools, podFence, type PodFence, podExec, podGet, podText, podPost, type ExecResult } from "./pod.ts";
export { scratchTools, scratchRoot } from "./scratch.ts";
