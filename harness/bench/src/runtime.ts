// Production seam between a Session and the engine: one runTask per turn, the log rows as context.
import { runTask, makeAiSdkLlm, modelFrom, reachable, ask, type Llm } from "./engine/index.ts";
import type { Turn } from "./session.ts";

export function makeTurn(env = process.env): Turn {
  // The model's own key is checked per model (reachable), since each session may pick a different one.
  for (const k of ["TYPESAFE_API_KEY"]) if (!env[k]) throw new Error(`${k} is not set; the bench cannot run an engine`);
  // One Llm per model spec, built on first use: sessions on the same model share it, a session that
  // switches models gets a fresh one next turn.
  const llms = new Map<string, Llm>();
  const llmFor = (spec: string): Llm => {
    let llm = llms.get(spec);
    if (!llm) {
      const why = reachable(spec, env);
      if (why) throw new Error(why);
      llm = makeAiSdkLlm(() => modelFrom(spec, env));
      llms.set(spec, llm);
    }
    return llm;
  };
  // async so an unreachable model rejects the turn (an error row) instead of throwing into the scheduler
  return async (ctx) => {
    const llm = llmFor(ctx.model);
    const history = ctx.history.filter((r) => r.kind === "user" || r.kind === "turn.end").slice(-10).map((r) => (r.kind === "user" ? `user: ${r.text}` : `main: ${r.answer ?? r.error ?? ""}`)).join("\n");
    return runTask({ llm, ask, cwd: ctx.cwd, log: ctx.log, user: ctx.user, context: () => history, steer: () => ({ lines: [], stop: ctx.signal.aborted }), tools: ctx.tools, readOnly: ctx.readOnly }, ctx.prompt, ctx.prompt.slice(0, 80));
  };
}
