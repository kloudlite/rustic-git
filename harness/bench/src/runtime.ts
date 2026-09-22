// Production seam between a Session and the engine: one runTask per turn, the log rows as context.
import { runTask, makeAiSdkLlm, ask } from "./engine/index.ts";
import type { Turn } from "./session.ts";

export function makeTurn(env = process.env): Turn {
  for (const k of ["TYPESAFE_API_KEY", "JEVHARN_API_KEY"]) if (!env[k]) throw new Error(`${k} is not set; the bench cannot run an engine`);
  const llm = makeAiSdkLlm();
  return (ctx) => {
    const history = ctx.history.filter((r) => r.kind === "user" || r.kind === "turn.end").slice(-10).map((r) => (r.kind === "user" ? `user: ${r.text}` : `main: ${r.answer ?? r.error ?? ""}`)).join("\n");
    return runTask({ llm, ask, cwd: ctx.cwd, log: ctx.log, user: ctx.user, context: () => history, steer: () => ({ lines: [], stop: ctx.signal.aborted }), tools: ctx.tools, readOnly: ctx.readOnly }, ctx.prompt, ctx.prompt.slice(0, 80));
  };
}
